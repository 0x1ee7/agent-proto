//! Audit adapter: maps worker audit notes to a2a-audit records ([0013-007], [0013-008]).
//!
//! Every note stays in the buffer returned by `take_audit`. A note whose reason has an
//! RFC-0013 table 4.2 type is also recorded through the attached [`AuditRecorder`]. A record
//! that cannot be built or emitted is counted and buffered as `AUDIT_EMIT_FAILED`; it is never
//! dropped silently ([0013-015]).

use std::fmt::Debug;
use std::sync::{Arc, Mutex, PoisonError};

use a2a_audit::{AuditError, EventBuilder, Pipeline};
use a2a_core::approval::MAX_OPEN_PER_TASK;
use a2a_core::task::TaskStore;
use a2a_types::ids::{AgentId, ContextId, TaskId};
use a2a_types::proto::a2abus::AuditEventType;
use bus_api::bus::Bus;

use crate::backend::Backend;
use crate::worker::{AuditNote, Worker};

/// Destination for mapped audit records. [`Pipeline`] implements it.
pub trait AuditRecorder: Send + Debug {
    /// Seals, spools and publishes one record ([0013-014]). Publish failures stay spooled and
    /// are retried by the pipeline; they are not reported here.
    ///
    /// # Errors
    /// Returns [`AuditError`] when the record cannot be sealed or spooled.
    fn record(&mut self, event: EventBuilder) -> Result<(), AuditError>;
}

impl AuditRecorder for Pipeline {
    fn record(&mut self, event: EventBuilder) -> Result<(), AuditError> {
        self.emit(event).map(|_| ())
    }
}

/// How a worker reason code is audited ([0013-007], RFC-0013 table 4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Class {
    /// Output arrived after its turn or task closed (`LATE_OUTPUT_DROPPED`).
    LateOutput,
    /// The per-task approval cap was reached (`APPROVAL_FLOOD`).
    ApprovalFlood,
    /// A decision named an approval that is not open (`APPROVAL_NOT_APPLICABLE`).
    ApprovalNotApplicable,
    /// An operational failure with no table 4.2 type: buffered and logged only.
    LogOnly,
}

/// Classifies a reason by its leading token: `CODE` or `CODE: detail` or `CODE {debug}: detail`.
pub(crate) fn classify(reason: &str) -> Class {
    let token = reason.split([':', ' ']).next().unwrap_or_default();
    match token {
        "LATE_BACKEND_OUTPUT" | "LATE_ARTIFACT_DELTA" | "LATE_USAGE_SAMPLE" => Class::LateOutput,
        "APPROVAL_FLOOD" => Class::ApprovalFlood,
        "APPROVAL_NOT_FOUND" => Class::ApprovalNotApplicable,
        _ => Class::LogOnly,
    }
}

/// Task facts read from the task engine when a note is audited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskFacts {
    pub(crate) context_id: String,
    /// Wire name of the task state, for example `TASK_STATE_COMPLETED`.
    pub(crate) state: String,
}

/// Builds the record for one note. Returns `Ok(None)` for log-only reasons.
///
/// # Errors
/// Returns a static reason when a mapped note cannot form a valid record; the caller counts it.
pub(crate) fn build_record(
    note: &AuditNote,
    agent: &AgentId,
    facts: Option<&TaskFacts>,
) -> Result<Option<EventBuilder>, &'static str> {
    let class = classify(&note.reason);
    if class == Class::LogOnly {
        return Ok(None);
    }
    let task_str = note.task_id.as_deref().ok_or("audit record has no task")?;
    let task = TaskId::parse(task_str).map_err(|_| "task id is not a valid identifier")?;
    let event = match class {
        Class::LateOutput => {
            let facts = facts.ok_or("late output for a task that is not in the store")?;
            EventBuilder::new(AuditEventType::LateOutputDropped)
                .detail_str("task_id", task_str)
                .detail_str("state", &facts.state)
        }
        Class::ApprovalFlood => EventBuilder::new(AuditEventType::ApprovalFlood)
            .detail_str("task_id", task_str)
            .detail_count("open_count", MAX_OPEN_PER_TASK as u64),
        Class::ApprovalNotApplicable => {
            let request = note
                .reason
                .strip_prefix("APPROVAL_NOT_FOUND: ")
                .ok_or("approval reason has no request id")?;
            EventBuilder::new(AuditEventType::ApprovalNotApplicable)
                .detail_str("request_id", request)
        }
        Class::LogOnly => return Ok(None),
    };
    let mut event = event.task(task).agent(agent.clone());
    if let Some(facts) = facts {
        let context = ContextId::parse(&facts.context_id)
            .map_err(|_| "context id is not a valid identifier")?;
        event = event.context(context);
    }
    Ok(Some(event))
}

