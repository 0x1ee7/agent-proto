//! The in-memory binding: [`MemBus`] implements [`bus_api::bus::Bus`] over the
//! shared state. Operations check capabilities, then the caller's gate, then
//! grants, then limits, in that order (RFC-0011 [0011-030..044]).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use a2a_types::ids::TenantId;
use bus_api::bus::{Bus, Consumer, RequestStream, Watch};
use bus_api::capabilities::Capabilities;
use bus_api::error::BusError;
use bus_api::headers::Headers;
use bus_api::message::{
    Delivery, Entry, PublishAck, Reply, ReplyHandle, ReplySink, Request, Settlement, StartPosition,
    WatchEvent,
};
use bus_api::name::{
    Bucket, ConsumerName, Key, KeyPrefix, MsgId, Principal, StreamName, Subject, SubjectPattern,
};
use bus_api::value::{Digest, Revision, StreamSeq};
use bytes::Bytes;

use crate::config::{ConfigError, MemConfig};
use crate::rng::SplitMix64;
use crate::state::{
    BucketState, CLOSED, CONSUMER_CRASHED, ConsumerKey, ConsumerState, Dedupe, Faults, Inner, Pre,
    Queued, RESTARTED, RETENTION_EXCEEDED, ReplyState, SEQ_NOT_YET_ASSIGNED, ServerState, Settle,
    Shared, Slot, StreamState, digest_of, wait_for,
};

/// Deterministic in-process bus. Build one with [`MemBus::new`]; drive
/// simulation through [`MemBus::control`].
pub struct MemBus {
    shared: Arc<Shared>,
}

impl fmt::Debug for MemBus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemBus").finish_non_exhaustive()
    }
}

/// Receives requests for one served pattern.
#[derive(Debug)]
pub struct MemRequests {
    shared: Arc<Shared>,
    server: u64,
    epoch: u64,
}

/// A durable consumer handle.
#[derive(Debug)]
pub struct MemConsumer {
    shared: Arc<Shared>,
    key: ConsumerKey,
    epoch: u64,
    consumer_epoch: u64,
}

/// A KV watch: a snapshot followed by live changes of one bucket.
#[derive(Debug)]
pub struct MemWatch {
    shared: Arc<Shared>,
    bucket: Bucket,
    prefix: KeyPrefix,
    /// Index of the next unread entry of the bucket log.
    pos: usize,
    /// Snapshot events not yet returned.
    pending: VecDeque<WatchEvent>,
    epoch: u64,
}

#[derive(Debug)]
struct MemSettlement {
    shared: Arc<Shared>,
    key: ConsumerKey,
    seq: u64,
    epoch: u64,
    consumer_epoch: u64,
}

#[derive(Debug)]
struct MemReplySink {
    shared: Arc<Shared>,
    req_id: u64,
    epoch: u64,
    replier: Principal,
    requester: Principal,
}

impl MemBus {
    /// Builds a bus from a configuration.
    ///
    /// # Errors
    /// Returns the first [`ConfigError`] found by [`MemConfig::validate`].
    pub fn new(config: MemConfig) -> Result<Self, ConfigError> {
        config.validate()?;
        let buckets: BTreeMap<Bucket, BucketState> = config
            .kv_buckets
            .iter()
            .map(|b| (b.clone(), BucketState::default()))
            .collect();
        let streams: BTreeMap<StreamName, StreamState> = config
            .streams
            .iter()
            .map(|s| {
                (
                    s.name.clone(),
                    StreamState {
                        subjects: s.subjects.clone(),
                        retention: s.retention_messages.map(NonZeroUsize::get),
                        first_seq: 1,
                        next_seq: 1,
                        messages: VecDeque::new(),
                    },
                )
            })
            .collect();
        let grants = config
            .grants
            .iter()
            .map(|g| (g.principal.clone(), g.clone()))
            .collect();
        let inner = Inner {
            seed: config.seed,
            now: Duration::ZERO,
            epoch: 0,
            closed: false,
            rng: SplitMix64::new(config.seed),
            faults: Faults::default(),
            trace: Vec::new(),
            ack_wait: config.ack_wait,
            overload_retry_after: config.overload_retry_after,
            blob_tenant_quota: config.blob_tenant_quota,
            dedupe_window: config.capabilities.dedupe_window,
            max_pending: config.capabilities.max_pending,
            no_responder_signal: config.capabilities.no_responder_signal,
            grants,
            streams,
            consumers: BTreeMap::new(),
            dedupe: BTreeMap::new(),
            servers: Vec::new(),
            next_server: 0,
            replies: BTreeMap::new(),
            next_request: 0,
            buckets,
            blobs: HashMap::new(),
            blob_usage: HashMap::new(),
        };
        Ok(Self {
            shared: Arc::new(Shared::new(config.capabilities, inner)),
        })
    }

