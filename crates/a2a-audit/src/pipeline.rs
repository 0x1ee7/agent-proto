//! Emitter-side pipeline: seal, spool, publish, gap and checkpoint events (RFC-0013 0013-013,
//! 0013-014, 0013-015, 0013-027, 0013-046, 0013-047).
//!
//! Every event is sealed (redacted, hashed, chained) and appended to the local spool before any
//! publish attempt. An event leaves the spool only after the publisher accepted it, so a crash
//! or a publish failure at any point leaves the event in the spool for the next flush.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use a2a_types::ids::TaskId;
use a2a_types::proto::a2abus::{AuditEvent, AuditEventType};
use prost::Message;
use serde_json::{Map, Value, json};

use crate::chain::{CheckpointSigner, GENESIS_HASH};
use crate::clock::{Clock, format_timestamp};
use crate::coalesce::{PeerCoalescer, Summary};
use crate::emitter::{Emitter, EmitterIdentity};
use crate::error::AuditError;
use crate::event::EventBuilder;
use crate::redact::Redactor;
use crate::spool::{AppendOutcome, Entry, Overflow, Spool, SpoolError};

/// A publish attempt failed. The event stays spooled and is retried by the next flush.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("publish failed: {0}")]
pub struct PublishError(pub String);

/// Transport the pipeline publishes sealed events to (the NATS subject, 0013-009).
pub trait Publisher: Send {
    /// Publishes one event. `payload` is the protobuf encoding of `event`. Returns only after the
    /// transport has accepted the message.
    fn publish(&mut self, event: &AuditEvent, payload: &[u8]) -> Result<(), PublishError>;
}

/// The latest sequence number and chain hash of one emitter, as recorded in a checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmitterHead {
    /// Highest sequence number the emitter has spooled.
    pub seq: u64,
    /// Chain hash of that event.
    pub chain_hash: String,
}

/// Construction parameters for [`Pipeline`].
#[derive(Debug)]
pub struct PipelineConfig {
    /// Who emits.
    pub identity: EmitterIdentity,
    /// Root of the local audit state. The spool lives under `<state_dir>/audit/spool/`.
    pub state_dir: PathBuf,
    /// Spool size cap in bytes (0013-015).
    pub spool_cap_bytes: u64,
    /// Timestamp source.
    pub clock: Arc<dyn Clock>,
    /// Checkpoint signer. Required only for supervisor checkpoints.
    pub signer: Option<Arc<dyn CheckpointSigner>>,
    /// Redaction rules applied before hashing (0013-022).
    pub redactor: Redactor,
}

/// One emitter's sealing, spooling, and publishing.
pub struct Pipeline {
    emitter: Emitter,
    spool: Spool,
    publisher: Box<dyn Publisher>,
    coalescer: PeerCoalescer,
    dropped: u64,
}

impl std::fmt::Debug for Pipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pipeline")
            .field("identity", self.emitter.identity())
            .field("pending", &self.spool.pending())
            .field("dropped", &self.dropped)
            .finish_non_exhaustive()
    }
}

impl Pipeline {
    /// Opens the spool, resumes the emitter from its high-water mark (0013-003), and attaches the
    /// publisher. Events left in the spool by an earlier run are published by the next
    /// [`Pipeline::flush`].
    pub fn open(config: PipelineConfig, publisher: Box<dyn Publisher>) -> Result<Self, AuditError> {
        let spool_dir = config.state_dir.join("audit").join("spool");
        std::fs::create_dir_all(&spool_dir)
            .map_err(|err| AuditError::Spool(SpoolError::from(err)))?;
        let spool_path = spool_dir.join(format!("{}.jsonl", config.identity.name()));
        let spool = Spool::open(&spool_path, config.spool_cap_bytes)?;
        let (seq, hash) = spool.high_water();
        let hash = if seq == 0 {
            GENESIS_HASH.to_owned()
        } else {
            hash.to_owned()
        };
        let emitter = Emitter::resume(
            config.identity,
            config.clock,
            config.redactor,
            config.signer,
            (seq, &hash),
        )?;
        Ok(Self {
            emitter,
            spool,
            publisher,
            coalescer: PeerCoalescer::default(),
            dropped: 0,
        })
    }

    /// The emitter's identity.
    pub fn identity(&self) -> &EmitterIdentity {
        self.emitter.identity()
    }

    /// Seals and spools an event, then attempts to publish everything pending. Returns the
    /// event's sequence number. Publish failures do not fail the call (0013-013).
    pub fn emit(&mut self, builder: EventBuilder) -> Result<u64, AuditError> {
        let seq = self
            .spool_event(builder, Overflow::Discard)?
            .ok_or(AuditError::SpoolFull)?;
        self.flush()?;
        Ok(seq)
    }

    /// Emits REDACTION_APPLIED for `task` if any rule fired while sealing its events, at most once
    /// per task (0013-046). Returns the sequence number if one was emitted.
    pub fn finish_task(&mut self, task: &TaskId) -> Result<Option<u64>, AuditError> {
        let tally = self.emitter.take_tally(task.as_str());
        if tally.is_empty() {
            return Ok(None);
        }
        let builder = EventBuilder::new(AuditEventType::RedactionApplied)
            .task(task.clone())
            .detail("rules", tally.to_value());
        let seq = self
            .spool_event(builder, Overflow::Discard)?
            .ok_or(AuditError::SpoolFull)?;
        self.flush()?;
        Ok(Some(seq))
    }

