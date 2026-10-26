//! Shared state of the in-memory bus. One mutex guards every structure.
//! Waiters sleep on one `Notify`; every state change and clock advance wakes
//! them, and each waiter re-checks its condition under the lock.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use a2a_types::ids::TenantId;
use bus_api::capabilities::Capabilities;
use bus_api::control::DelayDist;
use bus_api::error::BusError;
use bus_api::headers::Headers;
use bus_api::message::{Entry, Reply, WatchEvent};
use bus_api::name::{
    Bucket, ConsumerName, Key, MsgId, Principal, StreamName, Subject, SubjectPattern,
};
use bus_api::value::Digest;
use bytes::Bytes;
use sha2::Digest as _;
use sha2::Sha256;
use tokio::sync::Notify;

use crate::config::Grant;
use crate::rng::SplitMix64;

pub(crate) const CLOSED: &str = "CLOSED";
pub(crate) const RESTARTED: &str = "RESTARTED";
pub(crate) const PARTITIONED: &str = "PARTITIONED";
pub(crate) const CONSUMER_CRASHED: &str = "CONSUMER_CRASHED";
pub(crate) const RETENTION_EXCEEDED: &str = "RETENTION_EXCEEDED";
pub(crate) const SEQ_NOT_YET_ASSIGNED: &str = "SEQ_NOT_YET_ASSIGNED";

/// Key of one durable consumer: its stream and its name.
pub(crate) type ConsumerKey = (StreamName, ConsumerName);

pub(crate) struct Shared {
    /// Declared capabilities. Immutable, so readable without the lock.
    pub(crate) caps: Capabilities,
    pub(crate) inner: Mutex<Inner>,
    pub(crate) notify: Notify,
}

impl fmt::Debug for Shared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Shared").finish_non_exhaustive()
    }
}

impl Shared {
    pub(crate) fn new(caps: Capabilities, inner: Inner) -> Self {
        Self {
            caps,
            inner: Mutex::new(inner),
            notify: Notify::new(),
        }
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn wake(&self) {
        self.notify.notify_waiters();
    }
}

/// Waits until `step` returns a result. `step` runs under the lock; `None`
/// means the condition is not met yet. The notification is registered before
/// the check, so a wake between check and await is not lost.
pub(crate) async fn wait_for<T, F>(shared: &Shared, mut step: F) -> Result<T, BusError>
where
    T: Send,
    F: FnMut(&mut Inner) -> Option<Result<T, BusError>> + Send,
{
    loop {
        let notified = shared.notify.notified();
        let mut notified = std::pin::pin!(notified);
        notified.as_mut().enable();
        {
            let mut inner = shared.lock();
            if let Some(result) = step(&mut inner) {
                return result;
            }
        }
        notified.await;
    }
}

/// SHA-256 digest of blob content.
pub(crate) fn digest_of(bytes: &[u8]) -> Digest {
    let mut out = [0_u8; 32];
    out.copy_from_slice(&Sha256::digest(bytes));
    Digest::from_bytes(out)
}

#[derive(Default)]
pub(crate) struct Faults {
    pub(crate) drop: Option<f64>,
    pub(crate) duplicate: Option<f64>,
    pub(crate) reorder: Option<NonZeroUsize>,
    pub(crate) delay: Option<DelayDist>,
    pub(crate) partition: Option<(Vec<Principal>, Vec<Principal>)>,
}

/// Which precondition a KV write checks.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Pre {
    Any,
    Create,
    Update(u64),
}

/// How a delivery is settled.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Settle {
    /// Acknowledge or terminate: no more redelivery.
    Done,
    /// Redeliver after the given delay.
    Nak(Duration),
    /// Extend the in-flight window by the consumer ack wait.
    InProgress,
}

