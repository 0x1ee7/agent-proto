//! Helpers shared by the check modules: publish, open, receive and labels.

use bus_api::bus::{Bus, Consumer};
use bus_api::headers::Headers;
use bus_api::message::{Delivery, PublishAck, StartPosition};
use bus_api::name::{ConsumerName, Principal, StreamName};
use bus_api::value::StreamSeq;
use bytes::Bytes;

use super::{bx, pat, subj};

/// Builds headers from literal pairs.
pub(super) fn headers(pairs: &[(&str, &str)]) -> Result<Headers, String> {
    let mut out = Headers::new();
    for (name, value) in pairs {
        out.insert(name, value)
            .map_err(|e| format!("header {name}: {e}"))?;
    }
    Ok(out)
}

/// Stream sequence from an ack; an ack without one is a failure.
pub(super) fn seq_of(ack: &PublishAck) -> Result<StreamSeq, String> {
    ack.seq
        .ok_or_else(|| "publish ack carries no stream sequence".to_owned())
}

/// Publishes `payload` on `subject` with no headers.
pub(super) async fn put<B: Bus>(
    bus: &B,
    who: &Principal,
    subject: &str,
    payload: &'static [u8],
) -> Result<PublishAck, String> {
    let subject = subj(subject)?;
    bx(
        "publish",
        bus.publish(who, &subject, &Headers::new(), Bytes::from_static(payload))
            .await,
    )
}

/// Opens a consumer on `stream` with `consumer` and `filter`.
pub(super) async fn open<B: Bus>(
    bus: &B,
    who: &Principal,
    stream: &StreamName,
    consumer: &str,
    filter: &str,
    start: StartPosition,
) -> Result<B::Consumer, String> {
    let consumer =
        ConsumerName::parse(consumer).map_err(|e| format!("consumer {consumer}: {e}"))?;
    let filter = pat(filter)?;
    bx(
        "consume",
        bus.consume(who, stream, &consumer, &filter, start).await,
    )
}

/// Receives the next delivery from `consumer`.
pub(super) async fn next_of<C: Consumer>(consumer: &mut C) -> Result<Delivery, String> {
    bx("consumer next", consumer.next().await)
}

/// Checks a delivery's subject, payload and, when given, sequence.
pub(super) fn expect_delivery(
    delivery: &Delivery,
    subject: &str,
    payload: &[u8],
    seq: Option<StreamSeq>,
) -> Result<(), String> {
    if delivery.subject().as_str() != subject {
        return Err(format!(
            "delivery subject {}, expected {subject}",
            delivery.subject().as_str()
        ));
    }
    if delivery.payload().as_ref() != payload {
        return Err(format!(
            "delivery payload {:?}, expected {:?}",
            delivery.payload(),
            payload
        ));
    }
    if let Some(seq) = seq {
        if delivery.stream_seq() != seq {
            return Err(format!(
                "delivery seq {}, expected {}",
                delivery.stream_seq().get(),
                seq.get()
            ));
        }
    }
    Ok(())
}
