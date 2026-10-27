//! KV buckets (RFC-0010 section 4.6): put, get, create, update, delete and watch
//! over the `KV_<bucket>` streams.
//!
//! Every write is a JetStream publish to `$KV.<bucket>.<key>`. Live entries are
//! messages without a `KV-Operation` other than `PUT` and without a
//! `Nats-Marker-Reason`; deletes and TTL expiries are markers.

use std::fmt;
use std::time::Duration;

use async_nats::HeaderMap;
use async_nats::jetstream::consumer::push::{self, Messages};
use async_nats::jetstream::consumer::{AckPolicy, DeliverPolicy, ReplayPolicy};
use async_nats::jetstream::context::{GetStreamErrorKind, PublishError, PublishErrorKind};
use async_nats::jetstream::stream::DirectGetErrorKind;
use bus_api::bus::Watch;
use bus_api::error::BusError;
use bus_api::message::{Entry, WatchEvent};
use bus_api::name::{Bucket, Key, KeyPrefix};
use bus_api::value::Revision;
use bytes::Bytes;
use futures_util::StreamExt;

use crate::connection::NatsBus;
use crate::denial::guard;
use crate::errors::{self, reason};
use crate::layout::{kv_bucket, kv_stream, kv_subject};
use crate::stream::publish_durable;

/// Header that carries the TTL of a put (RFC-0010 0010-031).
const NATS_TTL: &str = "Nats-TTL";
/// Header that makes a write conditional on the last sequence of its subject.
const EXPECTED_LAST_SUBJECT_SEQ: &str = "Nats-Expected-Last-Subject-Sequence";
/// Header that tags KV operations; absent on puts.
const KV_OPERATION: &str = "KV-Operation";
/// Value of `KV-Operation` on a delete marker.
const KV_DELETE: &str = "DEL";
/// Value of `KV-Operation` on a put (the explicit form).
const KV_PUT: &str = "PUT";
/// Header the server sets on TTL-expiry and max-age markers.
const MARKER_REASON: &str = "Nats-Marker-Reason";
/// Attempts of a create that races with a marker re-creation.
const CREATE_ATTEMPTS: usize = 4;
/// Idle heartbeat of watch consumers.
const WATCH_IDLE_HEARTBEAT: Duration = Duration::from_secs(5);
/// Inactivity after which the server removes a watch consumer.
const WATCH_INACTIVE_THRESHOLD: Duration = Duration::from_secs(30);

/// Live state of one subject, read with a direct get.
enum Last {
    /// The last message is a put; carries the entry.
    Live(Entry),
    /// The last message is a delete or expiry marker at this revision.
    Marker(Revision),
    /// The subject has no message.
    Missing,
}

/// Result of one conditional or unconditional write.
enum Outcome {
    Stored(Revision),
    /// The `Nats-Expected-Last-Subject-Sequence` check failed.
    Mismatch,
}

/// A supported bucket of this tenant with its physical names.
struct Target {
    physical: String,
    stream: String,
    max_value: usize,
}

impl Target {
    /// Resolves `bucket` to its physical names and value limit. The bucket's
    /// backing stream must exist in this tenant, else the bucket is `Absent`.
    /// The value limit is the stream's `max_message_size`, which the server enforces too.
    async fn resolve(bus: &NatsBus, bucket: &Bucket) -> Result<Self, BusError> {
        bus.check_tenant(bucket.tenant())?;
        let physical = kv_bucket(bus.tenant(), bucket.name());
        let stream = kv_stream(&physical);
        let info = format!("$JS.API.STREAM.INFO.{stream}");
        let lookup = bus.js().get_stream(&stream);
        let handle = match guard(bus.denials(), |s: &str| s == info, lookup).await? {
            Ok(handle) => handle,
            // A missing bucket and one the identity may not read answer alike (0011-044).
            Err(err) => {
                return Err(match err.kind() {
                    GetStreamErrorKind::JetStream(js) if js.code() == 404 => {
                        BusError::PermissionDenied
                    }
                    _ => errors::unavailable(reason::STREAM_NOT_FOUND),
                });
            }
        };
        let max_value = handle.cached_info().config.max_message_size;
        Ok(Self {
            physical,
            stream,
            max_value: usize::try_from(max_value).unwrap_or(usize::MAX),
        })
    }

    fn subject(&self, key: &Key) -> String {
        kv_subject(&self.physical, key.as_str())
    }

