//! Event sealing: identity, envelope, redaction, sequencing, hash chain and checkpoint signing
//! (RFC-0013 0013-002..0013-006, 0013-022, 0013-026, 0013-027, 0013-031, 0013-046).

use std::collections::BTreeMap;
use std::sync::Arc;

use a2a_types::ids::{AgentId, TenantId};
use a2a_types::proto::a2abus::{AuditEvent, AuditEventType, EmitterKind};
use chrono::{DateTime, TimeDelta, Utc};
use serde_json::Value;
use uuid::Uuid;

use crate::chain::{self, CheckpointSigner, GENESIS_HASH};
use crate::clock::{Clock, format_timestamp};
use crate::correlation::TraceBindings;
use crate::error::AuditError;
use crate::event::{EventBuilder, fit_details};
use crate::redact::{Redactor, RuleTally};
use crate::schema;

/// Maximum number of tasks whose redaction tallies are held at once.
const TASK_TALLY_CAP: usize = 4096;

/// Who emits: tenant, emitter name, and kind (0013-006, 0013-009).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmitterIdentity {
    tenant: TenantId,
    name: String,
    kind: EmitterKind,
}

impl EmitterIdentity {
    /// Validates the identity for its kind: supervisor is `supervisor`, operator is `tui` or
    /// `cli`, gateway is the gateway id, and a worker is a non-reserved agent id.
    pub fn new(tenant: &str, name: &str, kind: EmitterKind) -> Result<Self, AuditError> {
        let tenant =
            TenantId::parse(tenant).map_err(|err| AuditError::Identity(err.to_string()))?;
        let valid = match kind {
            EmitterKind::Supervisor => name == "supervisor",
            EmitterKind::Operator => name == "tui" || name == "cli",
            EmitterKind::Gateway => name == AgentId::gateway().as_str(),
            EmitterKind::Worker => AgentId::parse(name).is_ok_and(|id| !id.is_reserved()),
            EmitterKind::Unspecified => false,
        };
        if !valid {
            return Err(AuditError::Identity(format!(
                "emitter {name:?} is not valid for kind {}",
                kind.as_str_name()
            )));
        }
        Ok(Self {
            tenant,
            name: name.to_owned(),
            kind,
        })
    }

    /// Tenant identifier.
    pub fn tenant(&self) -> &str {
        self.tenant.as_str()
    }

    /// Emitter name, which is also the spool file stem and the NATS subject segment.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Emitter kind.
    pub fn kind(&self) -> EmitterKind {
        self.kind
    }
}

/// An event that has been redacted, sequenced, hashed and (for checkpoints) signed, but not
/// yet committed to the emitter state.
#[derive(Debug, Clone)]
pub struct Sealed {
    /// Per-emitter sequence number.
    pub seq: u64,
    /// Chain hash over the event's preimage.
    pub chain_hash: String,
    /// Event as JSON, including `chainHash`.
    pub value: Value,
    /// Event as protobuf, ready to publish.
    pub event: AuditEvent,
    task: Option<String>,
    traceparent: Option<String>,
    tally: RuleTally,
}

impl Sealed {
    /// Task the event belongs to, if any.
    pub fn task(&self) -> Option<&str> {
        self.task.as_deref()
    }

    /// Rule counts recorded while redacting this event.
    pub fn tally(&self) -> &RuleTally {
        &self.tally
    }
}

/// Per-emitter sealing state: next sequence number, previous chain hash, trace bindings, and
/// per-task redaction tallies.
#[derive(Debug)]
pub struct Emitter {
    identity: EmitterIdentity,
    clock: Arc<dyn Clock>,
    signer: Option<Arc<dyn CheckpointSigner>>,
    redactor: Redactor,
    next_seq: u64,
    prev_hash: String,
    trace: TraceBindings,
    tallies: BTreeMap<String, RuleTally>,
}

impl Emitter {
    /// Starts a new emitter at sequence 1 with the genesis previous hash.
    pub fn new(
        identity: EmitterIdentity,
        clock: Arc<dyn Clock>,
        redactor: Redactor,
        signer: Option<Arc<dyn CheckpointSigner>>,
    ) -> Self {
        Self {
            identity,
            clock,
            signer,
            redactor,
            next_seq: 1,
            prev_hash: GENESIS_HASH.to_owned(),
            trace: TraceBindings::default(),
            tallies: BTreeMap::new(),
        }
    }