    /// Simulation control handle sharing this bus's state.
    #[must_use]
    pub fn control(&self) -> crate::control::MemControl {
        crate::control::MemControl::new(Arc::clone(&self.shared))
    }

    fn publish_now(
        &self,
        who: &Principal,
        subject: &Subject,
        headers: &Headers,
        payload: Bytes,
    ) -> Result<PublishAck, BusError> {
        let caps = &self.shared.caps;
        check_headers(caps, headers)?;
        let ack = {
            let mut inner = self.shared.lock();
            let captured = inner.captures(subject);
            inner.gate(who, captured.is_some())?;
            if !inner.may_publish(who, subject) {
                return Err(BusError::PermissionDenied);
            }
            check_size(payload.len(), caps.max_message_size)?;
            match captured {
                Some(stream) => {
                    let seq = inner.store(&stream, subject, headers, payload, who)?;
                    PublishAck {
                        stream: Some(stream),
                        seq: Some(StreamSeq::new(seq)),
                    }
                }
                None => PublishAck {
                    stream: None,
                    seq: None,
                },
            }
        };
        self.shared.wake();
        Ok(ack)
    }

    /// Registers the request before routing it, so a reply can never race
    /// past its own state. Returns the request id and its deadline.
    fn begin_request(
        &self,
        who: &Principal,
        subject: &Subject,
        headers: &Headers,
        payload: &Bytes,
        timeout: Duration,
    ) -> Result<(u64, Duration), BusError> {
        let caps = &self.shared.caps;
        need(caps.request_reply, "request_reply")?;
        check_headers(caps, headers)?;
        let started = {
            let mut inner = self.shared.lock();
            inner.gate(who, false)?;
            if !inner.may_publish(who, subject) {
                return Err(BusError::PermissionDenied);
            }
            check_size(payload.len(), caps.max_message_size)?;
            let req_id = inner.next_request;
            inner.next_request += 1;
            let deadline = inner.now.saturating_add(timeout);
            inner.replies.insert(req_id, ReplyState::Pending);
            if let Err(e) = inner.route(who, req_id, subject, headers, payload) {
                inner.replies.remove(&req_id);
                return Err(e);
            }
            (req_id, deadline)
        };
        self.shared.wake();
        Ok(started)
    }

    fn open_server(
        &self,
        who: &Principal,
        pattern: &SubjectPattern,
    ) -> Result<MemRequests, BusError> {
        need(self.shared.caps.request_reply, "request_reply")?;
        let mut inner = self.shared.lock();
        inner.gate(who, false)?;
        if !inner.grant(who)?.serve.contains(pattern) {
            return Err(BusError::PermissionDenied);
        }
        let server = inner.next_server;
        inner.next_server += 1;
        inner.servers.push(ServerState {
            id: server,
            pattern: pattern.clone(),
            principal: who.clone(),
            queue: VecDeque::new(),
        });
        let epoch = inner.epoch;
        drop(inner);
        Ok(MemRequests {
            shared: Arc::clone(&self.shared),
            server,
            epoch,
        })
    }

    fn append_now(
        &self,
        who: &Principal,
        stream: &StreamName,
        subject: &Subject,
        headers: &Headers,
        payload: Bytes,
        msg_id: &MsgId,
    ) -> Result<PublishAck, BusError> {
        let caps = &self.shared.caps;
        need(caps.durable_streams, "durable_streams")?;
        check_headers(caps, headers)?;
        let mut inner = self.shared.lock();
        inner.gate(who, true)?;
        if !inner.grant(who)?.streams.contains(stream) {
            return Err(BusError::PermissionDenied);
        }
        let captures = inner
            .streams
            .get(stream)
            .is_some_and(|st| st.subjects.iter().any(|p| p.matches(subject)));
        if !captures {
            return Err(BusError::Absent);
        }
        check_size(payload.len(), caps.max_message_size)?;
        let key = (stream.clone(), msg_id.clone());
        let now = inner.now;
        let window = inner.dedupe_window;
        if let Some(prior) = inner.dedupe.get(&key).copied() {
            if now.saturating_sub(prior.stored_at) < window {
                return Err(BusError::Duplicate {
                    original_seq: StreamSeq::new(prior.seq),
                });
            }
            inner.dedupe.remove(&key);
        }
        let seq = inner.store(stream, subject, headers, payload, who)?;
        inner.dedupe.insert(
            key,
            Dedupe {
                seq,
                stored_at: now,
            },
        );
        drop(inner);
        self.shared.wake();
        Ok(PublishAck {
            stream: Some(stream.clone()),
            seq: Some(StreamSeq::new(seq)),
        })
    }