    /// Counts one rejected peer message. Rejections with the same peer and reason are summarised
    /// into one PEER_REJECTED event per window by [`Pipeline::drain`] (0013-047).
    pub fn peer_rejected(&mut self, peer: &str, reason: &str) {
        self.coalescer
            .record(peer, reason, self.emitter.clock().now());
    }

    /// Emits PEER_REJECTED for every coalescing window that has closed, then publishes pending
    /// events. Returns the number of events published.
    pub fn drain(&mut self) -> Result<usize, AuditError> {
        let now = self.emitter.clock().now();
        for summary in self.coalescer.take_expired(now) {
            self.emit_peer_summary(&summary)?;
        }
        self.flush()
    }

    /// Emits PEER_REJECTED for every open window, regardless of age, then publishes pending
    /// events. Called at shutdown.
    pub fn shutdown(&mut self) -> Result<usize, AuditError> {
        for summary in self.coalescer.take_all() {
            self.emit_peer_summary(&summary)?;
        }
        self.flush()
    }

    /// Emits AUDIT_CHECKPOINT (0013-027). `peers` holds the other emitters' heads; this
    /// emitter's own head is added automatically. The checkpoint is signed by the emitter's
    /// configured signer.
    pub fn checkpoint(&mut self, peers: &BTreeMap<String, EmitterHead>) -> Result<u64, AuditError> {
        let mut heads = peers.clone();
        if let Some(own) = self.head() {
            heads.insert(self.emitter.identity().name().to_owned(), own);
        }
        let emitters: Map<String, Value> = heads
            .iter()
            .map(|(name, head)| {
                (
                    name.clone(),
                    json!({"seq": head.seq.to_string(), "chain_hash": head.chain_hash}),
                )
            })
            .collect();
        let signed_at = format_timestamp(self.emitter.clock().now());
        let builder = EventBuilder::new(AuditEventType::AuditCheckpoint)
            .detail("emitters", Value::Object(emitters))
            .detail_str("signed_at", &signed_at);
        let seq = self
            .spool_event(builder, Overflow::Discard)?
            .ok_or(AuditError::SpoolFull)?;
        self.flush()?;
        Ok(seq)
    }

    /// This emitter's latest sequence number and chain hash, or `None` before its first event.
    pub fn head(&self) -> Option<EmitterHead> {
        let (seq, hash) = self.spool.high_water();
        (seq > 0).then(|| EmitterHead {
            seq,
            chain_hash: hash.to_owned(),
        })
    }

    /// Publishes spooled events in sequence order, acknowledging each only after the publisher
    /// accepts it. Emits one AUDIT_GAP per recorded gap first. Stops at the first publish failure
    /// and returns the number published so far.
    pub fn flush(&mut self) -> Result<usize, AuditError> {
        while self.spool_pending_gap()? {}
        let mut published = 0;
        while let Some(entry) = self.spool.front() {
            let event: AuditEvent = serde_json::from_value(entry.event)?;
            let payload = event.encode_to_vec();
            if self.publisher.publish(&event, &payload).is_err() {
                break;
            }
            self.spool.ack(entry.seq)?;
            published += 1;
        }
        Ok(published)
    }

    /// Events spooled but not yet acknowledged.
    pub fn pending(&self) -> usize {
        self.spool.pending()
    }

    /// Events discarded from a full spool since this pipeline was opened (0013-015).
    pub fn dropped_total(&self) -> u64 {
        self.dropped
    }

    fn spool_event(
        &mut self,
        builder: EventBuilder,
        overflow: Overflow,
    ) -> Result<Option<u64>, AuditError> {
        let sealed = self.emitter.prepare(builder)?;
        let entry = Entry {
            seq: sealed.seq,
            hash: sealed.chain_hash.clone(),
            event: sealed.value.clone(),
        };
        match self.spool.append(entry, overflow)? {
            AppendOutcome::Written { discarded } => self.dropped += discarded,
            AppendOutcome::Full => return Ok(None),
        }
        self.emitter.commit(&sealed);
        Ok(Some(sealed.seq))
    }

    fn spool_pending_gap(&mut self) -> Result<bool, AuditError> {
        let Some((first, last)) = self.spool.first_gap() else {
            return Ok(false);
        };
        let builder = EventBuilder::new(AuditEventType::AuditGap)
            .detail_str("first_seq", &first.to_string())
            .detail_str("last_seq", &last.to_string());
        if self.spool_event(builder, Overflow::Refuse)?.is_none() {
            return Ok(false);
        }
        self.spool.clear_gap()?;
        Ok(true)
    }

    fn emit_peer_summary(&mut self, summary: &Summary) -> Result<(), AuditError> {
        let builder = EventBuilder::new(AuditEventType::PeerRejected)
            .detail_str("peer", &summary.peer)
            .detail_str("reason", &summary.reason)
            .detail_count("count", summary.count);
        self.spool_event(builder, Overflow::Discard)?
            .ok_or(AuditError::SpoolFull)?;
        Ok(())
    }
}