    /// Resumes after `high_water` (the spool's sequence and chain hash) so numbering continues
    /// with no reuse (0013-003).
    pub fn resume(
        identity: EmitterIdentity,
        clock: Arc<dyn Clock>,
        redactor: Redactor,
        signer: Option<Arc<dyn CheckpointSigner>>,
        high_water: (u64, &str),
    ) -> Result<Self, AuditError> {
        let (seq, hash) = high_water;
        let valid = if seq == 0 {
            hash == GENESIS_HASH
        } else {
            is_hex64(hash)
        };
        if !valid {
            return Err(AuditError::Resume(format!(
                "high water of emitter {} is malformed",
                identity.name()
            )));
        }
        let mut emitter = Self::new(identity, clock, redactor, signer);
        emitter.next_seq = seq + 1;
        emitter.prev_hash = hash.to_owned();
        Ok(emitter)
    }

    /// The emitter's identity.
    pub fn identity(&self) -> &EmitterIdentity {
        &self.identity
    }

    /// The clock used for timestamps.
    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    /// Redacts, sequences, hashes and (for checkpoints) signs an event without changing
    /// emitter state. Call [`Emitter::commit`] once the event is safely spooled.
    pub fn prepare(&self, builder: EventBuilder) -> Result<Sealed, AuditError> {
        let (ty, mut correlation, mut details) = builder.into_parts();
        let spec = schema::spec(ty).ok_or(AuditError::UnknownType(ty as i32))?;
        if !spec.kinds.contains(&self.identity.kind) {
            return Err(AuditError::KindNotAllowed {
                kind: self.identity.kind.as_str_name().to_owned(),
                event: ty.as_str_name().to_owned(),
            });
        }
        correlation.validate()?;
        let task = correlation
            .task
            .as_ref()
            .map(|task| task.as_str().to_owned());
        correlation.traceparent = self
            .trace
            .resolve(task.as_deref(), correlation.traceparent.as_deref())?;
        schema::check_details(spec, &details)?;
        let tally = self.redactor.redact_details(&mut details);
        let truncated = fit_details(&mut details)?;

        let mut event = AuditEvent {
            event_id: Uuid::now_v7().to_string(),
            time: format_timestamp(self.clock.now()),
            seq: self.next_seq,
            tenant: self.identity.tenant().to_owned(),
            emitter: self.identity.name().to_owned(),
            emitter_kind: self.identity.kind as i32,
            r#type: ty as i32,
            redactions: tally.rule_ids(),
            truncated,
            prev_hash: self.prev_hash.clone(),
            details: Some(serde_json::from_value(Value::Object(details))?),
            ..Default::default()
        };
        let traceparent = correlation.traceparent.clone();
        correlation.apply(&mut event);

        if ty == AuditEventType::AuditCheckpoint {
            let signer = self.signer.as_ref().ok_or(AuditError::NoSigner)?;
            event.key_id = signer.key_id().to_owned();
            let preimage = chain::preimage(&serde_json::to_value(&event)?)?;
            event.signature = chain::encode_signature(&signer.sign(&preimage));
        }

        let preimage = chain::preimage(&serde_json::to_value(&event)?)?;
        let chain_hash = chain::chain_hash(&preimage);
        event.chain_hash = chain_hash.clone();
        schema::validate_event(&event)?;
        let value = serde_json::to_value(&event)?;

        Ok(Sealed {
            seq: event.seq,
            chain_hash,
            value,
            event,
            task,
            traceparent,
            tally,
        })
    }

    /// Advances sequence, previous hash, trace bindings and the task tally after `sealed` has
    /// been spooled.
    pub fn commit(&mut self, sealed: &Sealed) {
        self.next_seq = sealed.seq + 1;
        self.prev_hash = sealed.chain_hash.clone();
        if let (Some(task), Some(traceparent)) = (&sealed.task, &sealed.traceparent) {
            self.trace.bind(task, traceparent);
        }
        if let Some(task) = &sealed.task {
            if !sealed.tally.is_empty() {
                if !self.tallies.contains_key(task) && self.tallies.len() >= TASK_TALLY_CAP {
                    let _ = self.tallies.pop_first();
                }
                self.tallies
                    .entry(task.clone())
                    .or_default()
                    .merge(&sealed.tally);
            }
        }
    }