    fn open_consumer(
        &self,
        who: &Principal,
        stream: &StreamName,
        consumer: &ConsumerName,
        filter: &SubjectPattern,
        start: StartPosition,
    ) -> Result<MemConsumer, BusError> {
        let caps = &self.shared.caps;
        need(caps.durable_streams, "durable_streams")?;
        if matches!(start, StartPosition::FromSeq(_)) {
            need(caps.replay_from_seq, "replay_from_seq")?;
        }
        let mut inner = self.shared.lock();
        inner.gate(who, true)?;
        if !inner.grant(who)?.streams.contains(stream) {
            return Err(BusError::PermissionDenied);
        }
        let key = (stream.clone(), consumer.clone());
        if let Some(existing) = inner.consumers.get(&key) {
            let consumer_epoch = existing.epoch;
            let epoch = inner.epoch;
            return Ok(MemConsumer {
                shared: Arc::clone(&self.shared),
                key,
                epoch,
                consumer_epoch,
            });
        }
        let st = inner.streams.get(stream).ok_or(BusError::Absent)?;
        let cursor = match start {
            StartPosition::FromSeq(seq) => {
                let n = seq.get();
                if n < st.first_seq {
                    return Err(BusError::Unavailable {
                        reason: RETENTION_EXCEEDED,
                    });
                }
                if n > st.next_seq {
                    return Err(BusError::Unavailable {
                        reason: SEQ_NOT_YET_ASSIGNED,
                    });
                }
                n
            }
            StartPosition::FromFirst => st.first_seq,
            StartPosition::NewOnly => st.next_seq,
        };
        inner.consumers.insert(
            key.clone(),
            ConsumerState {
                filter: filter.clone(),
                cursor,
                track: BTreeMap::new(),
                epoch: 0,
            },
        );
        let epoch = inner.epoch;
        drop(inner);
        Ok(MemConsumer {
            shared: Arc::clone(&self.shared),
            key,
            epoch,
            consumer_epoch: 0,
        })
    }

    fn kv_write(
        &self,
        who: &Principal,
        bucket: &Bucket,
        key: &Key,
        value: Bytes,
        ttl: Option<Duration>,
        pre: Pre,
    ) -> Result<Revision, BusError> {
        let caps = &self.shared.caps;
        need(caps.kv, "kv")?;
        let rev = {
            let mut inner = self.shared.lock();
            inner.gate(who, true)?;
            inner.kv_check(who, bucket)?;
            check_size(value.len(), caps.kv_max_value_size)?;
            if ttl.is_some() {
                need(caps.kv_ttl, "kv_ttl")?;
            }
            inner.kv_put(bucket, key, value, ttl, pre)?
        };
        self.shared.wake();
        Ok(Revision::new(rev))
    }

    fn open_watch(
        &self,
        who: &Principal,
        bucket: &Bucket,
        prefix: &KeyPrefix,
    ) -> Result<MemWatch, BusError> {
        need(self.shared.caps.kv, "kv")?;
        let mut inner = self.shared.lock();
        inner.gate(who, true)?;
        inner.kv_check(who, bucket)?;
        let now = inner.now;
        let epoch = inner.epoch;
        let b = inner.buckets.get_mut(bucket).ok_or(BusError::Internal)?;
        b.sweep(now);
        let mut pending: VecDeque<WatchEvent> = b
            .slots
            .iter()
            .filter_map(|(key, slot)| match slot {
                Slot::Live {
                    value, revision, ..
                } if key.as_str().starts_with(prefix.as_str()) => Some(WatchEvent::Put(Entry {
                    key: key.clone(),
                    value: value.clone(),
                    revision: Revision::new(*revision),
                })),
                _ => None,
            })
            .collect();
        pending.push_back(WatchEvent::SnapshotEnd);
        let pos = b.log.len();
        drop(inner);
        Ok(MemWatch {
            shared: Arc::clone(&self.shared),
            bucket: bucket.clone(),
            prefix: prefix.clone(),
            pos,
            pending,
            epoch,
        })
    }
}

