//! Tenancy rows: principal presence, permission per subject and stream, and
//! tenant isolation including denied-versus-absent indistinguishability.

use std::time::Duration;

use bus_api::bus::Bus;
use bus_api::error::BusError;
use bus_api::headers::Headers;
use bus_api::message::StartPosition;
use bus_api::name::{Bucket, ConsumerName, Key, KeyPrefix, MsgId, Principal};
use bus_api::value::Digest;
use bytes::Bytes;

use super::support::put;
use super::{BusFactory, Ctx, bx, expect_denied, pat, settle, subj};

fn key(text: &str) -> Result<Key, String> {
    Key::parse(text).map_err(|e| format!("key {text}: {e}"))
}

fn denied<T>(what: &str, result: Result<T, BusError>) -> Result<(), String> {
    expect_denied(what, result.map(|_| ()))
}

fn error_of<T>(what: &str, result: Result<T, BusError>) -> Result<BusError, String> {
    match result {
        Err(e) => Ok(e),
        Ok(_) => Err(format!("{what}: operation succeeded")),
    }
}

/// [0011-040] every operation carries a principal; anonymous is rejected on
/// every operation family.
pub(super) fn r0011_040<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    let fx = ctx.fx;
    let bus = ctx.bus;
    let anon = Principal::anonymous();
    let echo = pat("rpc.echo")?;
    let filter = pat("suite.>")?;
    let consumer = ConsumerName::parse("suite-c040").map_err(|e| format!("consumer: {e}"))?;
    let id = MsgId::parse("suite-msg-040").map_err(|e| format!("msg id: {e}"))?;
    settle(ctx.control, async {
        denied(
            "publish anonymous",
            bus.publish(&anon, &subj("suite.x")?, &Headers::new(), Bytes::new())
                .await,
        )?;
        denied(
            "request anonymous",
            bus.request(
                &anon,
                &subj("rpc.echo")?,
                &Headers::new(),
                Bytes::new(),
                Duration::from_secs(1),
            )
            .await,
        )?;
        denied("serve anonymous", bus.serve(&anon, &echo).await)?;
        denied(
            "append anonymous",
            bus.append(
                &anon,
                &fx.stream,
                &subj("suite.x")?,
                &Headers::new(),
                Bytes::new(),
                &id,
            )
            .await,
        )?;
        denied(
            "consume anonymous",
            bus.consume(
                &anon,
                &fx.stream,
                &consumer,
                &filter,
                StartPosition::FromFirst,
            )
            .await,
        )?;
        denied(
            "kv_get anonymous",
            bus.kv_get(&anon, &fx.bucket_local, &key("k40")?).await,
        )?;
        denied(
            "blob_put anonymous",
            bus.blob_put(&anon, &fx.local, Bytes::from_static(b"x"))
                .await,
        )?;
        Ok::<(), String>(())
    })
}

/// [0011-041] publish and consume are permitted per subject and stream; a
/// granted subject still succeeds.
pub(super) fn r0011_041<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    let fx = ctx.fx;
    let bus = ctx.bus;
    let filter = pat("suite.>")?;
    let consumer = ConsumerName::parse("suite-c041").map_err(|e| format!("consumer: {e}"))?;
    settle(ctx.control, async {
        denied(
            "publish by a principal without a publish grant",
            bus.publish(
                &fx.server,
                &subj("suite.x41")?,
                &Headers::new(),
                Bytes::new(),
            )
            .await,
        )?;
        denied(
            "publish outside the granted subjects",
            bus.publish(
                &fx.client,
                &subj("forbidden.x41")?,
                &Headers::new(),
                Bytes::new(),
            )
            .await,
        )?;
        denied(
            "consume by a principal without stream grant",
            bus.consume(
                &fx.outsider,
                &fx.stream,
                &consumer,
                &filter,
                StartPosition::FromFirst,
            )
            .await,
        )?;
        denied(
            "consume by an ungranted principal",
            bus.consume(
                &fx.denied,
                &fx.stream,
                &consumer,
                &filter,
                StartPosition::FromFirst,
            )
            .await,
        )?;
        put(bus, &fx.client, "suite.x41", b"granted").await?;
        Ok::<(), String>(())
    })
}

/// [0011-042] tenant isolation is absolute: KV reads, KV watch, and blob
/// digests of another tenant are denied.
pub(super) fn r0011_042<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    let fx = ctx.fx;
    let bus = ctx.bus;
    let prefix = KeyPrefix::parse("k42").map_err(|e| format!("prefix: {e}"))?;
    settle(ctx.control, async {
        let digest = bx(
            "blob_put",
            bus.blob_put(&fx.client, &fx.local, Bytes::from_static(b"iso42"))
                .await,
        )?;
        bx(
            "kv_put",
            bus.kv_put(
                &fx.client,
                &fx.bucket_local,
                &key("k42")?,
                Bytes::from_static(b"v"),
                None,
            )
            .await,
        )?;
        denied(
            "kv_get by outsider on the local bucket",
            bus.kv_get(&fx.outsider, &fx.bucket_local, &key("k42")?)
                .await,
        )?;
        denied(
            "kv_watch by outsider on the local bucket",
            bus.kv_watch(&fx.outsider, &fx.bucket_local, &prefix).await,
        )?;
        denied(
            "blob_get by outsider on the local tenant",
            bus.blob_get(&fx.outsider, &fx.local, &digest).await,
        )?;
        denied(
            "blob_get by client naming another tenant",
            bus.blob_get(&fx.client, &fx.other, &digest).await,
        )?;
        Ok::<(), String>(())
    })
}

/// [0011-044] denied and absent give the same `PermissionDenied`, so an
/// outsider cannot tell a stored digest from an unknown one, or a foreign
/// bucket from a missing one.
pub(super) fn r0011_044<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    let fx = ctx.fx;
    let bus = ctx.bus;
    let absent = Digest::from_bytes([0xAB; 32]);
    let missing = Bucket::new(&fx.local, "no-such44").map_err(|e| format!("bucket: {e}"))?;
    settle(ctx.control, async {
        let digest = bx(
            "blob_put",
            bus.blob_put(&fx.client, &fx.local, Bytes::from_static(b"iso44"))
                .await,
        )?;
        let on_existing = error_of(
            "blob_get existing digest by outsider",
            bus.blob_get(&fx.outsider, &fx.local, &digest).await,
        )?;
        let on_absent = error_of(
            "blob_get absent digest by outsider",
            bus.blob_get(&fx.outsider, &fx.local, &absent).await,
        )?;
        let on_foreign = error_of(
            "kv_get on another tenant's bucket",
            bus.kv_get(&fx.client, &fx.bucket_other, &key("k44")?).await,
        )?;
        let on_missing = error_of(
            "kv_get on a missing bucket",
            bus.kv_get(&fx.client, &missing, &key("k44")?).await,
        )?;
        for (what, found) in [
            ("existing digest", on_existing),
            ("absent digest", on_absent),
            ("foreign bucket", on_foreign),
            ("missing bucket", on_missing),
        ] {
            if found != BusError::PermissionDenied {
                return Err(format!("{what}: expected PermissionDenied, got {found}"));
            }
        }
        Ok::<(), String>(())
    })
}
