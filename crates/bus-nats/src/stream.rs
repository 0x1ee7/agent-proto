//! Durable streams (RFC-0010 section 4.5): publish and append with `Nats-Msg-Id`
//! dedupe, and durable push consumers with explicit acknowledgement.
//!
//! A stream is not chosen by this binding from the subject. A subject is stored in
//! whichever JetStream stream captures it; `append` and `consume` check the stream's
//! own subject list before they touch it.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_nats::HeaderMap;
use async_nats::jetstream::consumer::push::{self, Messages};
use async_nats::jetstream::consumer::{AckPolicy, DeliverPolicy, ReplayPolicy};
use async_nats::jetstream::context::{PublishError, PublishErrorKind};
use async_nats::jetstream::publish::PublishAck as JsAck;
use async_nats::jetstream::stream::Stream;
use bus_api::bus::Consumer;
use bus_api::error::BusError;
use bus_api::headers::Headers;
use bus_api::message::{Delivery, PublishAck, StartPosition};
use bus_api::name::{ConsumerName, MsgId, StreamName, Subject, SubjectPattern};
use bus_api::value::StreamSeq;
use bytes::Bytes;
use futures_util::StreamExt;

use crate::connection::NatsBus;
use crate::denial::{CORE_PUBLISH_REPORT_WINDOW, Denied, denied_within, guard};
use crate::errors::{self, reason};
use crate::layout::{MAX_ACK_PENDING, MAX_DELIVER};
use crate::principal::{inbox_prefix, principal_from_subject};
use crate::settle::JetStreamSettlement;
use crate::wire::{check_budget, inbound_headers, to_header_map};

/// Header carrying the JetStream dedupe key (`Nats-Msg-Id`).
pub(crate) const NATS_MSG_ID: &str = "Nats-Msg-Id";

/// Heartbeat interval of push consumers, so a stalled delivery is noticed.
const IDLE_HEARTBEAT: Duration = Duration::from_secs(5);