    fn check_value(&self, value: &Bytes) -> Result<(), BusError> {
        if value.len() > self.max_value {
            return Err(BusError::PayloadTooLarge {
                limit: self.max_value,
            });
        }
        Ok(())
    }

    /// Reads the live state of `key` with a direct get on the backing stream.
    async fn last(&self, bus: &NatsBus, key: &Key) -> Result<Last, BusError> {
        let stream = bus
            .js()
            .get_stream(&self.stream)
            .await
            .map_err(|_| errors::unavailable(reason::STREAM_NOT_FOUND))?;
        let message = match stream.direct_get_last_for_subject(self.subject(key)).await {
            Ok(message) => message,
            Err(err) if matches!(err.kind(), DirectGetErrorKind::NotFound) => {
                return Ok(Last::Missing);
            }
            Err(_) => return Err(errors::unavailable(reason::JETSTREAM_API)),
        };
        let revision = Revision::new(message.sequence);
        if is_live(Some(&message.headers)) {
            Ok(Last::Live(Entry {
                key: key.clone(),
                value: message.payload,
                revision,
            }))
        } else {
            Ok(Last::Marker(revision))
        }
    }

    /// The error of a refused conditional write: a live entry is a conflict, anything
    /// else is absent.
    async fn refusal(&self, bus: &NatsBus, key: &Key) -> BusError {
        match self.last(bus, key).await {
            Ok(Last::Live(entry)) => BusError::Conflict {
                current_revision: entry.revision,
            },
            Ok(Last::Marker(_) | Last::Missing) => BusError::Absent,
            Err(err) => err,
        }
    }
}

/// Whether a stored KV message is a live put rather than a delete or expiry marker.
fn is_live(headers: Option<&HeaderMap>) -> bool {
    let Some(headers) = headers else {
        return true;
    };
    if headers.get(MARKER_REASON).is_some() {
        return false;
    }
    match headers.get(KV_OPERATION) {
        None => true,
        Some(operation) => operation.as_str() == KV_PUT,
    }
}

/// Converts a TTL into the `Nats-TTL` header value. Whole seconds only.
fn ttl_header(ttl: Duration) -> Result<String, BusError> {
    if ttl.is_zero() || ttl.subsec_nanos() != 0 {
        return Err(errors::unavailable(reason::INVALID_TTL));
    }
    Ok(format!("{}s", ttl.as_secs()))
}

/// Builds the headers of one KV write.
fn write_headers(ttl: Option<&str>, expected: Option<u64>, operation: Option<&str>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Some(ttl) = ttl {
        headers.insert(NATS_TTL, ttl);
    }
    if let Some(expected) = expected {
        let expected = expected.to_string();
        headers.insert(EXPECTED_LAST_SUBJECT_SEQ, expected.as_str());
    }
    if let Some(operation) = operation {
        headers.insert(KV_OPERATION, operation);
    }
    headers
}

/// Publishes one KV write and reports whether its expected sequence matched.
async fn write(
    bus: &NatsBus,
    subject: String,
    headers: HeaderMap,
    payload: Bytes,
) -> Result<Outcome, BusError> {
    match publish_durable(bus, &subject, headers, payload).await? {
        Ok(ack) => Ok(Outcome::Stored(Revision::new(ack.sequence))),
        Err(err) => classify(&err),
    }
}

fn classify(err: &PublishError) -> Result<Outcome, BusError> {
    if matches!(err.kind(), PublishErrorKind::WrongLastSequence) {
        Ok(Outcome::Mismatch)
    } else {
        Err(errors::js_publish(err))
    }
}

/// `kv_put`: unconditional write, with an optional TTL.
pub(crate) async fn put(
    bus: &NatsBus,
    bucket: &Bucket,
    key: &Key,
    value: Bytes,
    ttl: Option<Duration>,
) -> Result<Revision, BusError> {
    let target = Target::resolve(bus, bucket).await?;
    target.check_value(&value)?;
    let ttl = ttl.map(ttl_header).transpose()?;
    let headers = write_headers(ttl.as_deref(), None, None);
    match write(bus, target.subject(key), headers, value).await? {
        Outcome::Stored(revision) => Ok(revision),
        Outcome::Mismatch => Err(errors::unavailable(reason::PUBLISH_FAILED)),
    }
}

