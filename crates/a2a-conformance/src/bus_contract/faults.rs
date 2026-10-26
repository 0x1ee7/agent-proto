//! Fault rows: at-least-once, ordering, fault switches, determinism, restart,
//! partition, and back-pressure limits.

use std::num::NonZeroUsize;
use std::time::Duration;

use bus_api::bus::{Bus, Consumer};
use bus_api::control::{BusControl, DelayDist, Fault, Probability};
use bus_api::error::BusError;
use bus_api::headers::Headers;
use bus_api::message::StartPosition;
use bus_api::name::{ConsumerName, MsgId, Principal};
use bus_api::value::Digest;
use bytes::Bytes;
use sha2::{Digest as _, Sha256};

use super::messaging::stalled;
use super::support::{expect_delivery, next_of, open, put, seq_of};
use super::{BusFactory, Ctx, bx, pat, settle, subj};
use crate::driver::{drive, ready_now};
use crate::fixtures::Fixtures;

/// Transcript of a scripted run: deliveries seen and the fault trace.
type Transcript = (Vec<String>, Vec<String>);

fn prob(value: f64) -> Result<Probability, String> {
    Probability::new(value).ok_or_else(|| format!("probability {value} out of range"))
}

fn window(size: usize) -> Result<NonZeroUsize, String> {
    NonZeroUsize::new(size).ok_or_else(|| "reorder window must be non-zero".to_owned())
}

fn grew(control: &impl BusControl, before: usize, what: &str) -> Result<(), String> {
    if control.fault_trace().len() > before {
        Ok(())
    } else {
        Err(format!(
            "{what}: fault trace did not record the injected fault"
        ))
    }
}

/// [0011-020] at-least-once: a dropped delivery is not lost, and a duplicated
/// delivery carries the published sequence.
pub(super) fn r0011_020<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    let fx = ctx.fx;
    let bus = ctx.bus;
    let control = ctx.control;

    control.set_fault(Fault::Drop(prob(1.0)?));
    let seq = settle(control, async {
        seq_of(&put(bus, &fx.client, "suite.fl.a", b"f1").await?)
    })?;
    let mut c = settle(
        control,
        open(
            bus,
            &fx.client,
            &fx.stream,
            "suite-c020a",
            "suite.fl.a",
            StartPosition::FromFirst,
        ),
    )?;
    match ready_now(next_of(&mut c)) {
        None => {}
        Some(Ok(_)) => return Err("delivery was not dropped under Drop(1.0)".to_owned()),
        Some(Err(reason)) => return Err(reason),
    }
    control.clear_faults();
    control.advance(fx.ack_wait);
    let d = settle(control, next_of(&mut c))?;
    expect_delivery(&d, "suite.fl.a", b"f1", Some(seq))?;
    bx("ack", d.ack())?;

    control.set_fault(Fault::Duplicate(prob(1.0)?));
    let seq = settle(control, async {
        seq_of(&put(bus, &fx.client, "suite.fl.b", b"f2").await?)
    })?;
    let mut c = settle(
        control,
        open(
            bus,
            &fx.client,
            &fx.stream,
            "suite-c020b",
            "suite.fl.b",
            StartPosition::FromFirst,
        ),
    )?;
    let d = settle(control, next_of(&mut c))?;
    expect_delivery(&d, "suite.fl.b", b"f2", Some(seq))?;
    bx("ack", d.ack())?;
    control.clear_faults();
    Ok(())
}

/// [0011-021] under reordering, each subject still arrives in publish order.
pub(super) fn r0011_021<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    let fx = ctx.fx;
    let bus = ctx.bus;
    let control = ctx.control;
    settle(control, async {
        for (subject, payload) in [
            ("suite.ro.a", b"a1".as_slice()),
            ("suite.ro.b", b"b1".as_slice()),
            ("suite.ro.a", b"a2".as_slice()),
            ("suite.ro.b", b"b2".as_slice()),
            ("suite.ro.a", b"a3".as_slice()),
            ("suite.ro.b", b"b3".as_slice()),
        ] {
            put(bus, &fx.client, subject, payload).await?;
        }
        Ok::<(), String>(())
    })?;

    control.set_fault(Fault::Reorder { window: window(4)? });
    let mut c = settle(
        control,
        open(
            bus,
            &fx.client,
            &fx.stream,
            "suite-c021",
            "suite.ro.>",
            StartPosition::FromFirst,
        ),
    )?;
    let mut seen: Vec<(String, Vec<u8>)> = Vec::new();
    for _ in 0..6 {
        let d = settle(control, next_of(&mut c))?;
        seen.push((d.subject().as_str().to_owned(), d.payload().to_vec()));
        bx("ack", d.ack())?;
    }
    control.clear_faults();

    let on = |subject: &str| -> Vec<Vec<u8>> {
        seen.iter()
            .filter(|(s, _)| s == subject)
            .map(|(_, p)| p.clone())
            .collect()
    };
    if on("suite.ro.a") != [b"a1".to_vec(), b"a2".to_vec(), b"a3".to_vec()] {
        return Err(format!("subject suite.ro.a order {:?}", on("suite.ro.a")));
    }
    if on("suite.ro.b") != [b"b1".to_vec(), b"b2".to_vec(), b"b3".to_vec()] {
        return Err(format!("subject suite.ro.b order {:?}", on("suite.ro.b")));
    }
    Ok(())
}