    /// Prepares and commits in one step.
    pub fn seal(&mut self, builder: EventBuilder) -> Result<Sealed, AuditError> {
        let sealed = self.prepare(builder)?;
        self.commit(&sealed);
        Ok(sealed)
    }

    /// Returns and clears the redaction tally accumulated for `task`, so REDACTION_APPLIED is
    /// emitted at most once per task (0013-046).
    pub fn take_tally(&mut self, task: &str) -> RuleTally {
        self.tallies.remove(task).unwrap_or_default()
    }
}

/// Checkpoint cadence (0013-027): a checkpoint is due once `every` has passed since the last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointSchedule {
    every: TimeDelta,
    last: DateTime<Utc>,
}

impl CheckpointSchedule {
    /// Hourly cadence starting at `now`.
    pub fn hourly(now: DateTime<Utc>) -> Self {
        Self {
            every: TimeDelta::hours(1),
            last: now,
        }
    }

    /// Whether a checkpoint is due at `now`.
    pub fn is_due(&self, now: DateTime<Utc>) -> bool {
        now - self.last >= self.every
    }

    /// Records that a checkpoint was emitted at `now`.
    pub fn mark(&mut self, now: DateTime<Utc>) {
        self.last = now;
    }
}

fn is_hex64(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::collections::HashSet;
    use std::time::Duration;

    use a2a_types::ids::TaskId;
    use chrono::TimeZone;
    use serde_json::json;

    use super::*;
    use crate::chain::CheckpointVerifier;
    use crate::clock::{ManualClock, is_audit_timestamp};
    use crate::testkit::{DigestSigner, DigestVerifier};

    const SECRET: &str = "hunter2-secret-token";
    const TRACE: &str = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
    const OTHER_TRACE: &str = "00-1af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";

    fn start() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 1, 12, 0, 0).single().unwrap()
    }

    fn worker_identity() -> EmitterIdentity {
        EmitterIdentity::new("tenant-a", "agent-a", EmitterKind::Worker).unwrap()
    }

    fn worker(clock: &ManualClock, redactor: Redactor) -> Emitter {
        Emitter::new(worker_identity(), Arc::new(clock.clone()), redactor, None)
    }

    fn approval(task: &TaskId) -> EventBuilder {
        EventBuilder::new(AuditEventType::ApprovalRequested)
            .task(task.clone())
            .detail("request_id", json!("req-1"))
    }

    #[test]
    fn req_0013_002_envelope_fields_are_present() {
        let clock = ManualClock::new(start());
        let mut emitter = worker(&clock, Redactor::new());
        let sealed = emitter.seal(approval(&TaskId::generate())).unwrap();
        let event = &sealed.event;
        assert!(!event.event_id.is_empty());
        assert!(!event.time.is_empty());
        assert_eq!(event.seq, 1);
        assert_eq!(event.tenant, "tenant-a");
        assert_eq!(event.emitter, "agent-a");
        assert_eq!(event.emitter_kind, EmitterKind::Worker as i32);
        assert_eq!(event.r#type, AuditEventType::ApprovalRequested as i32);
        assert_eq!(event.prev_hash, GENESIS_HASH);
        assert_eq!(event.chain_hash.len(), 64);
        assert!(schema::validate_event(event).is_ok());
    }

    #[test]
    fn req_0013_002_identity_rejects_kind_mismatch() {
        assert!(EmitterIdentity::new("tenant-a", "agent-a", EmitterKind::Unspecified).is_err());
        assert!(EmitterIdentity::new("tenant-a", "tui", EmitterKind::Supervisor).is_err());
        assert!(EmitterIdentity::new("tenant-a", "supervisor", EmitterKind::Supervisor).is_ok());
        assert!(EmitterIdentity::new("tenant-a", "cli", EmitterKind::Operator).is_ok());
    }

    #[test]
    fn req_0013_002_kind_outside_spec_is_refused() {
        let clock = ManualClock::new(start());
        let emitter = Emitter::new(
            EmitterIdentity::new("tenant-a", "supervisor", EmitterKind::Supervisor).unwrap(),
            Arc::new(clock),
            Redactor::new(),
            None,
        );
        let err = emitter.prepare(approval(&TaskId::generate())).err();
        assert!(matches!(err, Some(AuditError::KindNotAllowed { .. })));
    }

    #[test]
    fn req_0013_004_event_ids_are_unique_uuidv7() {
        let clock = ManualClock::new(start());
        let mut emitter = worker(&clock, Redactor::new());
        let task = TaskId::generate();
        let mut ids = HashSet::new();
        for _ in 0..10_000 {
            let sealed = emitter.seal(approval(&task)).unwrap();
            let id = Uuid::parse_str(&sealed.event.event_id).unwrap();
            assert_eq!(id.get_version_num(), 7);
            assert!(ids.insert(sealed.event.event_id));
        }
        assert_eq!(ids.len(), 10_000);
    }

    #[test]
    fn req_0013_005_timestamp_is_utc_milliseconds() {
        let clock = ManualClock::new(start());
        clock.advance(Duration::from_millis(123));
        let mut emitter = worker(&clock, Redactor::new());
        let sealed = emitter.seal(approval(&TaskId::generate())).unwrap();
        assert_eq!(sealed.event.time, "2026-05-01T12:00:00.123Z");
        assert!(is_audit_timestamp(&sealed.event.time));
    }

    #[test]
    fn req_0013_003_sequence_is_contiguous_and_resumes_from_high_water() {
        let clock = ManualClock::new(start());
        let mut emitter = worker(&clock, Redactor::new());
        let task = TaskId::generate();
        let first = emitter.seal(approval(&task)).unwrap();
        let second = emitter.seal(approval(&task)).unwrap();
        assert_eq!((first.seq, second.seq), (1, 2));
        assert_eq!(second.event.prev_hash, first.chain_hash);

        let mut resumed = Emitter::resume(
            worker_identity(),
            Arc::new(clock.clone()),
            Redactor::new(),
            None,
            (second.seq, &second.chain_hash),
        )
        .unwrap();
        let third = resumed.seal(approval(&task)).unwrap();
        assert_eq!(third.seq, 3);
        assert_eq!(third.event.prev_hash, second.chain_hash);
    }

    #[test]
    fn req_0013_003_resume_rejects_malformed_high_water() {
        let clock = ManualClock::new(start());
        let err = Emitter::resume(
            worker_identity(),
            Arc::new(clock.clone()),
            Redactor::new(),
            None,
            (4, "not-a-hash"),
        )
        .err();
        assert!(matches!(err, Some(AuditError::Resume(_))));
        let err = Emitter::resume(
            worker_identity(),
            Arc::new(clock),
            Redactor::new(),
            None,
            (0, &"a".repeat(64)),
        )
        .err();
        assert!(matches!(err, Some(AuditError::Resume(_))));
    }

    #[test]
    fn req_0013_026_chain_hash_recomputes_and_links() {
        let clock = ManualClock::new(start());
        let mut emitter = worker(&clock, Redactor::new());
        let task = TaskId::generate();
        let events: Vec<Sealed> = (0..3)
            .map(|_| emitter.seal(approval(&task)).unwrap())
            .collect();
        assert_eq!(events[0].event.prev_hash, GENESIS_HASH);
        for pair in events.windows(2) {
            assert_eq!(pair[1].event.prev_hash, pair[0].chain_hash);
        }
        for sealed in &events {
            let recomputed = chain::chain_hash(&chain::preimage(&sealed.value).unwrap());
            assert_eq!(recomputed, sealed.chain_hash);
        }
    }

    #[test]
    fn req_0013_022_secret_is_absent_from_sealed_event() {
        let clock = ManualClock::new(start());
        let mut emitter = worker(&clock, Redactor::new().with_secret(SECRET));
        let task = TaskId::generate();
        let sealed = emitter
            .seal(
                EventBuilder::new(AuditEventType::ApprovalRequested)
                    .task(task)
                    .detail("request_id", json!("req-1"))
                    .detail("args_summary", json!(format!("deploy with {SECRET}"))),
            )
            .unwrap();
        let text = serde_json::to_string(&sealed.value).unwrap();
        assert!(!text.contains(SECRET));
        assert!(sealed.event.redactions.iter().any(|rule| rule == "R-A5"));
    }

    #[test]
    fn req_0013_046_redaction_tally_is_taken_once_per_task() {
        let clock = ManualClock::new(start());
        let mut emitter = worker(&clock, Redactor::new().with_secret(SECRET));
        let task = TaskId::generate();
        for _ in 0..2 {
            emitter
                .seal(
                    EventBuilder::new(AuditEventType::ApprovalRequested)
                        .task(task.clone())
                        .detail("request_id", json!("req-1"))
                        .detail("args_summary", json!(format!("deploy with {SECRET}"))),
                )
                .unwrap();
        }
        let tally = emitter.take_tally(task.as_str());
        assert_eq!(tally.count(crate::redact::RuleId::A5), 2);
        assert!(emitter.take_tally(task.as_str()).is_empty());
    }

    #[test]
    fn req_0013_031_traceparent_is_inherited_within_a_task() {
        let clock = ManualClock::new(start());
        let mut emitter = worker(&clock, Redactor::new());
        let task = TaskId::generate();
        let first = emitter.seal(approval(&task).traceparent(TRACE)).unwrap();
        assert_eq!(first.event.traceparent, TRACE);
        let second = emitter.seal(approval(&task)).unwrap();
        assert_eq!(second.event.traceparent, TRACE);
    }

    #[test]
    fn req_0013_031_conflicting_traceparent_is_rejected() {
        let clock = ManualClock::new(start());
        let mut emitter = worker(&clock, Redactor::new());
        let task = TaskId::generate();
        emitter.seal(approval(&task).traceparent(TRACE)).unwrap();
        let err = emitter.seal(approval(&task).traceparent(OTHER_TRACE)).err();
        assert!(matches!(err, Some(AuditError::Correlation(_))));
    }

    #[test]
    fn req_0013_027_checkpoint_signature_verifies_only_with_right_key() {
        let clock = ManualClock::new(start());
        let signer = Arc::new(DigestSigner::new("key-1"));
        let mut emitter = Emitter::new(
            EmitterIdentity::new("tenant-a", "supervisor", EmitterKind::Supervisor).unwrap(),
            Arc::new(clock.clone()),
            Redactor::new(),
            Some(signer),
        );
        let sealed = emitter
            .seal(
                EventBuilder::new(AuditEventType::AuditCheckpoint)
                    .detail(
                        "emitters",
                        json!({"agent-a": {"seq": "1", "chain_hash": "a".repeat(64)}}),
                    )
                    .detail("signed_at", json!("2026-05-01T12:00:00.000Z")),
            )
            .unwrap();
        assert_eq!(sealed.event.key_id, "key-1");
        let preimage = chain::preimage(&sealed.value).unwrap();
        let signature = chain::decode_signature(&sealed.event.signature).unwrap();
        assert!(DigestVerifier::new("key-1").verify("key-1", &preimage, &signature));
        assert!(!DigestVerifier::new("key-2").verify("key-1", &preimage, &signature));
    }

    #[test]
    fn req_0013_027_checkpoint_without_signer_is_refused() {
        let clock = ManualClock::new(start());
        let emitter = Emitter::new(
            EmitterIdentity::new("tenant-a", "supervisor", EmitterKind::Supervisor).unwrap(),
            Arc::new(clock),
            Redactor::new(),
            None,
        );
        let err = emitter
            .prepare(
                EventBuilder::new(AuditEventType::AuditCheckpoint)
                    .detail("emitters", json!({}))
                    .detail("signed_at", json!("2026-05-01T12:00:00.000Z")),
            )
            .err();
        assert!(matches!(err, Some(AuditError::NoSigner)));
    }

    #[test]
    fn req_0013_027_hourly_schedule_is_due_after_an_hour() {
        let mut schedule = CheckpointSchedule::hourly(start());
        assert!(!schedule.is_due(start() + TimeDelta::minutes(59)));
        assert!(schedule.is_due(start() + TimeDelta::hours(1)));
        schedule.mark(start() + TimeDelta::hours(1));
        assert!(!schedule.is_due(start() + TimeDelta::hours(1)));
    }
}
