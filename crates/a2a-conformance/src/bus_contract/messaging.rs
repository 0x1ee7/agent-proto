//! Messaging rows: publish, request/reply, durable streams, replay, limits.

use std::time::Duration;

use bus_api::bus::{Bus, Consumer, RequestStream};
use bus_api::capabilities::M1_MIN_MESSAGE_SIZE;
use bus_api::error::BusError;
use bus_api::headers::Headers;
use bus_api::message::{Request, StartPosition};
use bus_api::name::{ConsumerName, MsgId, StreamName};
use bytes::Bytes;

use super::support::{expect_delivery, headers, next_of, open, put, seq_of};
use super::{BusFactory, Ctx, bx, expect_variant, pat, settle, subj};
use crate::driver::{drive, join, ready_now};
use crate::fixtures::CONSUMER;

/// Result for a driver that ran out of virtual time.
pub(super) fn stalled() -> String {
    "operation did not complete within the virtual-time budget".to_owned()
}

/// [0011-003] subjects, headers and payload pass through byte-exact.
pub(super) fn r0011_003<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    let fx = ctx.fx;
    let bus = ctx.bus;
    let sent = headers(&[("x-a2a-trace", "Trace-01/ok")])?;
    let subject = subj("suite.rt.hdr")?;
    let payload = Bytes::from_static(b"\x00\x01binary\xff");
    settle(ctx.control, async {
        let ack = bx(
            "publish",
            bus.publish(&fx.client, &subject, &sent, payload.clone())
                .await,
        )?;
        let mut c = open(
            bus,
            &fx.client,
            &fx.stream,
            CONSUMER,
            "suite.>",
            StartPosition::FromFirst,
        )
        .await?;
        let d = next_of(&mut c).await?;
        expect_delivery(&d, "suite.rt.hdr", &payload, Some(seq_of(&ack)?))?;
        if d.headers().get("x-a2a-trace") != Some("Trace-01/ok") {
            return Err(format!(
                "header x-a2a-trace came back as {:?}",
                d.headers().get("x-a2a-trace")
            ));
        }
        if d.headers().len() != sent.len() {
            return Err(format!(
                "header count {}, expected {}",
                d.headers().len(),
                sent.len()
            ));
        }
        bx("ack", d.ack())?;
        Ok::<(), String>(())
    })
}

/// [0011-010] publish acks only after the store, with stream and sequence.
pub(super) fn r0011_010<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    let fx = ctx.fx;
    let bus = ctx.bus;
    settle(ctx.control, async {
        let a1 = put(bus, &fx.client, "suite.ack.a", b"one").await?;
        let a2 = put(bus, &fx.client, "suite.ack.a", b"two").await?;
        if a1.stream.as_ref().map(StreamName::as_str) != Some(fx.stream.as_str()) {
            return Err(format!("ack stream {:?}, expected SUITE", a1.stream));
        }
        let s1 = seq_of(&a1)?;
        let s2 = seq_of(&a2)?;
        if s2.get() <= s1.get() {
            return Err(format!(
                "ack seqs {} then {} do not increase",
                s1.get(),
                s2.get()
            ));
        }
        let mut c = open(
            bus,
            &fx.client,
            &fx.stream,
            CONSUMER,
            "suite.>",
            StartPosition::FromFirst,
        )
        .await?;
        let d = next_of(&mut c).await?;
        expect_delivery(&d, "suite.ack.a", b"one", Some(s1))?;
        bx("ack", d.ack())?;
        Ok::<(), String>(())
    })
}