/// [0011-033] partition tolerance is exercised by the partition check (064).
pub(super) fn r0011_033<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    r0011_064(ctx)
}

/// [0011-060] every fault switch is accepted and its operation completes;
/// drop, duplicate, partition, restart and crash record a trace entry.
pub(super) fn r0011_060<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    let fx = ctx.fx;
    let bus = ctx.bus;
    let control = ctx.control;

    let before = control.fault_trace().len();
    control.set_fault(Fault::Drop(prob(1.0)?));
    settle(control, async {
        put(bus, &fx.client, "suite.f60.drop", b"d").await?;
        Ok::<(), String>(())
    })?;
    let mut c = settle(
        control,
        open(
            bus,
            &fx.client,
            &fx.stream,
            "suite-c060a",
            "suite.f60.drop",
            StartPosition::FromFirst,
        ),
    )?;
    match ready_now(next_of(&mut c)) {
        None => {}
        Some(Ok(_)) => return Err("dropped delivery was delivered".to_owned()),
        Some(Err(reason)) => return Err(reason),
    }
    control.clear_faults();
    grew(control, before, "drop")?;

    let before = control.fault_trace().len();
    control.set_fault(Fault::Duplicate(prob(1.0)?));
    settle(control, async {
        put(bus, &fx.client, "suite.f60.dup", b"u").await?;
        Ok::<(), String>(())
    })?;
    let mut c = settle(
        control,
        open(
            bus,
            &fx.client,
            &fx.stream,
            "suite-c060b",
            "suite.f60.dup",
            StartPosition::FromFirst,
        ),
    )?;
    let d = settle(control, next_of(&mut c))?;
    bx("ack", d.ack())?;
    control.clear_faults();
    grew(control, before, "duplicate")?;

    control.set_fault(Fault::Reorder { window: window(2)? });
    settle(control, async {
        put(bus, &fx.client, "suite.f60.ro.a", b"r1").await?;
        put(bus, &fx.client, "suite.f60.ro.b", b"r2").await?;
        let mut c = open(
            bus,
            &fx.client,
            &fx.stream,
            "suite-c060c",
            "suite.f60.ro.>",
            StartPosition::FromFirst,
        )
        .await?;
        for _ in 0..2 {
            let d = next_of(&mut c).await?;
            bx("ack", d.ack())?;
        }
        Ok::<(), String>(())
    })?;
    control.clear_faults();

    control.set_fault(Fault::Delay(DelayDist::Fixed(Duration::from_secs(1))));
    settle(control, async {
        put(bus, &fx.client, "suite.f60.dl", b"l").await?;
        let mut c = open(
            bus,
            &fx.client,
            &fx.stream,
            "suite-c060d",
            "suite.f60.dl",
            StartPosition::FromFirst,
        )
        .await?;
        let d = next_of(&mut c).await?;
        bx("ack", d.ack())?;
        Ok::<(), String>(())
    })?;
    control.clear_faults();

    let before = control.fault_trace().len();
    let server = fx.server.clone();
    let client = fx.client.clone();
    control.set_fault(Fault::Partition {
        set_a: vec![client.clone()],
        set_b: vec![server.clone()],
    });
    let echo = pat("rpc.echo")?;
    let _serving = settle(control, async {
        bx("serve", bus.serve(&server, &echo).await)
    })?;
    let echo_subject = subj("rpc.echo")?;
    let out = drive(
        control,
        bus.request(
            &client,
            &echo_subject,
            &Headers::new(),
            Bytes::from_static(b"p"),
            Duration::from_secs(1),
        ),
    )
    .ok_or_else(stalled)?;
    match out {
        Err(BusError::Timeout | BusError::Unavailable { .. }) => {}
        other => return Err(format!("partitioned request: unexpected {other:?}")),
    }
    control.clear_faults();
    grew(control, before, "partition")?;

    let before = control.fault_trace().len();
    control.restart_bus();
    settle(control, async {
        put(bus, &fx.client, "suite.f60.rs", b"after").await?;
        Ok::<(), String>(())
    })?;
    grew(control, before, "restart")?;

    let before = control.fault_trace().len();
    settle(control, async {
        put(bus, &fx.client, "suite.f60.cr", b"crash").await?;
        Ok::<(), String>(())
    })?;
    let crash_consumer =
        ConsumerName::parse("suite-c060e").map_err(|e| format!("consumer: {e}"))?;
    let mut c = settle(
        control,
        open(
            bus,
            &fx.client,
            &fx.stream,
            "suite-c060e",
            "suite.f60.cr",
            StartPosition::FromFirst,
        ),
    )?;
    if !control.crash_consumer(&fx.stream, &crash_consumer) {
        return Err("crash_consumer found no consumer to crash".to_owned());
    }
    grew(control, before, "crash consumer")?;
    match ready_now(c.next()) {
        Some(Err(BusError::Unavailable { .. })) => Ok::<(), String>(()),
        other => Err(format!(
            "stale handle after crash: expected Unavailable, got {other:?}"
        )),
    }?;
    let mut again = settle(
        control,
        open(
            bus,
            &fx.client,
            &fx.stream,
            "suite-c060e",
            "suite.f60.cr",
            StartPosition::FromFirst,
        ),
    )?;
    let d = settle(control, next_of(&mut again))?;
    expect_delivery(&d, "suite.f60.cr", b"crash", None)?;
    bx("ack", d.ack())?;
    Ok(())
}

