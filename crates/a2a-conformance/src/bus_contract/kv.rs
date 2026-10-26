//! KV and blob rows: revisions, TTL, watch snapshots, blob integrity, limits.

use std::time::Duration;

use bus_api::bus::{Bus, Watch};
use bus_api::error::BusError;
use bus_api::message::WatchEvent;
use bus_api::name::{Key, KeyPrefix};
use bus_api::value::Revision;
use bytes::Bytes;
use sha2::{Digest as _, Sha256};

use super::{BusFactory, Ctx, bx, settle};

fn key(text: &str) -> Result<Key, String> {
    Key::parse(text).map_err(|e| format!("key {text}: {e}"))
}

fn revision(value: u64) -> Result<Revision, String> {
    Ok(Revision::new(value))
}

/// [0011-016] put/get/create/update(CAS)/delete, and TTL expiry.
pub(super) fn r0011_016<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    let fx = ctx.fx;
    let bus = ctx.bus;
    let bucket = &fx.bucket_local;
    let k = key("k16")?;
    settle(ctx.control, async {
        let r1 = bx(
            "kv_create",
            bus.kv_create(&fx.client, bucket, &k, Bytes::from_static(b"v1"), None)
                .await,
        )?;
        let got = bx("kv_get", bus.kv_get(&fx.client, bucket, &k).await)?;
        if got.value.as_ref() != b"v1" || got.revision.get() != r1.get() {
            return Err(format!(
                "kv_get after create: {:?} rev {}",
                got.value,
                got.revision.get()
            ));
        }
        let r2 = bx(
            "kv_update",
            bus.kv_update(&fx.client, bucket, &k, Bytes::from_static(b"v2"), r1, None)
                .await,
        )?;
        if r2.get() <= r1.get() {
            return Err(format!(
                "update revision {} not above {}",
                r2.get(),
                r1.get()
            ));
        }
        match bus
            .kv_update(
                &fx.client,
                bucket,
                &k,
                Bytes::from_static(b"stale"),
                r1,
                None,
            )
            .await
        {
            Err(BusError::Conflict { .. }) => {}
            other => return Err(format!("stale CAS: expected Conflict, got {other:?}")),
        }
        let got = bx("kv_get", bus.kv_get(&fx.client, bucket, &k).await)?;
        if got.value.as_ref() != b"v2" {
            return Err(format!("stale CAS changed the value to {:?}", got.value));
        }
        bx(
            "kv_delete",
            bus.kv_delete(&fx.client, bucket, &k, Some(r2)).await,
        )?;
        match bus.kv_get(&fx.client, bucket, &k).await {
            Err(BusError::Absent) => Ok::<(), String>(()),
            other => Err(format!("get after delete: expected Absent, got {other:?}")),
        }
    })?;

    let ttl_key = key("t16")?;
    let old = settle(ctx.control, async {
        let rev = bx(
            "kv_put ttl",
            bus.kv_put(
                &fx.client,
                bucket,
                &ttl_key,
                Bytes::from_static(b"short"),
                Some(fx.scaled(Duration::from_secs(2))),
            )
            .await,
        )?;
        let got = bx("kv_get", bus.kv_get(&fx.client, bucket, &ttl_key).await)?;
        if got.value.as_ref() != b"short" {
            return Err(format!("ttl value before expiry: {:?}", got.value));
        }
        Ok::<Revision, String>(rev)
    })?;
    ctx.control.advance(ctx.fx.scaled(Duration::from_secs(3)))?;
    settle(ctx.control, async {
        match bus.kv_get(&fx.client, bucket, &ttl_key).await {
            Err(BusError::Absent) => {}
            other => {
                return Err(format!(
                    "ttl key after expiry: expected Absent, got {other:?}"
                ));
            }
        }
        let again = bx(
            "kv_create after expiry",
            bus.kv_create(
                &fx.client,
                bucket,
                &ttl_key,
                Bytes::from_static(b"again"),
                None,
            )
            .await,
        )?;
        if again.get() <= old.get() {
            return Err(format!(
                "re-create revision {} not above expired revision {}",
                again.get(),
                old.get()
            ));
        }
        Ok::<(), String>(())
    })
}

/// [0011-017] blob digest is SHA-256; corrupted bytes are never returned.
pub(super) fn r0011_017<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    let fx = ctx.fx;
    let bus = ctx.bus;
    let tenant = &fx.local;
    let data = Bytes::from_static(b"blob-017 payload");
    let expected: [u8; 32] = Sha256::digest(&data).into();
    settle(ctx.control, async {
        let digest = bx(
            "blob_put",
            bus.blob_put(&fx.client, tenant, data.clone()).await,
        )?;
        if digest.as_bytes() != &expected {
            return Err("blob digest is not the SHA-256 of the payload".to_owned());
        }
        let back = bx("blob_get", bus.blob_get(&fx.client, tenant, &digest).await)?;
        if back != data {
            return Err("blob_get returned different bytes".to_owned());
        }
        if !ctx.control.corrupt_blob(tenant, &digest)? {
            return Err("control could not corrupt the stored blob".to_owned());
        }
        match bus.blob_get(&fx.client, tenant, &digest).await {
            Err(BusError::Corrupt { digest: found }) if found == digest => Ok::<(), String>(()),
            other => Err(format!("corrupted blob: expected Corrupt, got {other:?}")),
        }
    })
}