/// `kv_get`: the live entry of `key`, or `Absent`.
pub(crate) async fn get(bus: &NatsBus, bucket: &Bucket, key: &Key) -> Result<Entry, BusError> {
    let target = Target::resolve(bus, bucket).await?;
    match target.last(bus, key).await? {
        Last::Live(entry) => Ok(entry),
        Last::Marker(_) | Last::Missing => Err(BusError::Absent),
    }
}

/// `kv_create`: write only when no live entry exists. A marker (expired or deleted
/// key) is replaced with its revision as the expected sequence (0010-031, 0010-032).
pub(crate) async fn create(
    bus: &NatsBus,
    bucket: &Bucket,
    key: &Key,
    value: Bytes,
    ttl: Option<Duration>,
) -> Result<Revision, BusError> {
    let target = Target::resolve(bus, bucket).await?;
    target.check_value(&value)?;
    let ttl = ttl.map(ttl_header).transpose()?;
    let subject = target.subject(key);
    let mut expected = 0_u64;
    for _ in 0..CREATE_ATTEMPTS {
        let headers = write_headers(ttl.as_deref(), Some(expected), None);
        match write(bus, subject.clone(), headers, value.clone()).await? {
            Outcome::Stored(revision) => return Ok(revision),
            Outcome::Mismatch => match target.last(bus, key).await? {
                Last::Live(entry) => {
                    return Err(BusError::Conflict {
                        current_revision: entry.revision,
                    });
                }
                Last::Marker(revision) => expected = revision.get(),
                Last::Missing => expected = 0,
            },
        }
    }
    Err(errors::unavailable(reason::PUBLISH_FAILED))
}

/// `kv_update`: write only when the live entry is at `expected`.
pub(crate) async fn update(
    bus: &NatsBus,
    bucket: &Bucket,
    key: &Key,
    value: Bytes,
    expected: Revision,
    ttl: Option<Duration>,
) -> Result<Revision, BusError> {
    let target = Target::resolve(bus, bucket).await?;
    target.check_value(&value)?;
    let ttl = ttl.map(ttl_header).transpose()?;
    let headers = write_headers(ttl.as_deref(), Some(expected.get()), None);
    match write(bus, target.subject(key), headers, value).await? {
        Outcome::Stored(revision) => Ok(revision),
        Outcome::Mismatch => Err(target.refusal(bus, key).await),
    }
}

/// `kv_delete`: writes a delete marker, conditional on `expected` when given.
pub(crate) async fn delete(
    bus: &NatsBus,
    bucket: &Bucket,
    key: &Key,
    expected: Option<Revision>,
) -> Result<Revision, BusError> {
    let target = Target::resolve(bus, bucket).await?;
    let headers = write_headers(None, expected.map(Revision::get), Some(KV_DELETE));
    match write(bus, target.subject(key), headers, Bytes::new()).await? {
        Outcome::Stored(revision) => Ok(revision),
        Outcome::Mismatch => Err(target.refusal(bus, key).await),
    }
}

/// `kv_watch`: an ephemeral push consumer with `LastPerSubject` delivery over the
/// bucket. The snapshot ends when the consumer reports no pending messages (0010-034).
pub(crate) async fn watch(
    bus: &NatsBus,
    bucket: &Bucket,
    prefix: &KeyPrefix,
) -> Result<NatsWatch, BusError> {
    let target = Target::resolve(bus, bucket).await?;
    let stream = bus
        .js()
        .get_stream(&target.stream)
        .await
        .map_err(|_| errors::unavailable(reason::STREAM_NOT_FOUND))?;
    let config = push::Config {
        deliver_subject: bus.client().new_inbox(),
        deliver_policy: DeliverPolicy::LastPerSubject,
        ack_policy: AckPolicy::None,
        filter_subject: format!("$KV.{}.>", target.physical),
        replay_policy: ReplayPolicy::Instant,
        flow_control: true,
        idle_heartbeat: WATCH_IDLE_HEARTBEAT,
        inactive_threshold: WATCH_INACTIVE_THRESHOLD,
        ..Default::default()
    };
    let create = stream.create_consumer(config);
    let consumer = match guard(
        bus.denials(),
        |s: &str| s.starts_with("$JS.API.CONSUMER."),
        create,
    )
    .await?
    {
        Ok(consumer) => consumer,
        Err(_) => return Err(errors::unavailable(reason::JETSTREAM_API)),
    };
    let pending = consumer.cached_info().num_pending;
    let messages = consumer
        .messages()
        .await
        .map_err(|_| errors::unavailable(reason::CONSUMER_CLOSED))?;
    let mut ready = std::collections::VecDeque::new();
    if pending == 0 {
        ready.push_back(WatchEvent::SnapshotEnd);
    }
    Ok(NatsWatch {
        messages,
        subject_prefix: format!("$KV.{}.", target.physical),
        key_prefix: prefix.as_str().to_owned(),
        snapshot: Vec::new(),
        snapshot_open: pending > 0,
        ready,
    })
}