/// [0011-011] request returns one reply, NoResponder, or Timeout.
pub(super) fn r0011_011<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    let fx = ctx.fx;
    let bus = ctx.bus;
    let echo = pat("rpc.echo")?;
    let echo_subject = subj("rpc.echo")?;

    // Happy path: the server serves before the client requests.
    let server = async {
        let mut requests = bx("serve", bus.serve(&fx.server, &echo).await)?;
        let req = bx("serve next", requests.next().await)?;
        let Request { payload, reply, .. } = req;
        reply
            .reply(Headers::new(), payload)
            .map_err(|e| format!("reply: {e:?}"))?;
        Ok::<(), String>(())
    };
    let client = async {
        let reply = bx(
            "request",
            bus.request(
                &fx.client,
                &echo_subject,
                &Headers::new(),
                Bytes::from_static(b"ping"),
                fx.request_timeout,
            )
            .await,
        )?;
        if reply.payload.as_ref() != b"ping" {
            return Err(format!(
                "reply payload {:?}, expected \"ping\"",
                reply.payload
            ));
        }
        Ok::<(), String>(())
    };
    settle(ctx.control, async {
        let (s, c) = join(server, client).await;
        s?;
        c
    })?;

    // Silent server: the request must time out, not hang.
    let silent_pat = pat("rpc.silent")?;
    let _silent = settle(ctx.control, async {
        bx("serve silent", bus.serve(&fx.server, &silent_pat).await)
    })?;
    let silent_subject = subj("rpc.silent")?;
    let silent_out = drive(
        ctx.control,
        bus.request(
            &fx.client,
            &silent_subject,
            &Headers::new(),
            Bytes::from_static(b"ping"),
            fx.scaled(Duration::from_secs(1)),
        ),
    )?
    .ok_or_else(stalled)?;
    expect_variant(
        "silent request",
        silent_out,
        |e| matches!(e, BusError::Timeout),
        "Timeout",
    )?;

    // Unserved subject: NoResponder when the binding declares the signal.
    let nobody_subject = subj("rpc.nobody")?;
    let nobody = drive(
        ctx.control,
        bus.request(
            &fx.client,
            &nobody_subject,
            &Headers::new(),
            Bytes::from_static(b"ping"),
            fx.request_timeout,
        ),
    )?
    .ok_or_else(stalled)?;
    if ctx.bus.capabilities().no_responder_signal {
        expect_variant(
            "unserved request",
            nobody,
            |e| matches!(e, BusError::NoResponder),
            "NoResponder",
        )
    } else {
        expect_variant(
            "unserved request",
            nobody,
            |e| matches!(e, BusError::NoResponder | BusError::Timeout),
            "NoResponder or Timeout",
        )
    }
}

/// [0011-012] each request gets its own reply through its own handle.
///
/// Single use of the reply handle is enforced by type: `ReplyHandle::reply`
/// consumes the handle, so a second reply does not compile. This check covers
/// the run-time part: distinct requests get distinct replies.
pub(super) fn r0011_012<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    let fx = ctx.fx;
    let bus = ctx.bus;
    let echo = pat("rpc.echo")?;
    let echo_subject = subj("rpc.echo")?;
    let server = async {
        let mut requests = bx("serve", bus.serve(&fx.server, &echo).await)?;
        for _ in 0..2 {
            let req = bx("serve next", requests.next().await)?;
            let Request { payload, reply, .. } = req;
            reply
                .reply(Headers::new(), payload)
                .map_err(|e| format!("reply: {e:?}"))?;
        }
        Ok::<(), String>(())
    };
    let client = async {
        for word in [b"first".as_slice(), b"second".as_slice()] {
            let reply = bx(
                "request",
                bus.request(
                    &fx.client,
                    &echo_subject,
                    &Headers::new(),
                    Bytes::copy_from_slice(word),
                    fx.request_timeout,
                )
                .await,
            )?;
            if reply.payload.as_ref() != word {
                return Err(format!("reply {:?} for request {:?}", reply.payload, word));
            }
        }
        Ok::<(), String>(())
    };
    settle(ctx.control, async {
        let (s, c) = join(server, client).await;
        s?;
        c
    })
}

/// [0011-013] a repeated msg_id inside the window is `Duplicate`; no second copy.
pub(super) fn r0011_013<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    let fx = ctx.fx;
    let bus = ctx.bus;
    let subject = subj("suite.dd.13")?;
    let msg_id = MsgId::parse("suite-msg-013").map_err(|e| format!("msg id: {e}"))?;
    settle(ctx.control, async {
        let first = bx(
            "append",
            bus.append(
                &fx.client,
                &fx.stream,
                &subject,
                &Headers::new(),
                Bytes::from_static(b"once"),
                &msg_id,
            )
            .await,
        )?;
        let s1 = seq_of(&first)?;
        match bus
            .append(
                &fx.client,
                &fx.stream,
                &subject,
                &Headers::new(),
                Bytes::from_static(b"once"),
                &msg_id,
            )
            .await
        {
            Err(BusError::Duplicate { original_seq }) if original_seq == s1 => {}
            other => {
                return Err(format!(
                    "second append: expected Duplicate{{original_seq: {}}}, got {other:?}",
                    s1.get()
                ));
            }
        }
        let mut c = open(
            bus,
            &fx.client,
            &fx.stream,
            CONSUMER,
            "suite.dd.13",
            StartPosition::FromFirst,
        )
        .await?;
        let d = next_of(&mut c).await?;
        expect_delivery(&d, "suite.dd.13", b"once", Some(s1))?;
        bx("ack", d.ack())?;
        match ready_now(next_of(&mut c)) {
            None => Ok::<(), String>(()),
            Some(Ok(extra)) => Err(format!(
                "second copy delivered at seq {}",
                extra.stream_seq().get()
            )),
            Some(Err(reason)) => Err(reason),
        }
    })
}

