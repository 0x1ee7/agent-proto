//! Artifact store (RFC-0010 section 4.7): content-addressed blobs in the tenant's
//! object-store bucket. The object name is the lowercase SHA-256 hex of the content
//! (0010-037); a read verifies the content hash and a mismatch is a hard error.

use a2a_types::ids::TenantId;
use async_nats::jetstream::object_store::{GetErrorKind, ObjectStore};
use bus_api::error::BusError;
use bus_api::value::Digest;
use bytes::Bytes;
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncReadExt;

use crate::connection::NatsBus;
use crate::errors::{self, reason};
use crate::layout::{BLOB_MAX_BYTES, artifact_bucket, object_stream};

/// SHA-256 digest of `bytes` in the bus-api form.
fn digest_of(bytes: &[u8]) -> Digest {
    Digest::from_bytes(Sha256::digest(bytes).into())
}

/// The tenant's artifact object store.
async fn store(bus: &NatsBus) -> Result<ObjectStore, BusError> {
    bus.js()
        .get_object_store(artifact_bucket(bus.tenant()))
        .await
        .map_err(|_| errors::unavailable(reason::STREAM_NOT_FOUND))
}

/// `blob_put`: stores `bytes` under their SHA-256 digest and returns the digest.
pub(crate) async fn put(
    bus: &NatsBus,
    tenant: &TenantId,
    bytes: Bytes,
) -> Result<Digest, BusError> {
    bus.check_tenant(tenant)?;
    if bytes.len() > BLOB_MAX_BYTES {
        return Err(BusError::PayloadTooLarge {
            limit: BLOB_MAX_BYTES,
        });
    }
    let digest = digest_of(&bytes);
    let store = store(bus).await?;
    admit(bus, bytes.len()).await?;
    let mut data: &[u8] = &bytes;
    store
        .put(digest.to_string().as_str(), &mut data)
        .await
        .map_err(|_| errors::unavailable(reason::JETSTREAM_API))?;
    Ok(digest)
}

/// Refuses a blob that would take the artifact stream past its byte limit, so nothing
/// is stored. The stream's `max_bytes` bounds the tenant's blob quota; the refusal is
/// `Overloaded` with the bus's retry hint.
async fn admit(bus: &NatsBus, incoming: usize) -> Result<(), BusError> {
    let mut stream = bus
        .js()
        .get_stream(object_stream(&artifact_bucket(bus.tenant())))
        .await
        .map_err(|_| errors::unavailable(reason::STREAM_NOT_FOUND))?;
    let info = stream
        .info()
        .await
        .map_err(|_| errors::unavailable(reason::JETSTREAM_API))?;
    let limit = u64::try_from(info.config.max_bytes).unwrap_or(u64::MAX);
    let incoming = u64::try_from(incoming).unwrap_or(u64::MAX);
    if info.state.bytes.saturating_add(incoming) > limit {
        return Err(BusError::Overloaded {
            retry_after: bus.overload_retry_after(),
        });
    }
    Ok(())
}

/// `blob_get`: the bytes stored under `digest`. A missing object is `Absent`; content
/// whose hash differs from `digest` is `Corrupt` and is never retried.
pub(crate) async fn get(
    bus: &NatsBus,
    tenant: &TenantId,
    digest: &Digest,
) -> Result<Bytes, BusError> {
    bus.check_tenant(tenant)?;
    let store = store(bus).await?;
    let mut object = match store.get(digest.to_string().as_str()).await {
        Ok(object) => object,
        Err(err) if matches!(err.kind(), GetErrorKind::NotFound) => {
            return Err(BusError::Absent);
        }
        Err(_) => return Err(errors::unavailable(reason::JETSTREAM_API)),
    };
    let size = object.info().size;
    if size > BLOB_MAX_BYTES {
        return Err(BusError::Corrupt { digest: *digest });
    }
    let mut buffer = Vec::with_capacity(size);
    object
        .read_to_end(&mut buffer)
        .await
        .map_err(|_| errors::unavailable(reason::JETSTREAM_API))?;
    if digest_of(&buffer) != *digest {
        return Err(BusError::Corrupt { digest: *digest });
    }
    Ok(Bytes::from(buffer))
}
