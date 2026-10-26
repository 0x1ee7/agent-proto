//! Error rows: no secret leakage, typed taxonomy, duplicate folding, retry safety.

use std::time::Duration;

use bus_api::bus::{Bus, Consumer};
use bus_api::control::{BusControl, Fault};
use bus_api::error::BusError;
use bus_api::headers::Headers;
use bus_api::helpers::fold_duplicate;
use bus_api::message::StartPosition;
use bus_api::name::{Key, MsgId};
use bytes::Bytes;

use super::messaging::stalled;
use super::support::{open, seq_of};
use super::{BusFactory, Ctx, bx, expect_variant, pat, settle, subj};
use crate::driver::{drive, ready_now};

/// Canary placed in payloads and KV values; it must never appear in errors.
const CANARY: &str = "A2A-CANARY-0011-043";

fn key(text: &str) -> Result<Key, String> {
    Key::parse(text).map_err(|e| format!("key {text}: {e}"))
}

fn msg_id(text: &str) -> Result<MsgId, String> {
    MsgId::parse(text).map_err(|e| format!("msg id {text}: {e}"))
}

/// [0011-043] no secrets in errors: a canary in a payload and a KV value
/// never appears in the Debug or Display text of the errors they produce.
pub(super) fn r0011_043<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    let fx = ctx.fx;
    let bus = ctx.bus;
    let texts = settle(ctx.control, async {
        let mut texts = Vec::new();
        let denied = bus
            .publish(
                &fx.denied,
                &subj("suite.canary")?,
                &Headers::new(),
                Bytes::from(format!("{CANARY}-payload")),
            )
            .await;
        match denied {
            Err(e) => texts.push(format!("{e:?} {e}")),
            Ok(_) => return Err("publish by an ungranted principal succeeded".to_owned()),
        }
        bx(
            "kv_put",
            bus.kv_put(
                &fx.client,
                &fx.bucket_local,
                &key("canary43")?,
                Bytes::from(format!("{CANARY}-kv")),
                None,
            )
            .await,
        )?;
        match bus
            .kv_get(&fx.outsider, &fx.bucket_local, &key("canary43")?)
            .await
        {
            Err(e) => texts.push(format!("{e:?} {e}")),
            Ok(_) => return Err("outsider read a local bucket".to_owned()),
        }
        Ok::<Vec<String>, String>(texts)
    })?;
    if texts.is_empty() {
        return Err("no error text captured".to_owned());
    }
    for text in &texts {
        if text.contains(CANARY) {
            return Err(format!("error text leaked the canary: {text}"));
        }
    }
    Ok(())
}

/// [0011-050] every operation returns success or a typed variant; the
/// variants reachable through the suite are exercised here.
pub(super) fn r0011_050<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    let fx = ctx.fx;
    let bus = ctx.bus;
    let control = ctx.control;
    let client = &fx.client;

    settle(control, async {
        expect_variant(
            "publish by ungranted principal",
            bus.publish(&fx.denied, &subj("suite.x")?, &Headers::new(), Bytes::new())
                .await,
            |e| matches!(e, BusError::PermissionDenied),
            "PermissionDenied",
        )
    })?;

    let too_big = bus.capabilities().max_message_size + 1;
    settle(control, async {
        expect_variant(
            "publish over max_message_size",
            bus.publish(
                client,
                &subj("suite.big")?,
                &Headers::new(),
                Bytes::from(vec![0_u8; too_big]),
            )
            .await,
            |e| matches!(e, BusError::PayloadTooLarge { .. }),
            "PayloadTooLarge",
        )
    })?;

    let id = msg_id("suite-msg-050")?;
    let original = settle(control, async {
        let first = bx(
            "append",
            bus.append(
                client,
                &fx.stream,
                &subj("suite.dup50")?,
                &Headers::new(),
                Bytes::from_static(b"d"),
                &id,
            )
            .await,
        )?;
        seq_of(&first)
    })?;
    settle(control, async {
        expect_variant(
            "append with stored msg_id",
            bus.append(
                client,
                &fx.stream,
                &subj("suite.dup50")?,
                &Headers::new(),
                Bytes::from_static(b"d"),
                &id,
            )
            .await,
            |e| matches!(e, BusError::Duplicate { original_seq } if *original_seq == original),
            "Duplicate",
        )
    })?;

    let live = settle(control, async {
        bx(
            "kv_create",
            bus.kv_create(
                client,
                &fx.bucket_local,
                &key("k50")?,
                Bytes::from_static(b"a"),
                None,
            )
            .await,
        )
    })?;
    settle(control, async {
        expect_variant(
            "kv_create on a live key",
            bus.kv_create(
                client,
                &fx.bucket_local,
                &key("k50")?,
                Bytes::from_static(b"b"),
                None,
            )
            .await,
            |e| matches!(e, BusError::Conflict { current_revision } if *current_revision == live),
            "Conflict",
        )
    })?;
    settle(control, async {
        expect_variant(
            "kv_get on a never-written key",
            bus.kv_get(client, &fx.bucket_local, &key("never50")?).await,
            |e| matches!(e, BusError::Absent),
            "Absent",
        )
    })?;

    let (blobless, blobless_control) = bx(
        "create without blob_store",
        ctx.factory.create(fx, ctx.seed, Some("blob_store")),
    )?;
    settle(&blobless_control, async {
        expect_variant(
            "blob_put without blob_store",
            blobless
                .blob_put(client, &fx.local, Bytes::from_static(b"x"))
                .await,
            |e| matches!(e, BusError::Unsupported { .. }),
            "Unsupported",
        )
    })?;

    let chunk = fx.blob_quota / 2;
    settle(control, async {
        bx(
            "blob_put",
            bus.blob_put(client, &fx.local, Bytes::from(vec![1_u8; chunk]))
                .await,
        )?;
        bx(
            "blob_put",
            bus.blob_put(client, &fx.local, Bytes::from(vec![2_u8; chunk]))
                .await,
        )?;
        expect_variant(
            "blob_put over quota",
            bus.blob_put(client, &fx.local, Bytes::from(vec![3_u8; chunk]))
                .await,
            |e| matches!(e, BusError::Overloaded { .. }),
            "Overloaded",
        )
    })?;

    let silent = pat("rpc.silent")?;
    let server = fx.server.clone();
    let _requests = settle(control, async {
        bx("serve", bus.serve(&server, &silent).await)
    })?;
    let reply = drive(
        control,
        bus.request(
            client,
            &subj("rpc.silent")?,
            &Headers::new(),
            Bytes::from_static(b"s"),
            Duration::from_secs(1),
        ),
    )
    .ok_or_else(stalled)?;
    expect_variant(
        "request without a reply",
        reply,
        |e| matches!(e, BusError::Timeout),
        "Timeout",
    )?;

    let mut consumer = settle(
        control,
        open(
            bus,
            client,
            &fx.stream,
            "suite-c050",
            "suite.x",
            StartPosition::FromFirst,
        ),
    )?;
    settle(control, async { bx("close", bus.close().await) })?;
    match ready_now(consumer.next()) {
        Some(Err(BusError::Unavailable { .. })) => Ok(()),
        Some(Err(other)) => Err(format!("next after close: unexpected {other}")),
        Some(Ok(_)) => Err("delivery after close".to_owned()),
        None => Err("next after close did not complete".to_owned()),
    }
}