/// True when the NATS subject pattern `pattern` matches the literal `subject`.
/// `*` matches one token; `>` matches one or more trailing tokens.
pub(crate) fn pattern_matches(pattern: &str, subject: &str) -> bool {
    let mut pattern_tokens = pattern.split('.');
    let mut subject_tokens = subject.split('.');
    loop {
        match (pattern_tokens.next(), subject_tokens.next()) {
            (Some(">"), Some(_)) => return true,
            (Some("*"), Some(_)) => {}
            (Some(p), Some(s)) if p == s => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}

/// True when every subject matched by `filter` is also matched by `captured`.
/// Both are NATS subject patterns; a filter wider than the capture is refused.
pub(crate) fn pattern_covers(captured: &str, filter: &str) -> bool {
    let mut captured_tokens = captured.split('.');
    let mut filter_tokens = filter.split('.');
    loop {
        match (captured_tokens.next(), filter_tokens.next()) {
            (Some(">"), Some(_)) => return true,
            (Some(_), Some(">")) => return false,
            (Some("*"), Some(_)) => {}
            (Some(c), Some(f)) if c == f => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}

fn stream_name(name: &str) -> Result<StreamName, BusError> {
    StreamName::parse(name).map_err(|_| BusError::Internal)
}

/// Opens the stream by name; its configuration (subjects) comes with the handle.
async fn open_stream(bus: &NatsBus, stream: &StreamName) -> Result<Stream, BusError> {
    let info = format!("$JS.API.STREAM.INFO.{}", stream.as_str());
    match guard(
        bus.denials(),
        |s: &str| s == info,
        bus.js().get_stream(stream.as_str()),
    )
    .await?
    {
        Ok(handle) => Ok(handle),
        Err(_) => Err(errors::unavailable(reason::STREAM_NOT_FOUND)),
    }
}

/// Publishes to a subject through JetStream and waits for the PubAck. The outer
/// `Err` is a publish the identity may not make.
pub(crate) async fn publish_durable(
    bus: &NatsBus,
    subject: &str,
    headers: HeaderMap,
    payload: Bytes,
) -> Result<Result<JsAck, PublishError>, Denied> {
    guard(bus.denials(), |s: &str| s == subject, async {
        let pending = bus
            .js()
            .publish_with_headers(subject.to_owned(), headers, payload)
            .await?;
        pending.await
    })
    .await
}

/// `Bus::publish`: a subject some stream captures is stored by that stream; any
/// other subject is a core publish that no stream stores.
pub(crate) async fn publish(
    bus: &NatsBus,
    subject: &Subject,
    headers: &Headers,
    payload: Bytes,
) -> Result<PublishAck, BusError> {
    check_budget(headers, &payload)?;
    match publish_durable(
        bus,
        subject.as_str(),
        to_header_map(headers),
        payload.clone(),
    )
    .await?
    {
        Ok(ack) => Ok(PublishAck {
            stream: Some(stream_name(&ack.stream)?),
            seq: Some(StreamSeq::new(ack.sequence)),
        }),
        Err(err) if err.kind() == PublishErrorKind::StreamNotFound => {
            core_publish(bus, subject, headers, payload).await
        }
        Err(err) => Err(errors::js_publish(&err)),
    }
}

/// Core publish for a subject no stream captures. The server drops a publish the
/// identity may not make without a reply, so the publish is flushed and a denial
/// report that follows is awaited briefly (see `denial`).
async fn core_publish(
    bus: &NatsBus,
    subject: &Subject,
    headers: &Headers,
    payload: Bytes,
) -> Result<PublishAck, BusError> {
    let mut reports = bus.denials().subscribe();
    bus.client()
        .publish_with_headers(subject.as_str().to_owned(), to_header_map(headers), payload)
        .await
        .map_err(|err| errors::core_publish(&err))?;
    bus.client()
        .flush()
        .await
        .map_err(|_| errors::unavailable(reason::PUBLISH_FAILED))?;
    let denied = denied_within(
        &mut reports,
        |s: &str| s == subject.as_str(),
        CORE_PUBLISH_REPORT_WINDOW,
    )
    .await;
    if denied {
        return Err(BusError::PermissionDenied);
    }
    Ok(PublishAck {
        stream: None,
        seq: None,
    })
}

/// `Bus::append`: stores the message in `stream` with `msg_id` as the dedupe key.
/// A retry with the same `msg_id` inside the stream's duplicate window is refused
/// with `Duplicate` and the original sequence. A subject `stream` does not capture
/// is `Absent`.
pub(crate) async fn append(
    bus: &NatsBus,
    stream: &StreamName,
    subject: &Subject,
    headers: &Headers,
    payload: Bytes,
    msg_id: &MsgId,
) -> Result<PublishAck, BusError> {
    check_budget(headers, &payload)?;
    let handle = open_stream(bus, stream).await?;
    let captured = handle
        .cached_info()
        .config
        .subjects
        .iter()
        .any(|pattern| pattern_matches(pattern, subject.as_str()));
    if !captured {
        return Err(BusError::Absent);
    }
    let mut map = to_header_map(headers);
    map.insert(NATS_MSG_ID, msg_id.as_str());
    let ack = publish_durable(bus, subject.as_str(), map, payload)
        .await?
        .map_err(|err| errors::js_publish(&err))?;
    if ack.duplicate {
        return Err(BusError::Duplicate {
            original_seq: StreamSeq::new(ack.sequence),
        });
    }
    Ok(PublishAck {
        stream: Some(stream.clone()),
        seq: Some(StreamSeq::new(ack.sequence)),
    })
}

/// Durable push consumer over one stream. Each delivery carries its ack handle.
pub struct NatsConsumer {
    messages: Messages,
    /// The connection's closed flag; once set, `next` fails at once.
    closed: Arc<AtomicBool>,
}
impl fmt::Debug for NatsConsumer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NatsConsumer").finish_non_exhaustive()
    }
}

impl Consumer for NatsConsumer {
    async fn next(&mut self) -> Result<Delivery, BusError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(errors::unavailable(reason::CONSUMER_CLOSED));
        }
        let msg = self
            .messages
            .next()
            .await
            .ok_or_else(|| errors::unavailable(reason::CONSUMER_CLOSED))?
            .map_err(|_| errors::unavailable(reason::CONSUMER_CLOSED))?;
        let info = msg
            .info()
            .map_err(|_| errors::unavailable(reason::CONSUMER_CLOSED))?;
        let stream_seq = StreamSeq::new(info.stream_sequence);
        let attempt = u32::try_from(info.delivered).unwrap_or(u32::MAX);
        let (message, acker) = msg.split();
        let subject = Subject::parse(message.subject.as_str()).map_err(|_| BusError::Internal)?;
        let headers = inbound_headers(message.headers.as_ref()).ok_or(BusError::Internal)?;
        let principal = principal_from_subject(subject.as_str());
        Ok(Delivery::new(
            subject,
            principal,
            headers,
            message.payload,
            stream_seq,
            attempt,
            Box::new(JetStreamSettlement::new(acker)),
        ))
    }
}