/// Runs one scripted load under the installed faults and returns what was
/// delivered plus the fault trace.
async fn scripted<B: Bus, C: BusControl>(
    bus: &B,
    fx: &Fixtures,
    control: &C,
) -> Result<Transcript, String> {
    control.set_fault(Fault::Drop(prob(0.5)?));
    control.set_fault(Fault::Duplicate(prob(0.5)?));
    control.set_fault(Fault::Reorder { window: window(2)? });
    for i in 0..6 {
        let subject = format!("suite.det.{i}");
        put(bus, &fx.client, &subject, b"det").await?;
    }
    let mut c = open(
        bus,
        &fx.client,
        &fx.stream,
        "suite-c061",
        "suite.det.>",
        StartPosition::FromFirst,
    )
    .await?;
    let mut deliveries = Vec::new();
    for _ in 0..64 {
        match ready_now(next_of(&mut c)) {
            None => break,
            Some(Ok(d)) => {
                deliveries.push(format!(
                    "{} seq={} attempt={}",
                    d.subject().as_str(),
                    d.stream_seq().get(),
                    d.attempt()
                ));
                bx("ack", d.ack())?;
            }
            Some(Err(reason)) => return Err(reason),
        }
    }
    control.clear_faults();
    Ok((deliveries, control.fault_trace()))
}

/// [0011-061] faults are deterministic under the seed: the same seed and the
/// same script produce the same deliveries and the same fault trace.
pub(super) fn r0011_061<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    let (bus_a, control_a) = bx("create", ctx.factory.create(ctx.fx, ctx.seed, None))?;
    let (bus_b, control_b) = bx("create", ctx.factory.create(ctx.fx, ctx.seed, None))?;
    let a = settle(&control_a, scripted(&bus_a, ctx.fx, &control_a))?;
    let b = settle(&control_b, scripted(&bus_b, ctx.fx, &control_b))?;
    if a.1.is_empty() {
        return Err("seeded script injected no faults".to_owned());
    }
    if a != b {
        return Err(format!("same seed, different transcripts: {a:?} vs {b:?}"));
    }
    Ok(())
}

/// Six messages on distinct subjects under loss, duplication and reordering.
const SIX: [(&str, &str); 6] = [
    ("suite.r62.0", "p0"),
    ("suite.r62.1", "p1"),
    ("suite.r62.2", "p2"),
    ("suite.r62.3", "p3"),
    ("suite.r62.4", "p4"),
    ("suite.r62.5", "p5"),
];