/// [0011-014] `FromSeq(n)` delivers at or after n, in order, with no skip.
pub(super) fn r0011_014<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    let fx = ctx.fx;
    let bus = ctx.bus;
    settle(ctx.control, async {
        let a1 = put(bus, &fx.client, "suite.seq.a", b"m1").await?;
        let a2 = put(bus, &fx.client, "suite.seq.a", b"m2").await?;
        let a3 = put(bus, &fx.client, "suite.seq.a", b"m3").await?;
        let s2 = seq_of(&a2)?;
        let s3 = seq_of(&a3)?;
        let _ = seq_of(&a1)?;
        let mut c = open(
            bus,
            &fx.client,
            &fx.stream,
            CONSUMER,
            "suite.>",
            StartPosition::FromSeq(s2),
        )
        .await?;
        let d2 = next_of(&mut c).await?;
        expect_delivery(&d2, "suite.seq.a", b"m2", Some(s2))?;
        bx("ack", d2.ack())?;
        let d3 = next_of(&mut c).await?;
        expect_delivery(&d3, "suite.seq.a", b"m3", Some(s3))?;
        bx("ack", d3.ack())?;
        Ok::<(), String>(())
    })
}

/// [0011-015] ack, nak with delay, term, and `ack_wait` redelivery.
pub(super) fn r0011_015<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    let fx = ctx.fx;
    let bus = ctx.bus;
    let control = ctx.control;
    let mut c = settle(control, async {
        open(
            bus,
            &fx.client,
            &fx.stream,
            CONSUMER,
            "suite.>",
            StartPosition::FromFirst,
        )
        .await
    })?;

    // nak(delay): no early redelivery, then redelivery after the delay.
    let seq_a = settle(control, async {
        seq_of(&put(bus, &fx.client, "suite.st.a", b"one").await?)
    })?;
    let d1 = settle(control, next_of(&mut c))?;
    expect_delivery(&d1, "suite.st.a", b"one", Some(seq_a))?;
    bx("nak", d1.nak(fx.scaled(Duration::from_secs(2))))?;
    match ready_now(next_of(&mut c)) {
        None => {}
        Some(Ok(_)) => return Err("nak'd delivery redelivered before its delay".to_owned()),
        Some(Err(reason)) => return Err(reason),
    }
    let before = control.now()?;
    let d2 = settle(control, next_of(&mut c))?;
    let waited = control.now()?.saturating_sub(before);
    if waited < fx.scaled(Duration::from_secs(2)) {
        return Err(format!(
            "nak redelivery after {waited:?}, expected at least 2s"
        ));
    }
    expect_delivery(&d2, "suite.st.a", b"one", Some(seq_a))?;
    if d2.attempt() < 2 {
        return Err(format!(
            "redelivery attempt {}, expected at least 2",
            d2.attempt()
        ));
    }

    // term: never redelivered; the next message arrives instead.
    bx("term", d2.term())?;
    let seq_b = settle(control, async {
        seq_of(&put(bus, &fx.client, "suite.st.b", b"two").await?)
    })?;
    let d3 = settle(control, next_of(&mut c))?;
    expect_delivery(&d3, "suite.st.b", b"two", Some(seq_b))?;
    bx("ack", d3.ack())?;

    // unacked past ack_wait: redelivered with attempt >= 2.
    let seq_c = settle(control, async {
        seq_of(&put(bus, &fx.client, "suite.st.c", b"three").await?)
    })?;
    let d4 = settle(control, next_of(&mut c))?;
    expect_delivery(&d4, "suite.st.c", b"three", Some(seq_c))?;
    drop(d4);
    match ready_now(next_of(&mut c)) {
        None => {}
        Some(Ok(_)) => return Err("unacked delivery redelivered before ack_wait".to_owned()),
        Some(Err(reason)) => return Err(reason),
    }
    let before = control.now()?;
    let d5 = settle(control, next_of(&mut c))?;
    let waited = control.now()?.saturating_sub(before);
    if waited < fx.ack_wait {
        return Err(format!(
            "ack_wait redelivery after {waited:?}, expected at least {:?}",
            fx.ack_wait
        ));
    }
    expect_delivery(&d5, "suite.st.c", b"three", Some(seq_c))?;
    if d5.attempt() < 2 {
        return Err(format!(
            "redelivery attempt {}, expected at least 2",
            d5.attempt()
        ));
    }
    bx("ack", d5.ack())?;
    Ok(())
}