/// [0011-051] `Duplicate` is success for the append caller; `fold_duplicate`
/// maps it to `Ok` carrying the original sequence.
pub(super) fn r0011_051<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    let fx = ctx.fx;
    let bus = ctx.bus;
    let id = msg_id("suite-msg-051")?;
    let subject = subj("suite.fold51")?;
    settle(ctx.control, async {
        let first = bx(
            "append",
            bus.append(
                &fx.client,
                &fx.stream,
                &subject,
                &Headers::new(),
                Bytes::from_static(b"f"),
                &id,
            )
            .await,
        )?;
        let original = seq_of(&first)?;
        let raw = bus
            .append(
                &fx.client,
                &fx.stream,
                &subject,
                &Headers::new(),
                Bytes::from_static(b"f"),
                &id,
            )
            .await;
        match fold_duplicate(&fx.stream, raw) {
            Ok(ack) if ack.seq == Some(original) => Ok(()),
            other => Err(format!(
                "duplicate append folded to {other:?}, expected Ok with the original seq"
            )),
        }
    })
}

/// [0011-053] no silent retries of non-idempotent operations; an append retry
/// reuses the same msg_id and stores exactly once.
pub(super) fn r0011_053<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    let fx = ctx.fx;
    let bus = ctx.bus;
    let control = ctx.control;
    let client = fx.client.clone();
    let server = fx.server.clone();
    let id = msg_id("suite-msg-053")?;
    let subject = subj("suite.retry53")?;

    control.set_fault(Fault::Partition {
        set_a: vec![client.clone()],
        set_b: vec![server],
    });
    let under = drive(
        control,
        bus.append(
            &client,
            &fx.stream,
            &subject,
            &Headers::new(),
            Bytes::from_static(b"r"),
            &id,
        ),
    )
    .ok_or_else(stalled)?;
    match under {
        Err(BusError::Timeout | BusError::Unavailable { .. }) => {}
        other => return Err(format!("append under partition: unexpected {other:?}")),
    }
    control.clear_faults();

    let stored = settle(control, async {
        let first = bx(
            "append retry after heal",
            bus.append(
                &client,
                &fx.stream,
                &subject,
                &Headers::new(),
                Bytes::from_static(b"r"),
                &id,
            )
            .await,
        )?;
        seq_of(&first)
    })?;
    settle(control, async {
        expect_variant(
            "append again with the same msg_id",
            bus.append(
                &client,
                &fx.stream,
                &subject,
                &Headers::new(),
                Bytes::from_static(b"r"),
                &id,
            )
            .await,
            |e| matches!(e, BusError::Duplicate { original_seq } if *original_seq == stored),
            "Duplicate",
        )
    })
}