pub(crate) struct Inner {
    pub(crate) seed: u64,
    pub(crate) now: Duration,
    /// Bumped by a simulated restart. Handles from an older epoch fail.
    pub(crate) epoch: u64,
    pub(crate) closed: bool,
    pub(crate) rng: SplitMix64,
    pub(crate) faults: Faults,
    pub(crate) trace: Vec<String>,
    pub(crate) ack_wait: Duration,
    pub(crate) overload_retry_after: Duration,
    pub(crate) blob_tenant_quota: usize,
    pub(crate) dedupe_window: Duration,
    pub(crate) max_pending: usize,
    pub(crate) no_responder_signal: bool,
    pub(crate) grants: BTreeMap<Principal, Grant>,
    pub(crate) streams: BTreeMap<StreamName, StreamState>,
    pub(crate) consumers: BTreeMap<ConsumerKey, ConsumerState>,
    pub(crate) dedupe: BTreeMap<(StreamName, MsgId), Dedupe>,
    pub(crate) servers: Vec<ServerState>,
    pub(crate) next_server: u64,
    pub(crate) replies: BTreeMap<u64, ReplyState>,
    pub(crate) next_request: u64,
    pub(crate) buckets: BTreeMap<Bucket, BucketState>,
    pub(crate) blobs: HashMap<(TenantId, Digest), Bytes>,
    pub(crate) blob_usage: HashMap<TenantId, usize>,
}

pub(crate) struct StreamState {
    pub(crate) subjects: Vec<SubjectPattern>,
    pub(crate) retention: Option<usize>,
    /// Sequence of the oldest retained message.
    pub(crate) first_seq: u64,
    /// Sequence the next append receives.
    pub(crate) next_seq: u64,
    pub(crate) messages: VecDeque<StoredMsg>,
}

pub(crate) struct StoredMsg {
    pub(crate) subject: Subject,
    pub(crate) headers: Headers,
    pub(crate) payload: Bytes,
    /// Principal that published the message (RFC-0002 [0002-011]).
    pub(crate) publisher: Principal,
    /// Earliest virtual time the message may be delivered (delay fault).
    pub(crate) visible_at: Duration,
}