impl Bus for MemBus {
    type Requests = MemRequests;
    type Consumer = MemConsumer;
    type Watch = MemWatch;

    fn capabilities(&self) -> &Capabilities {
        &self.shared.caps
    }

    async fn publish(
        &self,
        who: &Principal,
        subject: &Subject,
        headers: &Headers,
        payload: Bytes,
    ) -> Result<PublishAck, BusError> {
        self.publish_now(who, subject, headers, payload)
    }

    async fn request(
        &self,
        who: &Principal,
        subject: &Subject,
        headers: &Headers,
        payload: Bytes,
        timeout: Duration,
    ) -> Result<Reply, BusError> {
        let (req_id, deadline) = self.begin_request(who, subject, headers, &payload, timeout)?;
        wait_for(&self.shared, move |inner| {
            inner.poll_reply(req_id, deadline)
        })
        .await
    }

    async fn serve(
        &self,
        who: &Principal,
        pattern: &SubjectPattern,
    ) -> Result<Self::Requests, BusError> {
        self.open_server(who, pattern)
    }

    async fn append(
        &self,
        who: &Principal,
        stream: &StreamName,
        subject: &Subject,
        headers: &Headers,
        payload: Bytes,
        msg_id: &MsgId,
    ) -> Result<PublishAck, BusError> {
        self.append_now(who, stream, subject, headers, payload, msg_id)
    }

    async fn consume(
        &self,
        who: &Principal,
        stream: &StreamName,
        consumer: &ConsumerName,
        filter: &SubjectPattern,
        start: StartPosition,
    ) -> Result<Self::Consumer, BusError> {
        self.open_consumer(who, stream, consumer, filter, start)
    }

    async fn kv_put(
        &self,
        who: &Principal,
        bucket: &Bucket,
        key: &Key,
        value: Bytes,
        ttl: Option<Duration>,
    ) -> Result<Revision, BusError> {
        self.kv_write(who, bucket, key, value, ttl, Pre::Any)
    }

    async fn kv_get(&self, who: &Principal, bucket: &Bucket, key: &Key) -> Result<Entry, BusError> {
        need(self.shared.caps.kv, "kv")?;
        let mut inner = self.shared.lock();
        inner.gate(who, true)?;
        inner.kv_check(who, bucket)?;
        let now = inner.now;
        let b = inner.buckets.get_mut(bucket).ok_or(BusError::Internal)?;
        b.sweep(now);
        match b.slots.get(key) {
            Some(Slot::Live {
                value, revision, ..
            }) => Ok(Entry {
                key: key.clone(),
                value: value.clone(),
                revision: Revision::new(*revision),
            }),
            _ => Err(BusError::Absent),
        }
    }

    async fn kv_create(
        &self,
        who: &Principal,
        bucket: &Bucket,
        key: &Key,
        value: Bytes,
        ttl: Option<Duration>,
    ) -> Result<Revision, BusError> {
        self.kv_write(who, bucket, key, value, ttl, Pre::Create)
    }

    async fn kv_update(
        &self,
        who: &Principal,
        bucket: &Bucket,
        key: &Key,
        value: Bytes,
        expected: Revision,
        ttl: Option<Duration>,
    ) -> Result<Revision, BusError> {
        self.kv_write(who, bucket, key, value, ttl, Pre::Update(expected.get()))
    }

    async fn kv_delete(
        &self,
        who: &Principal,
        bucket: &Bucket,
        key: &Key,
        expected: Option<Revision>,
    ) -> Result<Revision, BusError> {
        need(self.shared.caps.kv, "kv")?;
        let rev = {
            let mut inner = self.shared.lock();
            inner.gate(who, true)?;
            inner.kv_check(who, bucket)?;
            inner.kv_delete(bucket, key, expected.map(Revision::get))?
        };
        self.shared.wake();
        Ok(Revision::new(rev))
    }

    async fn kv_watch(
        &self,
        who: &Principal,
        bucket: &Bucket,
        prefix: &KeyPrefix,
    ) -> Result<Self::Watch, BusError> {
        self.open_watch(who, bucket, prefix)
    }