/// [0011-026] watch: sorted snapshot, exactly one SnapshotEnd, then changes.
pub(super) fn r0011_026<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    let fx = ctx.fx;
    let bus = ctx.bus;
    let bucket = &fx.bucket_local;
    let prefix = KeyPrefix::parse("wa").map_err(|e| format!("prefix: {e}"))?;
    settle(ctx.control, async {
        bx(
            "kv_put",
            bus.kv_put(
                &fx.client,
                bucket,
                &key("wa2")?,
                Bytes::from_static(b"two"),
                None,
            )
            .await,
        )?;
        bx(
            "kv_put",
            bus.kv_put(
                &fx.client,
                bucket,
                &key("wa1")?,
                Bytes::from_static(b"one"),
                None,
            )
            .await,
        )?;
        bx(
            "kv_put",
            bus.kv_put(
                &fx.client,
                bucket,
                &key("wb1")?,
                Bytes::from_static(b"other"),
                None,
            )
            .await,
        )?;
        let mut watch = bx("kv_watch", bus.kv_watch(&fx.client, bucket, &prefix).await)?;
        let mut snapshot = Vec::new();
        let mut ended = false;
        for _ in 0..16 {
            match bx("watch next", watch.next().await)? {
                WatchEvent::Put(entry) => snapshot.push(entry.key.as_str().to_owned()),
                WatchEvent::SnapshotEnd => {
                    ended = true;
                    break;
                }
                WatchEvent::Delete { .. } => {
                    return Err("delete event inside the snapshot".to_owned());
                }
            }
        }
        if !ended {
            return Err("snapshot never ended".to_owned());
        }
        if snapshot != ["wa1", "wa2"] {
            return Err(format!(
                "snapshot keys {snapshot:?}, expected sorted [wa1, wa2]"
            ));
        }
        let live = bx(
            "kv_put",
            bus.kv_put(
                &fx.client,
                bucket,
                &key("wa3")?,
                Bytes::from_static(b"three"),
                None,
            )
            .await,
        )?;
        match bx("watch next", watch.next().await)? {
            WatchEvent::Put(entry) if entry.key.as_str() == "wa3" && entry.revision == live => {}
            WatchEvent::SnapshotEnd => {
                return Err("second SnapshotEnd after the snapshot".to_owned());
            }
            _ => return Err("live change not delivered after SnapshotEnd".to_owned()),
        }

        let empty = KeyPrefix::parse("zz").map_err(|e| format!("prefix: {e}"))?;
        let mut none = bx("kv_watch", bus.kv_watch(&fx.client, bucket, &empty).await)?;
        match bx("watch next", none.next().await)? {
            WatchEvent::SnapshotEnd => Ok::<(), String>(()),
            _ => Err("empty snapshot did not begin with SnapshotEnd".to_owned()),
        }
    })
}

/// [0011-052] `Conflict` is returned and never resolved by overwrite.
pub(super) fn r0011_052<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    let fx = ctx.fx;
    let bus = ctx.bus;
    let bucket = &fx.bucket_local;
    let k = key("k52")?;
    settle(ctx.control, async {
        let live = bx(
            "kv_create",
            bus.kv_create(&fx.client, bucket, &k, Bytes::from_static(b"a"), None)
                .await,
        )?;
        match bus
            .kv_create(&fx.client, bucket, &k, Bytes::from_static(b"b"), None)
            .await
        {
            Err(BusError::Conflict { current_revision })
                if current_revision.get() == live.get() => {}
            other => {
                return Err(format!(
                    "create on live key: expected Conflict, got {other:?}"
                ));
            }
        }
        let stale = revision(live.get() + 1)?;
        match bus
            .kv_update(
                &fx.client,
                bucket,
                &k,
                Bytes::from_static(b"c"),
                stale,
                None,
            )
            .await
        {
            Err(BusError::Conflict { .. }) => {}
            other => return Err(format!("stale update: expected Conflict, got {other:?}")),
        }
        match bus.kv_delete(&fx.client, bucket, &k, Some(stale)).await {
            Err(BusError::Conflict { .. }) => {}
            other => return Err(format!("stale delete: expected Conflict, got {other:?}")),
        }
        let got = bx("kv_get", bus.kv_get(&fx.client, bucket, &k).await)?;
        if got.value.as_ref() != b"a" || got.revision.get() != live.get() {
            return Err(format!("conflicts changed the entry to {:?}", got.value));
        }
        Ok::<(), String>(())
    })
}

/// [0011-072] KV values and blobs are bounded and the bounds are reported.
pub(super) fn r0011_072<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    let fx = ctx.fx;
    let bus = ctx.bus;
    let kv_max = bus.capabilities().kv_max_value_size;
    let blob_max = bus.capabilities().blob_max_size;
    let bucket = &fx.bucket_local;
    settle(ctx.control, async {
        bx(
            "kv_put at max",
            bus.kv_put(
                &fx.client,
                bucket,
                &key("k72a")?,
                Bytes::from(vec![0_u8; kv_max]),
                None,
            )
            .await,
        )?;
        match bus
            .kv_put(
                &fx.client,
                bucket,
                &key("k72b")?,
                Bytes::from(vec![0_u8; kv_max + 1]),
                None,
            )
            .await
        {
            Err(BusError::PayloadTooLarge { limit }) if limit == kv_max => {}
            other => {
                return Err(format!(
                    "kv value over max: expected PayloadTooLarge{{limit: {kv_max}}}, got {other:?}"
                ));
            }
        }
        match bus
            .blob_put(&fx.client, &fx.local, Bytes::from(vec![0_u8; blob_max + 1]))
            .await
        {
            Err(BusError::PayloadTooLarge { limit }) if limit == blob_max => Ok::<(), String>(()),
            other => Err(format!(
                "blob over max: expected PayloadTooLarge{{limit: {blob_max}}}, got {other:?}"
            )),
        }
    })
}