/// [0011-019] close leaves unacked deliveries pending; restart redelivers them.
pub(super) fn r0011_019<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    let fx = ctx.fx;
    let bus = ctx.bus;
    let control = ctx.control;
    settle(control, async {
        put(bus, &fx.client, "suite.rs.a", b"acked").await?;
        put(bus, &fx.client, "suite.rs.b", b"unacked").await?;
        let mut c = open(
            bus,
            &fx.client,
            &fx.stream,
            CONSUMER,
            "suite.>",
            StartPosition::FromFirst,
        )
        .await?;
        let first = next_of(&mut c).await?;
        expect_delivery(&first, "suite.rs.a", b"acked", None)?;
        bx("ack", first.ack())?;
        let held = next_of(&mut c).await?;
        expect_delivery(&held, "suite.rs.b", b"unacked", None)?;
        drop(held);
        bx("close", bus.close().await)?;
        match ready_now(c.next()) {
            Some(Err(BusError::Unavailable { .. })) => Ok::<(), String>(()),
            other => Err(format!(
                "next after close: expected Unavailable, got {other:?}"
            )),
        }
    })?;
    control.restart_bus()?;
    settle(control, async {
        let mut c = open(
            bus,
            &fx.client,
            &fx.stream,
            CONSUMER,
            "suite.>",
            StartPosition::FromFirst,
        )
        .await?;
        let d = next_of(&mut c).await?;
        expect_delivery(&d, "suite.rs.b", b"unacked", None)?;
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

/// [0011-022] sequential publish-acks on one subject are stored in order.
pub(super) fn r0011_022<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    let fx = ctx.fx;
    let bus = ctx.bus;
    settle(ctx.control, async {
        let mut seqs = Vec::new();
        for payload in [b"x1".as_slice(), b"x2".as_slice(), b"x3".as_slice()] {
            let subject = subj("suite.ord.x")?;
            let ack = bx(
                "publish",
                bus.publish(
                    &fx.client,
                    &subject,
                    &Headers::new(),
                    Bytes::copy_from_slice(payload),
                )
                .await,
            )?;
            seqs.push((seq_of(&ack)?, payload));
        }
        if seqs
            .windows(2)
            .any(|pair| pair[0].0.get() >= pair[1].0.get())
        {
            return Err("publish-ack sequences are not strictly increasing".to_owned());
        }
        let mut c = open(
            bus,
            &fx.client,
            &fx.stream,
            CONSUMER,
            "suite.ord.x",
            StartPosition::FromFirst,
        )
        .await?;
        for (seq, payload) in &seqs {
            let d = next_of(&mut c).await?;
            expect_delivery(&d, "suite.ord.x", payload, Some(*seq))?;
            bx("ack", d.ack())?;
        }
        Ok::<(), String>(())
    })
}

/// [0011-023] dedupe covers the window; past it a second copy may be stored.
pub(super) fn r0011_023<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    let fx = ctx.fx;
    let bus = ctx.bus;
    let subject = subj("suite.dd.23")?;
    let msg_id = MsgId::parse("suite-msg-023").map_err(|e| format!("msg id: {e}"))?;
    let window = bus.capabilities().dedupe_window;
    settle(ctx.control, async {
        let first = bx(
            "append",
            bus.append(
                &fx.client,
                &fx.stream,
                &subject,
                &Headers::new(),
                Bytes::from_static(b"w"),
                &msg_id,
            )
            .await,
        )?;
        let s1 = seq_of(&first)?;
        match bus
            .append(
                &fx.client,
                &fx.stream,
                &subject,
                &Headers::new(),
                Bytes::from_static(b"w"),
                &msg_id,
            )
            .await
        {
            Err(BusError::Duplicate { original_seq }) if original_seq == s1 => Ok::<(), String>(()),
            other => Err(format!(
                "in-window append: expected Duplicate, got {other:?}"
            )),
        }
    })?;
    ctx.control.advance(window)?;
    settle(ctx.control, async {
        let outside = bus
            .append(
                &fx.client,
                &fx.stream,
                &subject,
                &Headers::new(),
                Bytes::from_static(b"w"),
                &msg_id,
            )
            .await;
        match outside {
            Ok(_) | Err(BusError::Duplicate { .. }) => Ok::<(), String>(()),
            other => Err(format!("out-of-window append: unexpected {other:?}")),
        }
    })
}