/// Records one note through `sink`. Returns the failure text when the record is lost.
pub(crate) fn record_note(
    sink: &Mutex<dyn AuditRecorder>,
    note: &AuditNote,
    agent: &AgentId,
    facts: Option<&TaskFacts>,
) -> Result<(), String> {
    let event = match build_record(note, agent, facts) {
        Ok(Some(event)) => event,
        Ok(None) => return Ok(()),
        Err(reason) => return Err(reason.to_owned()),
    };
    let mut guard = sink.lock().unwrap_or_else(PoisonError::into_inner);
    guard.record(event).map_err(|err| err.to_string())
}

impl<B: Bus, D: Backend, S: TaskStore> Worker<B, D, S> {
    /// Number of audit records lost because they could not be built or emitted ([0013-015]).
    #[must_use]
    pub fn audit_failures(&self) -> u64 {
        self.audit_failures
    }

    /// Attaches the record sink and replays notes buffered before the sink existed.
    pub(crate) fn attach_audit(&mut self, sink: Arc<Mutex<dyn AuditRecorder>>) {
        self.audit_sink = Some(sink);
        let buffered = self.audit.clone();
        for note in &buffered {
            self.record_or_count(note);
        }
    }

    /// Records `note` through the attached sink, counting and buffering any lost record.
    pub(crate) fn record_or_count(&mut self, note: &AuditNote) {
        let Some(sink) = self.audit_sink.clone() else {
            return;
        };
        if classify(&note.reason) == Class::LogOnly {
            return;
        }
        let facts = note
            .task_id
            .as_deref()
            .and_then(|task| self.task_facts(task));
        if let Err(error) = record_note(&sink, note, &self.config.agent, facts.as_ref()) {
            self.audit_failures = self.audit_failures.saturating_add(1);
            self.audit.push(AuditNote {
                task_id: note.task_id.clone(),
                reason: format!("AUDIT_EMIT_FAILED: {error}"),
            });
        }
    }