/// [0011-062] under faults the taxonomy holds and stored content is unchanged.
pub(super) fn r0011_062<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    let fx = ctx.fx;
    let bus = ctx.bus;
    let control = ctx.control;
    control.set_fault(Fault::Drop(prob(0.3)?));
    control.set_fault(Fault::Duplicate(prob(0.3)?));
    control.set_fault(Fault::Reorder { window: window(4)? });
    settle(control, async {
        for (subject, payload) in SIX {
            put(bus, &fx.client, subject, payload.as_bytes()).await?;
        }
        let mut c = open(
            bus,
            &fx.client,
            &fx.stream,
            "suite-c062",
            "suite.r62.>",
            StartPosition::FromFirst,
        )
        .await?;
        let mut delivered = [false; 6];
        for _ in 0..200 {
            match ready_now(next_of(&mut c)) {
                None => {
                    if delivered.iter().all(|done| *done) {
                        break;
                    }
                    control.advance(fx.ack_wait);
                }
                Some(Ok(d)) => {
                    let Some(index) = SIX.iter().position(|(s, _)| *s == d.subject().as_str())
                    else {
                        return Err(format!("unexpected subject {}", d.subject().as_str()));
                    };
                    let expected = SIX[index].1.as_bytes();
                    if d.payload().as_ref() != expected {
                        return Err(format!("payload changed on {}", SIX[index].0));
                    }
                    delivered[index] = true;
                    bx("ack", d.ack())?;
                }
                Some(Err(reason)) => return Err(reason),
            }
        }
        if let Some(missing) = delivered.iter().position(|done| !done) {
            return Err(format!("{} never delivered under faults", SIX[missing].0));
        }
        Ok::<(), String>(())
    })?;
    control.clear_faults();
    Ok(())
}

/// [0011-063] restart keeps acked appends; unacked deliveries come back with
/// attempt two or more.
pub(super) fn r0011_063<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    let fx = ctx.fx;
    let bus = ctx.bus;
    let control = ctx.control;
    settle(control, async {
        put(bus, &fx.client, "suite.rk.a", b"acked").await?;
        put(bus, &fx.client, "suite.rk.b", b"unacked").await?;
        let mut c = open(
            bus,
            &fx.client,
            &fx.stream,
            "suite-c063",
            "suite.>",
            StartPosition::FromFirst,
        )
        .await?;
        let first = next_of(&mut c).await?;
        expect_delivery(&first, "suite.rk.a", b"acked", None)?;
        bx("ack", first.ack())?;
        let held = next_of(&mut c).await?;
        expect_delivery(&held, "suite.rk.b", b"unacked", None)?;
        drop(held);
        Ok::<(), String>(())
    })?;
    control.restart_bus();
    settle(control, async {
        let mut c = open(
            bus,
            &fx.client,
            &fx.stream,
            "suite-c063",
            "suite.>",
            StartPosition::FromFirst,
        )
        .await?;
        let d = next_of(&mut c).await?;
        expect_delivery(&d, "suite.rk.b", b"unacked", None)?;
        if d.attempt() < 2 {
            return Err(format!(
                "redelivery attempt {}, expected at least 2",
                d.attempt()
            ));
        }
        bx("ack", d.ack())?;
        match ready_now(next_of(&mut c)) {
            None => Ok::<(), String>(()),
            Some(Ok(extra)) => Err(format!(
                "acked message redelivered at seq {}",
                extra.stream_seq().get()
            )),
            Some(Err(reason)) => Err(reason),
        }
    })
}