/// [0011-024] retained `FromSeq` delivers everything; a lost range is
/// reported as `RETENTION_EXCEEDED`.
pub(super) fn r0011_024<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    let fx = ctx.fx;
    let bus = ctx.bus;
    let control = ctx.control;
    let mut seqs = Vec::new();
    settle(control, async {
        for payload in [
            b"r1".as_slice(),
            b"r2".as_slice(),
            b"r3".as_slice(),
            b"r4".as_slice(),
        ] {
            let subject = subj("ret.r.a")?;
            let ack = bx(
                "publish",
                bus.publish(
                    &fx.client,
                    &subject,
                    &Headers::new(),
                    Bytes::copy_from_slice(payload),
                )
                .await,
            )?;
            seqs.push(seq_of(&ack)?);
        }
        Ok::<(), String>(())
    })?;

    let lost_consumer = ConsumerName::parse("suite-c024a").map_err(|e| format!("consumer: {e}"))?;
    let filter = pat("ret.>")?;
    let lost = drive(
        control,
        raw_first(
            bus,
            &fx.client,
            &fx.retained,
            &lost_consumer,
            &filter,
            StartPosition::FromSeq(seqs[0]),
        ),
    )?
    .ok_or_else(stalled)?;
    match lost {
        Err(BusError::Unavailable {
            reason: "RETENTION_EXCEEDED",
        }) => {}
        other => {
            return Err(format!(
                "FromSeq below retention: expected Unavailable RETENTION_EXCEEDED, got {other:?}"
            ));
        }
    }

    let retained_consumer =
        ConsumerName::parse("suite-c024b").map_err(|e| format!("consumer: {e}"))?;
    settle(control, async {
        let mut c = bx(
            "consume",
            bus.consume(
                &fx.client,
                &fx.retained,
                &retained_consumer,
                &filter,
                StartPosition::FromSeq(seqs[2]),
            )
            .await,
        )?;
        let d3 = next_of(&mut c).await?;
        expect_delivery(&d3, "ret.r.a", b"r3", Some(seqs[2]))?;
        bx("ack", d3.ack())?;
        let d4 = next_of(&mut c).await?;
        expect_delivery(&d4, "ret.r.a", b"r4", Some(seqs[3]))?;
        bx("ack", d4.ack())?;
        Ok::<(), String>(())
    })
}

/// Consume and receive the first delivery, keeping the raw error variant.
async fn raw_first<B: Bus>(
    bus: &B,
    who: &bus_api::name::Principal,
    stream: &StreamName,
    consumer: &ConsumerName,
    filter: &bus_api::name::SubjectPattern,
    start: StartPosition,
) -> Result<bus_api::message::Delivery, BusError> {
    let mut c = bus.consume(who, stream, consumer, filter, start).await?;
    c.next().await
}

/// [0011-032] publish limit enforced at the declared `max_message_size`.
pub(super) fn r0011_032<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    let fx = ctx.fx;
    let bus = ctx.bus;
    let max = bus.capabilities().max_message_size;
    if max < M1_MIN_MESSAGE_SIZE {
        return Err(format!(
            "max_message_size {max} below profile minimum {M1_MIN_MESSAGE_SIZE}"
        ));
    }
    let subject = subj("suite.big.x")?;
    let at_limit = drive(
        ctx.control,
        bus.publish(
            &fx.client,
            &subject,
            &Headers::new(),
            Bytes::from(vec![0_u8; max]),
        ),
    )?
    .ok_or_else(stalled)?;
    if let Err(e) = at_limit {
        return Err(format!(
            "publish of exactly max_message_size: unexpected {e:?}"
        ));
    }
    let over = drive(
        ctx.control,
        bus.publish(
            &fx.client,
            &subject,
            &Headers::new(),
            Bytes::from(vec![0_u8; max + 1]),
        ),
    )?
    .ok_or_else(stalled)?;
    expect_variant(
        "publish over max_message_size",
        over,
        |e| matches!(e, BusError::PayloadTooLarge { limit } if *limit == max),
        "PayloadTooLarge with limit = max_message_size",
    )
}