/// A watch over one bucket. Implements [`Watch`].
pub struct NatsWatch {
    messages: Messages,
    subject_prefix: String,
    key_prefix: String,
    /// Snapshot events, held until the snapshot ends so they can be sorted by key (0011-026).
    snapshot: Vec<WatchEvent>,
    /// True until the consumer reports no pending snapshot messages.
    snapshot_open: bool,
    /// Events ready to return, in order; a `SnapshotEnd` follows the snapshot.
    ready: std::collections::VecDeque<WatchEvent>,
}

impl fmt::Debug for NatsWatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NatsWatch").finish_non_exhaustive()
    }
}

impl NatsWatch {
    /// Maps one stored message to an event, or `None` when it is outside the prefix.
    fn event(
        &self,
        subject: &str,
        headers: Option<&HeaderMap>,
        value: Bytes,
        revision: Revision,
    ) -> Option<WatchEvent> {
        let key = Key::parse(subject.strip_prefix(&self.subject_prefix)?).ok()?;
        if !key.as_str().starts_with(&self.key_prefix) {
            return None;
        }
        Some(if is_live(headers) {
            WatchEvent::Put(Entry {
                key,
                value,
                revision,
            })
        } else {
            WatchEvent::Delete { key, revision }
        })
    }
}

/// The key a watch event sorts by. Only snapshot events are compared.
fn event_key(event: &WatchEvent) -> &str {
    match event {
        WatchEvent::Put(entry) => entry.key.as_str(),
        WatchEvent::Delete { key, .. } => key.as_str(),
        WatchEvent::SnapshotEnd => "",
    }
}

impl Watch for NatsWatch {
    async fn next(&mut self) -> Result<WatchEvent, BusError> {
        loop {
            if let Some(event) = self.ready.pop_front() {
                return Ok(event);
            }
            let closed = || errors::unavailable(reason::WATCH_CLOSED);
            let message = self
                .messages
                .next()
                .await
                .ok_or_else(closed)?
                .map_err(|_| closed())?;
            let info = message.info().map_err(|_| closed())?;
            let revision = Revision::new(info.stream_sequence);
            let ends = self.snapshot_open && info.pending == 0;
            let (message, _acker) = message.split();
            let event = self.event(
                message.subject.as_str(),
                message.headers.as_ref(),
                message.payload,
                revision,
            );
            if !self.snapshot_open {
                if let Some(event) = event {
                    return Ok(event);
                }
                continue;
            }
            self.snapshot.extend(event);
            if ends {
                self.snapshot_open = false;
                self.snapshot.sort_by(|a, b| event_key(a).cmp(event_key(b)));
                self.ready.extend(self.snapshot.drain(..));
                self.ready.push_back(WatchEvent::SnapshotEnd);
            }
        }
    }
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, *value);
        }
        map
    }

    #[test]
    fn req_0010_031_message_without_headers_is_live() {
        assert!(is_live(None));
    }

    #[test]
    fn req_0010_031_put_operation_is_live() {
        let map = headers(&[(KV_OPERATION, KV_PUT)]);
        assert!(is_live(Some(&map)));
    }

    #[test]
    fn req_0010_031_delete_operation_is_a_marker() {
        let map = headers(&[(KV_OPERATION, KV_DELETE)]);
        assert!(!is_live(Some(&map)));
    }

    #[test]
    fn req_0010_031_ttl_expiry_marker_reason_is_a_marker() {
        let map = headers(&[(MARKER_REASON, "MaxAge")]);
        assert!(!is_live(Some(&map)));
    }

    #[test]
    fn req_0010_031_ttl_header_is_whole_seconds() {
        let ttl = ttl_header(Duration::from_secs(30)).expect("whole seconds");
        assert_eq!(ttl, "30s");
    }

    #[test]
    fn req_0010_031_zero_or_fractional_ttl_is_refused() {
        assert!(ttl_header(Duration::ZERO).is_err());
        assert!(ttl_header(Duration::from_millis(1500)).is_err());
    }
}