/// [0011-064] during a partition, unknown-outcome operations return `Timeout`
/// or `Unavailable`, never success without a store; after healing, the same
/// msg_id stores exactly once.
pub(super) fn r0011_064<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    let fx = ctx.fx;
    let bus = ctx.bus;
    let control = ctx.control;
    let client: Principal = fx.client.clone();
    let server: Principal = fx.server.clone();
    let echo = pat("rpc.echo")?;
    let _serving = settle(control, async {
        bx("serve", bus.serve(&server, &echo).await)
    })?;
    let subject = subj("suite.pt.64")?;
    let msg_id = MsgId::parse("suite-msg-064").map_err(|e| format!("msg id: {e}"))?;
    let payload = Bytes::from_static(b"p64");

    control.set_fault(Fault::Partition {
        set_a: vec![client.clone()],
        set_b: vec![server.clone()],
    });
    let appended = drive(
        control,
        bus.append(
            &client,
            &fx.stream,
            &subject,
            &Headers::new(),
            payload.clone(),
            &msg_id,
        ),
    )
    .ok_or_else(stalled)?;
    match appended {
        Err(BusError::Timeout | BusError::Unavailable { .. }) => {}
        other => return Err(format!("append under partition: unexpected {other:?}")),
    }
    let echo_subject = subj("rpc.echo")?;
    let requested = drive(
        control,
        bus.request(
            &client,
            &echo_subject,
            &Headers::new(),
            Bytes::from_static(b"p"),
            Duration::from_secs(1),
        ),
    )
    .ok_or_else(stalled)?;
    match requested {
        Err(BusError::Timeout | BusError::Unavailable { .. }) => {}
        other => return Err(format!("request under partition: unexpected {other:?}")),
    }
    control.clear_faults();

    let ack = settle(control, async {
        bx(
            "append after heal",
            bus.append(
                &client,
                &fx.stream,
                &subject,
                &Headers::new(),
                payload.clone(),
                &msg_id,
            )
            .await,
        )
    })?;
    let stored = seq_of(&ack)?;
    settle(control, async {
        let mut c = open(
            bus,
            &client,
            &fx.stream,
            "suite-c064",
            "suite.pt.64",
            StartPosition::FromFirst,
        )
        .await?;
        let d = next_of(&mut c).await?;
        expect_delivery(&d, "suite.pt.64", b"p64", Some(stored))?;
        bx("ack", d.ack())?;
        match ready_now(next_of(&mut c)) {
            None => Ok::<(), String>(()),
            Some(Ok(extra)) => Err(format!(
                "partitioned append stored twice: extra seq {}",
                extra.stream_seq().get()
            )),
            Some(Err(reason)) => Err(reason),
        }
    })
}

/// [0011-070] blob quota is enforced with `Overloaded`; nothing is dropped
/// silently.
pub(super) fn r0011_070<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    let fx = ctx.fx;
    let bus = ctx.bus;
    let control = ctx.control;
    let chunk = fx.blob_quota / 2;
    let first = vec![1_u8; chunk];
    let second = vec![2_u8; chunk];
    let third = vec![3_u8; chunk];
    let third_digest: [u8; 32] = Sha256::digest(&third).into();
    settle(control, async {
        let da = bx(
            "blob_put",
            bus.blob_put(&fx.client, &fx.local, Bytes::from(first.clone()))
                .await,
        )?;
        bx(
            "blob_put",
            bus.blob_put(&fx.client, &fx.local, Bytes::from(second))
                .await,
        )?;
        match bus
            .blob_put(&fx.client, &fx.local, Bytes::from(third))
            .await
        {
            Err(BusError::Overloaded { retry_after }) if retry_after == fx.overload_retry_after => {
            }
            other => return Err(format!("over quota: expected Overloaded, got {other:?}")),
        }
        let back = bx("blob_get", bus.blob_get(&fx.client, &fx.local, &da).await)?;
        if back.as_ref() != first.as_slice() {
            return Err("earlier blob changed after quota rejection".to_owned());
        }
        let rejected = Digest::from_bytes(third_digest);
        match bus.blob_get(&fx.client, &fx.local, &rejected).await {
            Err(BusError::Absent) => Ok::<(), String>(()),
            other => Err(format!("rejected blob: expected Absent, got {other:?}")),
        }
    })
}

/// [0011-071] unacked deliveries per consumer are bounded by `max_pending`.
pub(super) fn r0011_071<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    let fx = ctx.fx;
    let bus = ctx.bus;
    let control = ctx.control;
    let max = bus.capabilities().max_pending;
    settle(control, async {
        for i in 0..=max {
            put(bus, &fx.client, &format!("suite.pend.{i}"), b"p").await?;
        }
        Ok::<(), String>(())
    })?;
    let mut c = settle(
        control,
        open(
            bus,
            &fx.client,
            &fx.stream,
            "suite-c071",
            "suite.pend.>",
            StartPosition::FromFirst,
        ),
    )?;
    let mut held = Vec::new();
    for _ in 0..max {
        held.push(settle(control, next_of(&mut c))?);
    }
    match ready_now(next_of(&mut c)) {
        None => {}
        Some(Ok(d)) => {
            return Err(format!(
                "delivery beyond max_pending: seq {}",
                d.stream_seq().get()
            ));
        }
        Some(Err(reason)) => return Err(reason),
    }
    let first = held.remove(0);
    bx("ack", first.ack())?;
    match ready_now(next_of(&mut c)) {
        Some(Ok(_)) => Ok(()),
        other => Err(format!("no delivery after an ack freed a slot: {other:?}")),
    }
}
