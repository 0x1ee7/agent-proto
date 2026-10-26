//! Pipeline-level checks through the public API (RFC-0013 0013-003, 0013-013, 0013-014,
//! 0013-047). Unit tests inside each module cover the finer rules.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::{Arc, Mutex};

use a2a_audit::clock::ManualClock;
use a2a_audit::pipeline::{Pipeline, PipelineConfig, PublishError, Publisher};
use a2a_audit::redact::Redactor;
use a2a_audit::{EmitterIdentity, EventBuilder};
use a2a_types::proto::a2abus::{AuditEvent, AuditEventType, EmitterKind};
use chrono::{TimeZone, Utc};
use prost::Message;
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Default)]
struct Log {
    payloads: Vec<Vec<u8>>,
    fail: bool,
}

struct Recorder(Arc<Mutex<Log>>);

impl Publisher for Recorder {
    fn publish(&mut self, _event: &AuditEvent, payload: &[u8]) -> Result<(), PublishError> {
        let mut log = self.0.lock().unwrap();
        if log.fail {
            return Err(PublishError("broker unavailable".to_owned()));
        }
        log.payloads.push(payload.to_vec());
        Ok(())
    }
}

fn clock() -> ManualClock {
    ManualClock::new(Utc.with_ymd_and_hms(2026, 5, 1, 12, 0, 0).single().unwrap())
}

fn config(dir: &Path, clock: &ManualClock, name: &str, kind: EmitterKind) -> PipelineConfig {
    PipelineConfig {
        identity: EmitterIdentity::new("tenant-a", name, kind).unwrap(),
        state_dir: dir.to_path_buf(),
        spool_cap_bytes: 1 << 20,
        clock: Arc::new(clock.clone()),
        signer: None,
        redactor: Redactor::new(),
    }
}

fn approval(request: &str) -> EventBuilder {
    EventBuilder::new(AuditEventType::ApprovalRequested).detail("request_id", json!(request))
}

fn decode(payload: &[u8]) -> AuditEvent {
    AuditEvent::decode(payload).unwrap()
}

#[test]
fn req_0013_013_published_payload_is_the_sealed_event() {
    let dir = tempfile::tempdir().unwrap();
    let clock = clock();
    let log = Arc::new(Mutex::new(Log::default()));
    let config = config(dir.path(), &clock, "agent-a", EmitterKind::Worker);
    let mut pipeline = Pipeline::open(config, Box::new(Recorder(log.clone()))).unwrap();

    assert_eq!(pipeline.emit(approval("req-1")).unwrap(), 1);
    assert_eq!(pipeline.emit(approval("req-2")).unwrap(), 2);
    assert_eq!(pipeline.pending(), 0);

    let payloads = log.lock().unwrap().payloads.clone();
    assert_eq!(payloads.len(), 2);
    let event = decode(payloads.last().unwrap());
    assert_eq!(event.seq, 2);
    assert_eq!(event.emitter, "agent-a");
    assert_eq!(event.r#type, AuditEventType::ApprovalRequested as i32);
    assert_eq!(event.chain_hash.len(), 64);
    assert_eq!(
        Uuid::parse_str(&event.event_id).unwrap().get_version_num(),
        7
    );
    let json: Value = serde_json::to_value(&event).unwrap();
    assert_eq!(json["details"]["request_id"], "req-2");
}

#[test]
fn req_0013_014_failed_publish_keeps_event_spooled_until_flush() {
    let dir = tempfile::tempdir().unwrap();
    let clock = clock();
    let log = Arc::new(Mutex::new(Log {
        fail: true,
        ..Log::default()
    }));
    let config = config(dir.path(), &clock, "agent-a", EmitterKind::Worker);
    let mut pipeline = Pipeline::open(config, Box::new(Recorder(log.clone()))).unwrap();

    assert_eq!(pipeline.emit(approval("req-1")).unwrap(), 1);
    assert_eq!(pipeline.pending(), 1);
    assert!(log.lock().unwrap().payloads.is_empty());

    log.lock().unwrap().fail = false;
    assert_eq!(pipeline.flush().unwrap(), 1);
    assert_eq!(pipeline.pending(), 0);
    assert_eq!(log.lock().unwrap().payloads.len(), 1);
}

#[test]
fn req_0013_003_reopened_pipeline_keeps_unacked_events_and_continues_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let clock = clock();
    let log = Arc::new(Mutex::new(Log {
        fail: true,
        ..Log::default()
    }));
    {
        let config = config(dir.path(), &clock, "agent-a", EmitterKind::Worker);
        let mut pipeline = Pipeline::open(config, Box::new(Recorder(log.clone()))).unwrap();
        pipeline.emit(approval("req-1")).unwrap();
        pipeline.emit(approval("req-2")).unwrap();
    }

    log.lock().unwrap().fail = false;
    let config = config(dir.path(), &clock, "agent-a", EmitterKind::Worker);
    let mut pipeline = Pipeline::open(config, Box::new(Recorder(log.clone()))).unwrap();
    assert_eq!(pipeline.head().map(|head| head.seq), Some(2));
    assert_eq!(pipeline.flush().unwrap(), 2);
    assert_eq!(pipeline.emit(approval("req-3")).unwrap(), 3);

    let payloads = log.lock().unwrap().payloads.clone();
    let seqs: Vec<u64> = payloads.iter().map(|payload| decode(payload).seq).collect();
    assert_eq!(seqs, vec![1, 2, 3]);
}

#[test]
fn req_0013_047_repeated_peer_rejections_become_one_summary() {
    let dir = tempfile::tempdir().unwrap();
    let clock = clock();
    let log = Arc::new(Mutex::new(Log::default()));
    let config = config(dir.path(), &clock, "gateway", EmitterKind::Gateway);
    let mut pipeline = Pipeline::open(config, Box::new(Recorder(log.clone()))).unwrap();

    for _ in 0..3 {
        pipeline.peer_rejected("agent-x", "bad_token");
    }
    assert_eq!(pipeline.shutdown().unwrap(), 1);

    let payloads = log.lock().unwrap().payloads.clone();
    assert_eq!(payloads.len(), 1);
    let event = decode(payloads.first().unwrap());
    assert_eq!(event.r#type, AuditEventType::PeerRejected as i32);
    let json: Value = serde_json::to_value(&event).unwrap();
    assert_eq!(json["details"]["peer"], "agent-x");
    assert_eq!(json["details"]["count"], "3");
}