/// `Bus::consume`: attaches the durable consumer `consumer` over `stream`, creating
/// it when absent. An existing consumer is attached as it is (its filter and start
/// are fixed at creation), matching the reference binding. A filter the stream does
/// not capture is `Absent`.
pub(crate) async fn consume(
    bus: &NatsBus,
    stream: &StreamName,
    consumer: &ConsumerName,
    filter: &SubjectPattern,
    start: StartPosition,
) -> Result<NatsConsumer, BusError> {
    let mut stream_handle = open_stream(bus, stream).await?;
    let captured = stream_handle
        .cached_info()
        .config
        .subjects
        .iter()
        .any(|pattern| pattern_covers(pattern, filter.as_str()));
    if !captured {
        return Err(BusError::Absent);
    }
    let deliver_policy = match start {
        StartPosition::FromSeq(seq) => {
            let info = stream_handle
                .info()
                .await
                .map_err(|_| errors::unavailable(reason::JETSTREAM_API))?;
            let k = seq.get().max(1);
            if k < info.state.first_sequence {
                return Err(errors::unavailable(reason::RETENTION_EXCEEDED));
            }
            if k > info.state.last_sequence.saturating_add(1) {
                return Err(errors::unavailable(reason::SEQ_NOT_YET_ASSIGNED));
            }
            DeliverPolicy::ByStartSequence { start_sequence: k }
        }
        StartPosition::FromFirst => DeliverPolicy::All,
        StartPosition::NewOnly => DeliverPolicy::New,
    };
    let config = push::Config {
        durable_name: Some(consumer.as_str().to_owned()),
        deliver_subject: format!("{}.c.{}", inbox_prefix(bus.identity()), consumer.as_str()),
        filter_subject: filter.as_str().to_owned(),
        ack_policy: AckPolicy::Explicit,
        ack_wait: bus.ack_wait(),
        max_deliver: MAX_DELIVER,
        max_ack_pending: MAX_ACK_PENDING,
        deliver_policy,
        replay_policy: ReplayPolicy::Instant,
        flow_control: true,
        idle_heartbeat: IDLE_HEARTBEAT,
        ..Default::default()
    };
    let create = stream_handle.get_or_create_consumer::<push::Config>(consumer.as_str(), config);
    let handle = match guard(
        bus.denials(),
        |s: &str| s.starts_with("$JS.API.CONSUMER."),
        create,
    )
    .await?
    {
        Ok(handle) => handle,
        Err(_) => return Err(errors::unavailable(reason::JETSTREAM_API)),
    };
    let messages = handle
        .messages()
        .await
        .map_err(|_| errors::unavailable(reason::CONSUMER_CLOSED))?;
    Ok(NatsConsumer {
        messages,
        closed: bus.closed(),
    })
}

#[cfg(test)]
mod tests {
    use super::{pattern_covers, pattern_matches};

    // 0011-013: a publish is stored by the stream whose subjects capture it.
    #[test]
    fn req_0011_013_capture_matches_wildcards() {
        assert!(pattern_matches("suite.>", "suite.a"));
        assert!(pattern_matches("suite.>", "suite.a.b"));
        assert!(pattern_matches("suite.*", "suite.a"));
        assert!(!pattern_matches("suite.*", "suite.a.b"));
        assert!(!pattern_matches("suite.>", "suite"));
        assert!(!pattern_matches("ret.>", "suite.a"));
    }

    // 0011-040: a consumer filter the stream does not capture is refused.
    #[test]
    fn req_0011_040_filter_must_be_covered_by_capture() {
        assert!(pattern_covers("suite.>", "suite.>"));
        assert!(pattern_covers("suite.>", "suite.a.>"));
        assert!(pattern_covers("suite.>", "suite.a"));
        assert!(!pattern_covers("suite.>", "suite"));
        assert!(!pattern_covers("suite.*", "suite.>"));
        assert!(!pattern_covers("ret.>", "suite.>"));
    }
}