    async fn blob_put(
        &self,
        who: &Principal,
        tenant: &TenantId,
        bytes: Bytes,
    ) -> Result<Digest, BusError> {
        let caps = &self.shared.caps;
        need(caps.blob_store, "blob_store")?;
        let mut inner = self.shared.lock();
        inner.gate(who, true)?;
        if inner.grant(who)?.tenant != *tenant {
            return Err(BusError::PermissionDenied);
        }
        check_size(bytes.len(), caps.blob_max_size)?;
        let digest = digest_of(&bytes);
        let key = (tenant.clone(), digest);
        if inner.blobs.contains_key(&key) {
            return Ok(digest);
        }
        let used = inner.blob_usage.get(tenant).copied().unwrap_or(0);
        let next = used.saturating_add(bytes.len());
        if next > inner.blob_tenant_quota {
            return Err(BusError::Overloaded {
                retry_after: inner.overload_retry_after,
            });
        }
        inner.blob_usage.insert(tenant.clone(), next);
        inner.blobs.insert(key, bytes);
        Ok(digest)
    }

    async fn blob_get(
        &self,
        who: &Principal,
        tenant: &TenantId,
        digest: &Digest,
    ) -> Result<Bytes, BusError> {
        need(self.shared.caps.blob_store, "blob_store")?;
        let inner = self.shared.lock();
        inner.gate(who, true)?;
        if inner.grant(who)?.tenant != *tenant {
            return Err(BusError::PermissionDenied);
        }
        let bytes = inner
            .blobs
            .get(&(tenant.clone(), *digest))
            .cloned()
            .ok_or(BusError::Absent)?;
        if digest_of(&bytes) != *digest {
            return Err(BusError::Corrupt { digest: *digest });
        }
        Ok(bytes)
    }

    async fn close(&self) -> Result<(), BusError> {
        self.shared.lock().closed = true;
        self.shared.wake();
        Ok(())
    }
}

impl RequestStream for MemRequests {
    async fn next(&mut self) -> Result<Request, BusError> {
        let shared = &self.shared;
        let server = self.server;
        let epoch = self.epoch;
        wait_for(shared, move |inner| {
            if inner.closed {
                return Some(Err(BusError::Unavailable { reason: CLOSED }));
            }
            if inner.epoch != epoch {
                return Some(Err(BusError::Unavailable { reason: RESTARTED }));
            }
            let Some(srv) = inner.servers.iter_mut().find(|s| s.id == server) else {
                return Some(Err(BusError::Internal));
            };
            let replier = srv.principal.clone();
            let queued: Queued = srv.queue.pop_front()?;
            Some(Ok(Request {
                subject: queued.subject,
                principal: Some(queued.requester.clone()),
                headers: queued.headers,
                payload: queued.payload,
                reply: ReplyHandle::new(Box::new(MemReplySink {
                    shared: Arc::clone(shared),
                    req_id: queued.req_id,
                    epoch,
                    replier,
                    requester: queued.requester,
                })),
            }))
        })
        .await
    }
}

impl Drop for MemRequests {
    fn drop(&mut self) {
        self.shared.lock().servers.retain(|s| s.id != self.server);
        self.shared.wake();
    }
}

impl Consumer for MemConsumer {
    async fn next(&mut self) -> Result<Delivery, BusError> {
        let shared = &self.shared;
        let key = self.key.clone();
        let epoch = self.epoch;
        let consumer_epoch = self.consumer_epoch;
        wait_for(shared, move |inner| {
            if let Err(e) = check_consumer(inner, &key, epoch, consumer_epoch) {
                return Some(Err(e));
            }
            next_delivery(inner, shared, &key, epoch, consumer_epoch)
        })
        .await
    }
}

impl Watch for MemWatch {
    async fn next(&mut self) -> Result<WatchEvent, BusError> {
        let shared = &self.shared;
        let epoch = self.epoch;
        let bucket = &self.bucket;
        let prefix = &self.prefix;
        let pos = &mut self.pos;
        let pending = &mut self.pending;
        wait_for(shared, move |inner| {
            if inner.closed {
                return Some(Err(BusError::Unavailable { reason: CLOSED }));
            }
            if inner.epoch != epoch {
                return Some(Err(BusError::Unavailable { reason: RESTARTED }));
            }
            if let Some(event) = pending.pop_front() {
                return Some(Ok(event));
            }
            let now = inner.now;
            let Some(b) = inner.buckets.get_mut(bucket) else {
                return Some(Err(BusError::Internal));
            };
            b.sweep(now);
            while *pos < b.log.len() {
                let event = &b.log[*pos];
                *pos += 1;
                if event.key.as_str().starts_with(prefix.as_str()) {
                    return Some(Ok(event.to_watch_event()));
                }
            }
            None
        })
        .await
    }
}