impl StreamState {
    pub(crate) fn get(&self, seq: u64) -> Option<&StoredMsg> {
        if seq < self.first_seq || seq >= self.next_seq {
            return None;
        }
        let idx = usize::try_from(seq - self.first_seq).ok()?;
        self.messages.get(idx)
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Dedupe {
    pub(crate) seq: u64,
    pub(crate) stored_at: Duration,
}

pub(crate) struct ConsumerState {
    /// Fixed at creation; a later `consume` with another filter resumes this one.
    pub(crate) filter: SubjectPattern,
    /// Lowest sequence not yet passed. Everything below is settled or unmatched.
    pub(crate) cursor: u64,
    /// Handed-out messages at or after the cursor.
    pub(crate) track: BTreeMap<u64, Track>,
    /// Bumped by a simulated consumer crash.
    pub(crate) epoch: u64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Track {
    pub(crate) visible_at: Duration,
    /// Number of the last handed-out attempt (1 for the first delivery).
    pub(crate) attempt: u32,
    pub(crate) settled: bool,
}

pub(crate) struct ServerState {
    pub(crate) id: u64,
    pub(crate) pattern: SubjectPattern,
    pub(crate) principal: Principal,
    pub(crate) queue: VecDeque<Queued>,
}
#[derive(Clone, Debug)]
pub(crate) struct Queued {
    pub(crate) req_id: u64,
    pub(crate) requester: Principal,
    pub(crate) subject: Subject,
    pub(crate) headers: Headers,
    pub(crate) payload: Bytes,
}

pub(crate) enum ReplyState {
    Pending,
    Done(Reply),
    Failed(&'static str),
}

#[derive(Default)]
pub(crate) struct BucketState {
    pub(crate) rev: u64,
    pub(crate) slots: BTreeMap<Key, Slot>,
    /// Every write and delete in order; watches read from a position here.
    pub(crate) log: Vec<KvEvent>,
}

pub(crate) enum Slot {
    Live {
        value: Bytes,
        revision: u64,
        expires_at: Option<Duration>,
    },
    Tombstone {
        revision: u64,
    },
}

impl Clone for Slot {
    fn clone(&self) -> Self {
        match self {
            Self::Live {
                value,
                revision,
                expires_at,
            } => Self::Live {
                value: value.clone(),
                revision: *revision,
                expires_at: *expires_at,
            },
            Self::Tombstone { revision } => Self::Tombstone {
                revision: *revision,
            },
        }
    }
}

pub(crate) struct KvEvent {
    pub(crate) key: Key,
    pub(crate) revision: u64,
    /// `None` marks a delete or an expiry.
    pub(crate) value: Option<Bytes>,
}

impl KvEvent {
    pub(crate) fn to_watch_event(&self) -> WatchEvent {
        match &self.value {
            Some(value) => WatchEvent::Put(Entry {
                key: self.key.clone(),
                value: value.clone(),
                revision: bus_api::value::Revision::new(self.revision),
            }),
            None => WatchEvent::Delete {
                key: self.key.clone(),
                revision: bus_api::value::Revision::new(self.revision),
            },
        }
    }
}

impl BucketState {
    pub(crate) fn next_rev(&mut self) -> u64 {
        self.rev += 1;
        self.rev
    }

    /// Turns every expired live entry into a tombstone and logs a delete.
    pub(crate) fn sweep(&mut self, now: Duration) {
        let expired: Vec<Key> = self
            .slots
            .iter()
            .filter_map(|(key, slot)| match slot {
                Slot::Live {
                    expires_at: Some(at),
                    ..
                } if *at <= now => Some(key.clone()),
                _ => None,
            })
            .collect();
        for key in expired {
            let revision = self.next_rev();
            self.slots.insert(key.clone(), Slot::Tombstone { revision });
            self.log.push(KvEvent {
                key,
                revision,
                value: None,
            });
        }
    }
}

/// Which handed-out messages a consumer may pick next, in stream order.
///
/// A message is skipped when an earlier unsettled message of the same subject
/// blocks it, which keeps per-subject order under reordering. Returns at most
/// `window` candidates as `(seq, attempt, is_new)`.
fn collect_candidates(
    stream: &StreamState,
    cons: &ConsumerState,
    now: Duration,
    unsettled: usize,
    max_pending: usize,
    window: usize,
) -> Vec<(u64, u32, bool)> {
    let mut blocked: BTreeSet<&Subject> = BTreeSet::new();
    let mut out = Vec::new();
    for (idx, msg) in stream.messages.iter().enumerate() {
        let Ok(offset) = u64::try_from(idx) else {
            break;
        };
        let seq = stream.first_seq + offset;
        if seq < cons.cursor || !cons.filter.matches(&msg.subject) {
            continue;
        }
        let track = cons.track.get(&seq);
        if track.is_some_and(|t| t.settled) || blocked.contains(&msg.subject) {
            continue;
        }
        let eligible = match track {
            Some(t) => (t.visible_at <= now).then_some((t.attempt.saturating_add(1), false)),
            None => (msg.visible_at <= now && unsettled < max_pending).then_some((1, true)),
        };
        blocked.insert(&msg.subject);
        if let Some((attempt, is_new)) = eligible {
            out.push((seq, attempt, is_new));
            if out.len() >= window {
                break;
            }
        }
    }
    out
}

impl Inner {
    /// Checks the bus state and caller identity before any operation.
    /// `store` marks operations that touch durable state; partitioned
    /// principals are cut off from those (RFC-0011 [0011-064]).
    pub(crate) fn gate(&self, who: &Principal, store: bool) -> Result<(), BusError> {
        if self.closed {
            return Err(BusError::Unavailable { reason: CLOSED });
        }
        if who.is_anonymous() {
            return Err(BusError::PermissionDenied);
        }
        if store && self.partitioned(who) {
            return Err(BusError::Unavailable {
                reason: PARTITIONED,
            });
        }
        Ok(())
    }

    /// Grant of a principal; a principal without a grant is denied.
    pub(crate) fn grant(&self, who: &Principal) -> Result<&Grant, BusError> {
        self.grants.get(who).ok_or(BusError::PermissionDenied)
    }

    pub(crate) fn may_publish(&self, who: &Principal, subject: &Subject) -> bool {
        self.grants
            .get(who)
            .is_some_and(|g| g.publish.iter().any(|p| p.matches(subject)))
    }

    fn partitioned(&self, who: &Principal) -> bool {
        match &self.faults.partition {
            Some((a, b)) => a.contains(who) || b.contains(who),
            None => false,
        }
    }

    /// Side of a partition: `Some(true)` for set A, `Some(false)` for set B.
    fn side(&self, who: &Principal) -> Option<bool> {
        let (a, b) = self.faults.partition.as_ref()?;
        if a.contains(who) {
            Some(true)
        } else if b.contains(who) {
            Some(false)
        } else {
            None
        }
    }

    /// Whether a message between two principals crosses an active partition.
    pub(crate) fn crosses(&self, x: &Principal, y: &Principal) -> bool {
        matches!((self.side(x), self.side(y)), (Some(p), Some(q)) if p != q)
    }

    pub(crate) fn roll(&mut self, p: Option<f64>) -> bool {
        match p {
            Some(p) => self.rng.next_unit() < p,
            None => false,
        }
    }

    /// Lowest stream name whose subjects capture `subject`.
    pub(crate) fn captures(&self, subject: &Subject) -> Option<StreamName> {
        self.streams
            .iter()
            .find(|(_, st)| st.subjects.iter().any(|p| p.matches(subject)))
            .map(|(name, _)| name.clone())
    }

    fn sample_delay(&mut self, dist: DelayDist) -> Duration {
        match dist {
            DelayDist::Fixed(d) => d,
            DelayDist::Uniform { min, max } => {
                min + max.saturating_sub(min).mul_f64(self.rng.next_unit())
            }
        }
    }

    /// Appends a message to a stream and applies retention.
    pub(crate) fn store(
        &mut self,
        stream: &StreamName,
        subject: &Subject,
        headers: &Headers,
        payload: Bytes,
        publisher: &Principal,
    ) -> Result<u64, BusError> {
        let delay = self.faults.delay;
        let extra = match delay {
            Some(dist) => self.sample_delay(dist),
            None => Duration::ZERO,
        };
        let visible_at = self.now.saturating_add(extra);
        let keys: Vec<ConsumerKey> = self
            .consumers
            .keys()
            .filter(|k| k.0 == *stream)
            .cloned()
            .collect();
        for key in &keys {
            self.advance_cursor(key);
        }
        let st = self.streams.get_mut(stream).ok_or(BusError::Internal)?;
        let seq = st.next_seq;
        st.next_seq += 1;
        if delay.is_some() {
            self.trace.push(format!(
                "delay message seq={seq} by={}ms",
                extra.as_millis()
            ));
        }
        st.messages.push_back(StoredMsg {
            subject: subject.clone(),
            headers: headers.clone(),
            payload,
            publisher: publisher.clone(),
            visible_at,
        });
        if let Some(limit) = st.retention {
            while st.messages.len() > limit {
                st.messages.pop_front();
                st.first_seq += 1;
            }
        }
        Ok(seq)
    }

    /// Picks the next delivery of a consumer and marks it in flight. Returns
    /// `(seq, attempt)`. Faults: a dropped delivery stays in flight until the
    /// ack wait expires; a duplicate is made visible again at once.
    pub(crate) fn select_delivery(&mut self, key: &ConsumerKey) -> Option<(u64, u32)> {
        let Self {
            streams,
            consumers,
            rng,
            faults,
            trace,
            now,
            ack_wait,
            max_pending,
            ..
        } = self;
        let stream = streams.get(&key.0)?;
        let cons = consumers.get_mut(key)?;
        let window = faults.reorder.map_or(1, NonZeroUsize::get);
        loop {
            let unsettled = cons.track.values().filter(|t| !t.settled).count();
            let cands = collect_candidates(stream, cons, *now, unsettled, *max_pending, window);
            let pick = if cands.len() > 1 && faults.reorder.is_some() {
                rng.next_below(cands.len())
            } else {
                0
            };
            let &(seq, attempt, is_new) = cands.get(pick)?;
            if pick != 0 {
                trace.push(format!("reorder delivery seq={seq}"));
            }
            if is_new {
                cons.track.insert(
                    seq,
                    Track {
                        visible_at: *now + *ack_wait,
                        attempt,
                        settled: false,
                    },
                );
            } else if let Some(t) = cons.track.get_mut(&seq) {
                t.visible_at = *now + *ack_wait;
                t.attempt = attempt;
            }
            if let Some(p) = faults.drop {
                if rng.next_unit() < p {
                    trace.push(format!("drop delivery seq={seq}"));
                    continue;
                }
            }
            if let Some(p) = faults.duplicate {
                if rng.next_unit() < p {
                    trace.push(format!("dup delivery seq={seq}"));
                    if let Some(t) = cons.track.get_mut(&seq) {
                        t.visible_at = *now;
                    }
                }
            }
            return Some((seq, attempt));
        }
    }

    /// Applies a settlement to one handed-out message. An unknown seq is
    /// already past the cursor, so the call is an idempotent success.
    pub(crate) fn settle(&mut self, key: &ConsumerKey, seq: u64, action: Settle) {
        let now = self.now;
        let ack_wait = self.ack_wait;
        if let Some(cons) = self.consumers.get_mut(key) {
            if let Some(t) = cons.track.get_mut(&seq) {
                match action {
                    Settle::Done => t.settled = true,
                    Settle::Nak(delay) => t.visible_at = now.saturating_add(delay),
                    Settle::InProgress => t.visible_at = now.saturating_add(ack_wait),
                }
            }
        }
        if matches!(action, Settle::Done) {
            self.advance_cursor(key);
        }
    }

    /// Moves the cursor past settled and unmatched messages.
    pub(crate) fn advance_cursor(&mut self, key: &ConsumerKey) {
        let Some(stream) = self.streams.get(&key.0) else {
            return;
        };
        let Some(cons) = self.consumers.get_mut(key) else {
            return;
        };
        while cons.cursor < stream.next_seq {
            let Some(msg) = stream.get(cons.cursor) else {
                break;
            };
            let settled = cons.track.get(&cons.cursor).is_some_and(|t| t.settled);
            let matching = cons.filter.matches(&msg.subject);
            if !settled && matching {
                break;
            }
            cons.track.remove(&cons.cursor);
            cons.cursor += 1;
        }
    }

    /// Returns a reply once one is stored. Pending requests whose requester
    /// gave up are dropped by the caller; a timeout also removes the state.
    pub(crate) fn poll_reply(
        &mut self,
        req_id: u64,
        deadline: Duration,
    ) -> Option<Result<Reply, BusError>> {
        match self.replies.remove(&req_id) {
            Some(ReplyState::Done(reply)) => Some(Ok(reply)),
            Some(ReplyState::Failed(reason)) => Some(Err(BusError::Unavailable { reason })),
            _ if self.closed => Some(Err(BusError::Unavailable { reason: CLOSED })),
            _ if self.now >= deadline => Some(Err(BusError::Timeout)),
            _ => {
                self.replies.insert(req_id, ReplyState::Pending);
                None
            }
        }
    }

    /// Queues a request on the first matching server. Returns an error when
    /// the request must fail at once; a request that is lost in transit or
    /// cut off by a partition is accepted and times out.
    pub(crate) fn route(
        &mut self,
        who: &Principal,
        req_id: u64,
        subject: &Subject,
        headers: &Headers,
        payload: &Bytes,
    ) -> Result<(), BusError> {
        let Some(idx) = self.servers.iter().position(|s| s.pattern.matches(subject)) else {
            return if self.no_responder_signal {
                Err(BusError::NoResponder)
            } else {
                Ok(())
            };
        };
        let Some(target) = self.servers.get(idx).map(|s| s.principal.clone()) else {
            return Ok(());
        };
        if self.crosses(who, &target) {
            self.trace.push(format!("partition request req={req_id}"));
            return Ok(());
        }
        if self.roll(self.faults.drop) {
            self.trace.push(format!("drop request req={req_id}"));
            return Ok(());
        }
        let duplicate = self.roll(self.faults.duplicate);
        let max = self.max_pending;
        let retry_after = self.overload_retry_after;
        let queued = Queued {
            req_id,
            requester: who.clone(),
            subject: subject.clone(),
            headers: headers.clone(),
            payload: payload.clone(),
        };
        let Some(server) = self.servers.get_mut(idx) else {
            return Ok(());
        };
        if server.queue.len() >= max {
            return Err(BusError::Overloaded { retry_after });
        }
        if duplicate {
            self.trace.push(format!("dup request req={req_id}"));
            if let Some(server) = self.servers.get_mut(idx) {
                if server.queue.len() < max {
                    server.queue.push_back(queued.clone());
                }
            }
        }
        if let Some(server) = self.servers.get_mut(idx) {
            server.queue.push_back(queued);
        }
        Ok(())
    }

    /// Simulated restart: keeps acked state, redelivers unsettled messages,
    /// fails pending requests, clears server queues.
    pub(crate) fn restart(&mut self) {
        self.closed = false;
        self.epoch += 1;
        for state in self.replies.values_mut() {
            if matches!(state, ReplyState::Pending) {
                *state = ReplyState::Failed(RESTARTED);
            }
        }
        for server in &mut self.servers {
            server.queue.clear();
        }
        let now = self.now;
        for cons in self.consumers.values_mut() {
            for t in cons.track.values_mut() {
                if !t.settled {
                    t.visible_at = now;
                }
            }
        }
        self.trace.push("restart bus".to_owned());
    }

    /// Simulated consumer crash. Returns false when the consumer does not exist.
    pub(crate) fn crash_consumer(&mut self, key: &ConsumerKey) -> bool {
        let now = self.now;
        let Some(cons) = self.consumers.get_mut(key) else {
            return false;
        };
        cons.epoch += 1;
        for t in cons.track.values_mut() {
            if !t.settled {
                t.visible_at = now;
            }
        }
        self.trace.push(format!(
            "crash consumer {}/{}",
            key.0.as_str(),
            key.1.as_str()
        ));
        true
    }

    /// Whether an operation on `bucket` by `who` may proceed.
    pub(crate) fn kv_check(&self, who: &Principal, bucket: &Bucket) -> Result<(), BusError> {
        let grant = self.grant(who)?;
        if grant.tenant != *bucket.tenant() || !self.buckets.contains_key(bucket) {
            return Err(BusError::PermissionDenied);
        }
        Ok(())
    }

    /// Writes a KV entry under a precondition. Returns the new revision.
    pub(crate) fn kv_put(
        &mut self,
        bucket: &Bucket,
        key: &Key,
        value: Bytes,
        ttl: Option<Duration>,
        pre: Pre,
    ) -> Result<u64, BusError> {
        let now = self.now;
        let b = self.buckets.get_mut(bucket).ok_or(BusError::Internal)?;
        b.sweep(now);
        let existing = b.slots.get(key).cloned();
        match (pre, &existing) {
            (Pre::Create, Some(Slot::Live { revision, .. })) => {
                return Err(BusError::Conflict {
                    current_revision: bus_api::value::Revision::new(*revision),
                });
            }
            (Pre::Update(expected), Some(Slot::Live { revision, .. })) => {
                if *revision != expected {
                    return Err(BusError::Conflict {
                        current_revision: bus_api::value::Revision::new(*revision),
                    });
                }
            }
            (Pre::Update(_), Some(Slot::Tombstone { revision })) => {
                return Err(BusError::Conflict {
                    current_revision: bus_api::value::Revision::new(*revision),
                });
            }
            (Pre::Update(_), None) => return Err(BusError::Absent),
            _ => {}
        }
        let revision = b.next_rev();
        let expires_at = ttl.map(|t| now.saturating_add(t));
        b.slots.insert(
            key.clone(),
            Slot::Live {
                value: value.clone(),
                revision,
                expires_at,
            },
        );
        b.log.push(KvEvent {
            key: key.clone(),
            revision,
            value: Some(value),
        });
        Ok(revision)
    }

    /// Deletes a live KV entry, optionally checking its revision.
    pub(crate) fn kv_delete(
        &mut self,
        bucket: &Bucket,
        key: &Key,
        expected: Option<u64>,
    ) -> Result<u64, BusError> {
        let now = self.now;
        let b = self.buckets.get_mut(bucket).ok_or(BusError::Internal)?;
        b.sweep(now);
        match b.slots.get(key) {
            Some(Slot::Live { revision, .. }) => {
                if let Some(expected) = expected {
                    if expected != *revision {
                        return Err(BusError::Conflict {
                            current_revision: bus_api::value::Revision::new(*revision),
                        });
                    }
                }
            }
            _ => return Err(BusError::Absent),
        }
        let revision = b.next_rev();
        b.slots.insert(key.clone(), Slot::Tombstone { revision });
        b.log.push(KvEvent {
            key: key.clone(),
            revision,
            value: None,
        });
        Ok(revision)
    }
}

#[cfg(test)]
#[allow(clippy::panic, reason = "test setup failures abort the test")]
mod tests {
    use super::*;

    fn sub(text: &str) -> Subject {
        Subject::parse(text).unwrap_or_else(|e| panic!("subject {text}: {e}"))
    }

    fn pat(text: &str) -> SubjectPattern {
        SubjectPattern::parse(text).unwrap_or_else(|e| panic!("pattern {text}: {e}"))
    }

    fn stream_with(messages: &[&str]) -> StreamState {
        let mut st = StreamState {
            subjects: vec![pat("t.>")],
            retention: None,
            first_seq: 1,
            next_seq: 1,
            messages: VecDeque::new(),
        };
        for m in messages {
            st.messages.push_back(StoredMsg {
                subject: sub(m),
                headers: Headers::new(),
                payload: Bytes::new(),
                publisher: Principal::anonymous(),
                visible_at: Duration::ZERO,
            });
            st.next_seq += 1;
        }
        st
    }

    fn consumer(filter: &str, cursor: u64) -> ConsumerState {
        ConsumerState {
            filter: pat(filter),
            cursor,
            track: BTreeMap::new(),
            epoch: 0,
        }
    }

    // [0011-025] per-subject order: a later message of the same subject is
    // not collected ahead of an earlier unsettled one, even with a wide window.
    #[test]
    fn req_0011_025_same_subject_blocks_later_candidates() {
        let st = stream_with(&["t.a", "t.b", "t.a", "t.b"]);
        let cons = consumer("t.>", 1);
        let cands = collect_candidates(&st, &cons, Duration::ZERO, 0, 10, 4);
        let seqs: Vec<u64> = cands.iter().map(|c| c.0).collect();
        assert_eq!(seqs, vec![1, 2]);
    }

    // [0011-071] at most max_pending new deliveries stay in flight.
    #[test]
    fn req_0011_071_pending_bound_stops_new_candidates() {
        let st = stream_with(&["t.a", "t.b"]);
        let cons = consumer("t.>", 1);
        assert!(collect_candidates(&st, &cons, Duration::ZERO, 1, 1, 4).is_empty());
    }

    #[test]
    fn stream_get_respects_retained_range() {
        let mut st = stream_with(&["t.a", "t.b"]);
        st.first_seq = 2;
        assert!(st.get(1).is_none());
        assert!(st.get(2).is_some());
        assert!(st.get(3).is_none());
    }
}