    fn task_facts(&self, task: &str) -> Option<TaskFacts> {
        let owner = self.tasks.task_owner(task)?;
        let view = self.tasks.task(&owner.origin, task).ok()?;
        Some(TaskFacts {
            context_id: view.context_id,
            state: view.state.as_str_name().to_owned(),
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {

    use a2a_audit::AuditError;

    use super::*;

    #[derive(Debug)]
    struct Accept;

    impl AuditRecorder for Accept {
        fn record(&mut self, _event: EventBuilder) -> Result<(), AuditError> {
            Ok(())
        }
    }

    #[derive(Debug)]
    struct Refuse;

    impl AuditRecorder for Refuse {
        fn record(&mut self, _event: EventBuilder) -> Result<(), AuditError> {
            Err(AuditError::Identity("spool full".to_owned()))
        }
    }

    fn agent() -> AgentId {
        "agent-a".parse().expect("agent id literal is valid")
    }

    fn facts() -> TaskFacts {
        TaskFacts {
            context_id: "ctx-1".to_owned(),
            state: "TASK_STATE_COMPLETED".to_owned(),
        }
    }

    fn note(task: Option<&str>, reason: &str) -> AuditNote {
        AuditNote {
            task_id: task.map(str::to_owned),
            reason: reason.to_owned(),
        }
    }

    /// [0013-007]: every reason code the worker and core emit is classified: the table 4.2
    /// codes map to their type, every other code is log-only.
    #[test]
    fn req_0013_007_maps_every_worker_reason_code() {
        let mapped = [
            ("LATE_BACKEND_OUTPUT", Class::LateOutput),
            ("LATE_BACKEND_OUTPUT: turn is not open", Class::LateOutput),
            ("LATE_ARTIFACT_DELTA", Class::LateOutput),
            ("LATE_USAGE_SAMPLE", Class::LateOutput),
            ("APPROVAL_FLOOD", Class::ApprovalFlood),
            ("APPROVAL_NOT_FOUND: req-1", Class::ApprovalNotApplicable),
        ];
        for (reason, class) in mapped {
            assert_eq!(classify(reason), class, "reason {reason}");
        }
        let log_only = [
            "BACKEND_START_FAILED: closed",
            "CANCEL_FAILED: closed",
            "SHUTDOWN_FAILED: closed",
            "BACKEND_FAULT Crash: lost",
            "DUPLICATE_BACKEND_EVENT",
            "BACKEND_EVENT_GAP: turn killed",
            "INPUT_REQUIRED_UNSUPPORTED",
            "RESOLVE_PERMISSION_FAILED: closed",
            "APPROVAL_NOT_ENCODABLE: bad",
            "EVENT_NOT_ENCODABLE: bad",
            "STALE_SUBMISSION",
            "REPLAY_PUBLISH_FAILED: closed",
            "UNTRUSTED_INPUT_REJECTED: nul",
            "AUDIT_EMIT_FAILED: spool full",
        ];
        for reason in log_only {
            assert_eq!(classify(reason), Class::LogOnly, "reason {reason}");
        }
    }

    /// [0013-008]: late-output records carry `task_id` and the task state as decimal-free
    /// strings, and the task and context correlation fields.
    #[test]
    fn req_0013_008_late_output_record_has_state_detail() {
        let event = build_record(
            &note(Some("task-1"), "LATE_BACKEND_OUTPUT"),
            &agent(),
            Some(&facts()),
        );
        let Ok(Some(event)) = event else {
            panic!("late output must produce a record");
        };
        let (ty, _, details) = event.into_parts();
        assert_eq!(ty, AuditEventType::LateOutputDropped);
        assert_eq!(details["task_id"], "task-1");
        assert_eq!(details["state"], "TASK_STATE_COMPLETED");
    }

    /// [0013-008]: a late output for a task missing from the store cannot form a record.
    #[test]
    fn req_0013_008_late_output_without_task_facts_is_an_error() {
        let result = build_record(&note(Some("task-1"), "LATE_USAGE_SAMPLE"), &agent(), None);
        assert!(result.is_err());
    }

    /// [0013-008]: an approval flood record names the task and the open count at the cap.
    #[test]
    fn req_0013_008_approval_flood_record_has_open_count() {
        let event = build_record(&note(Some("task-1"), "APPROVAL_FLOOD"), &agent(), None);
        let Ok(Some(event)) = event else {
            panic!("flood must produce a record");
        };
        let (ty, _, details) = event.into_parts();
        assert_eq!(ty, AuditEventType::ApprovalFlood);
        assert_eq!(
            details["open_count"],
            MAX_OPEN_PER_TASK.to_string().as_str()
        );
    }

    /// [0013-008]: a not-found decision names the request it referred to.
    #[test]
    fn req_0013_008_approval_not_found_record_has_request_id() {
        let event = build_record(
            &note(Some("task-1"), "APPROVAL_NOT_FOUND: req-7"),
            &agent(),
            None,
        );
        let Ok(Some(event)) = event else {
            panic!("not-found must produce a record");
        };
        let (ty, _, details) = event.into_parts();
        assert_eq!(ty, AuditEventType::ApprovalNotApplicable);
        assert_eq!(details["request_id"], "req-7");
    }

    /// [0013-015]: a record the sink refuses is reported, so the worker can count it.
    #[test]
    fn req_0013_015_emit_failure_is_reported() {
        let sink = Mutex::new(Refuse);
        let result = record_note(
            &sink,
            &note(Some("task-1"), "LATE_BACKEND_OUTPUT"),
            &agent(),
            Some(&facts()),
        );
        assert!(result.is_err_and(|err| err.contains("spool full")));
    }

    /// [0013-015]: an accepted record and a log-only note both succeed without a failure.
    #[test]
    fn req_0013_015_accepted_and_log_only_records_succeed() {
        let sink = Mutex::new(Accept);
        let accepted = record_note(
            &sink,
            &note(Some("task-1"), "APPROVAL_FLOOD"),
            &agent(),
            None,
        );
        let log_only = record_note(
            &sink,
            &note(Some("task-1"), "CANCEL_FAILED: x"),
            &agent(),
            None,
        );
        assert!(accepted.is_ok());
        assert!(log_only.is_ok());
    }

    /// [0013-015]: a mapped note without a task id cannot be recorded and is reported.
    #[test]
    fn req_0013_015_mapped_note_without_task_is_reported() {
        let sink = Mutex::new(Accept);
        let result = record_note(
            &sink,
            &note(None, "LATE_BACKEND_OUTPUT"),
            &agent(),
            Some(&facts()),
        );
        assert!(result.is_err());
    }
}