impl MemSettlement {
    fn apply(&self, action: Settle) -> Result<(), BusError> {
        {
            let mut inner = self.shared.lock();
            check_consumer(&inner, &self.key, self.epoch, self.consumer_epoch)?;
            inner.settle(&self.key, self.seq, action);
        }
        self.shared.wake();
        Ok(())
    }
}

impl Settlement for MemSettlement {
    fn ack(self: Box<Self>) -> Result<(), BusError> {
        self.apply(Settle::Done)
    }

    fn nak(self: Box<Self>, delay: Duration) -> Result<(), BusError> {
        self.apply(Settle::Nak(delay))
    }

    fn term(self: Box<Self>) -> Result<(), BusError> {
        self.apply(Settle::Done)
    }

    fn in_progress(&self) -> Result<(), BusError> {
        self.apply(Settle::InProgress)
    }
}

impl ReplySink for MemReplySink {
    fn send(self: Box<Self>, reply: Reply) -> Result<(), BusError> {
        {
            let mut inner = self.shared.lock();
            if inner.closed {
                return Err(BusError::Unavailable { reason: CLOSED });
            }
            if inner.epoch != self.epoch {
                return Err(BusError::Unavailable { reason: RESTARTED });
            }
            if inner.crosses(&self.replier, &self.requester) {
                inner
                    .trace
                    .push(format!("partition reply req={}", self.req_id));
                return Ok(());
            }
            let drop_p = inner.faults.drop;
            if inner.roll(drop_p) {
                inner.trace.push(format!("drop reply req={}", self.req_id));
                return Ok(());
            }
            // A reply for a requester that already gave up finds no pending
            // state and is discarded.
            if let Some(state) = inner.replies.get_mut(&self.req_id) {
                if matches!(state, ReplyState::Pending) {
                    *state = ReplyState::Done(reply);
                }
            }
        }
        self.shared.wake();
        Ok(())
    }
}

/// Fails when the consumer handle no longer refers to live state.
fn check_consumer(
    inner: &Inner,
    key: &ConsumerKey,
    epoch: u64,
    consumer_epoch: u64,
) -> Result<(), BusError> {
    if inner.closed {
        return Err(BusError::Unavailable { reason: CLOSED });
    }
    if inner.epoch != epoch {
        return Err(BusError::Unavailable { reason: RESTARTED });
    }
    let cons = inner.consumers.get(key).ok_or(BusError::Absent)?;
    if cons.epoch != consumer_epoch {
        return Err(BusError::Unavailable {
            reason: CONSUMER_CRASHED,
        });
    }
    Ok(())
}

/// One step of [`MemConsumer::next`]: `None` means nothing is deliverable yet.
fn next_delivery(
    inner: &mut Inner,
    shared: &Arc<Shared>,
    key: &ConsumerKey,
    epoch: u64,
    consumer_epoch: u64,
) -> Option<Result<Delivery, BusError>> {
    let first_seq = inner.streams.get(&key.0)?.first_seq;
    if inner.consumers.get(key)?.cursor < first_seq {
        return Some(Err(BusError::Unavailable {
            reason: RETENTION_EXCEEDED,
        }));
    }
    let (seq, attempt) = inner.select_delivery(key)?;
    let Some(msg) = inner.streams.get(&key.0).and_then(|st| st.get(seq)) else {
        return Some(Err(BusError::Internal));
    };
    Some(Ok(Delivery::new(
        msg.subject.clone(),
        Some(msg.publisher.clone()),
        msg.headers.clone(),
        msg.payload.clone(),
        StreamSeq::new(seq),
        attempt,
        Box::new(MemSettlement {
            shared: Arc::clone(shared),
            key: key.clone(),
            seq,
            epoch,
            consumer_epoch,
        }),
    )))
}

fn need(flag: bool, capability: &'static str) -> Result<(), BusError> {
    if flag {
        Ok(())
    } else {
        Err(BusError::Unsupported { capability })
    }
}

fn check_headers(caps: &Capabilities, headers: &Headers) -> Result<(), BusError> {
    if headers.is_empty() {
        Ok(())
    } else {
        need(caps.headers, "headers")
    }
}

fn check_size(len: usize, limit: usize) -> Result<(), BusError> {
    if len > limit {
        Err(BusError::PayloadTooLarge { limit })
    } else {
        Ok(())
    }
}
