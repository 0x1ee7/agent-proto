//! Approval queue (RFC-0006 approval flow, RFC-0012 4.10.1 approvals screen).

use std::collections::BTreeMap;

use a2a_client::approval::PendingApproval;
use a2a_core::approval::{ALLOW_ONCE_ID, REJECT_ONCE_ID};
use a2a_types::proto::a2abus;

/// Minimum gap between approval notices from one agent (0012 flood throttle).
pub const NOTICE_INTERVAL_MS: u64 = 5_000;
/// Open approvals from one agent at or above this count show the burst banner.
pub const BURST_THRESHOLD: usize = 3;
/// Dwell before `y` is accepted on the approval card (0012-046).
pub const DWELL_MS: u64 = 2_000;

/// Result of one decision call, reported by the driver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionOutcome {
    /// The bus acknowledged the decision.
    Accepted,
    /// The decision was refused before or at the bus (signing, approval check, wire).
    Refused(String),
    /// No acknowledgement: the decision may or may not have been applied.
    Unknown(String),
}

/// Lifecycle of one approval as seen by this console.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Open,
    /// Decision sent; waiting for the bus reply. Never resent.
    Sending {
        decision_id: String,
        option_id: String,
    },
    /// Reply lost or ambiguous; the operator must check the task state.
    Unknown {
        reason: String,
    },
    Allowed,
    Denied,
    Refused {
        reason: String,
    },
    /// Expired locally; denied with reason `expired` (0012-045).
    Expired,
    /// The task left INPUT_REQUIRED without this console deciding.
    Closed,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub agent: String,
    pub pending: PendingApproval,
    pub seen_ms: u64,
    pub phase: Phase,
    /// Operator note: local only, `ApprovalDecision` has no note field.
    pub note: Option<String>,
}

impl Entry {
    #[must_use]
    pub fn request_id(&self) -> &str {
        &self.pending.request().request_id
    }

    #[must_use]
    pub fn request(&self) -> &a2abus::ApprovalRequest {
        self.pending.request()
    }

    #[must_use]
    pub fn is_open(&self) -> bool {
        matches!(self.phase, Phase::Open)
    }

    /// Expiry in ms since the Unix epoch, when the request carries one.
    #[must_use]
    pub fn expires_ms(&self) -> Option<u64> {
        expires_ms(self.request())
    }
}

/// Converts `expires_at` to epoch milliseconds; negative or unset times yield `None`.
#[must_use]
pub fn expires_ms(request: &a2abus::ApprovalRequest) -> Option<u64> {
    let ts = request.expires_at.as_ref()?;
    let secs = u64::try_from(ts.seconds).ok()?;
    let millis = u64::try_from(ts.nanos.max(0) / 1_000_000).ok()?;
    secs.checked_mul(1_000)?.checked_add(millis)
}

/// Option id for `kind`, when the request offers it.
#[must_use]
pub fn option_for(
    request: &a2abus::ApprovalRequest,
    kind: a2abus::ApprovalOptionKind,
) -> Option<&str> {
    request
        .options
        .iter()
        .find(|o| o.kind == kind as i32)
        .map(|o| o.option_id.as_str())
}

/// Allow-once option id, if the request offers it. Rejects and allow-always
/// are never chosen by a shortcut key.
#[must_use]
pub fn allow_once_id(request: &a2abus::ApprovalRequest) -> Option<&str> {
    option_for(request, a2abus::ApprovalOptionKind::AllowOnce)
}

/// Reject-once option id, falling back to the protocol constant.
#[must_use]
pub fn reject_once_id(request: &a2abus::ApprovalRequest) -> &str {
    option_for(request, a2abus::ApprovalOptionKind::RejectOnce).unwrap_or(REJECT_ONCE_ID)
}

/// The protocol constant for allow-once, kept for callers that build decisions directly.
#[must_use]
pub const fn default_allow_id() -> &'static str {
    ALLOW_ONCE_ID
}

#[derive(Debug, Default)]
pub struct Queue {
    entries: Vec<Entry>,
    last_notice_ms: BTreeMap<String, u64>,
    notice: Option<String>,
    suppressed: u32,
}

impl Queue {
    /// Adds a newly seen approval. Repeats by request id are ignored.
    pub fn observe(&mut self, agent: &str, pending: PendingApproval, now_ms: u64) {
        let request_id = pending.request().request_id.clone();
        if self.entries.iter().any(|e| e.request_id() == request_id) {
            return;
        }
        self.entries.push(Entry {
            agent: agent.to_owned(),
            pending,
            seen_ms: now_ms,
            phase: Phase::Open,
            note: None,
        });
        self.notify(agent, now_ms);
    }

    fn notify(&mut self, agent: &str, now_ms: u64) {
        let due = self
            .last_notice_ms
            .get(agent)
            .is_none_or(|last| now_ms.saturating_sub(*last) >= NOTICE_INTERVAL_MS);
        if due {
            self.last_notice_ms.insert(agent.to_owned(), now_ms);
            self.notice = Some(format!("approval requested by {agent}"));
            self.suppressed = 0;
        } else {
            self.suppressed = self.suppressed.saturating_add(1);
        }
    }

    /// Ends open approvals on `task_id` once the task is no longer waiting.
    pub fn close_task(&mut self, task_id: &str, _now_ms: u64) {
        for entry in &mut self.entries {
            if entry.is_open() && entry.pending.task_id().as_str() == task_id {
                entry.phase = Phase::Closed;
            }
        }
    }

    /// Expires open approvals whose deadline has passed.
    pub fn expire(&mut self, now_ms: u64) {
        for entry in &mut self.entries {
            if entry.is_open() && entry.expires_ms().is_some_and(|exp| now_ms >= exp) {
                entry.phase = Phase::Expired;
            }
        }
    }

    /// Moves an open approval to `Sending`. Returns false when it is not open.
    pub fn begin(&mut self, request_id: &str, decision_id: &str, option_id: &str) -> bool {
        match self.find_mut(request_id) {
            Some(entry) if entry.is_open() => {
                entry.phase = Phase::Sending {
                    decision_id: decision_id.to_owned(),
                    option_id: option_id.to_owned(),
                };
                true
            }
            _ => false,
        }
    }

    /// Applies a decision outcome, matched by decision id so stale replies are ignored.
    pub fn resolve(&mut self, request_id: &str, decision_id: &str, outcome: DecisionOutcome) {
        let Some(entry) = self.find_mut(request_id) else {
            return;
        };
        let Phase::Sending {
            decision_id: pending_id,
            option_id,
        } = &entry.phase
        else {
            return;
        };
        if pending_id != decision_id {
            return;
        }
        let allow = option_id == ALLOW_ONCE_ID
            || entry.pending.request().options.iter().any(|o| {
                &o.option_id == option_id
                    && o.kind == a2abus::ApprovalOptionKind::AllowAlways as i32
            });
        entry.phase = match outcome {
            DecisionOutcome::Accepted if allow => Phase::Allowed,
            DecisionOutcome::Accepted => Phase::Denied,
            DecisionOutcome::Refused(reason) => Phase::Refused { reason },
            DecisionOutcome::Unknown(reason) => Phase::Unknown { reason },
        };
    }

    /// Local denial with reason `expired` when the operator decides too late (0012-045).
    pub fn set_note(&mut self, request_id: &str, note: Option<String>) {
        if let Some(entry) = self.find_mut(request_id) {
            entry.note = note;
        }
    }

    /// Open approvals from `agent`.
    #[must_use]
    pub fn open_count(&self, agent: &str) -> usize {
        self.entries
            .iter()
            .filter(|e| e.agent == agent && e.is_open())
            .count()
    }

    /// Burst banner (0012 flood control) for an agent with many open approvals.
    #[must_use]
    pub fn burst(&self, agent: &str) -> Option<usize> {
        let count = self.open_count(agent);
        (count >= BURST_THRESHOLD).then_some(count)
    }

    /// Latest notice line and how many notices were suppressed since it.
    #[must_use]
    pub fn notice(&self) -> Option<(&str, u32)> {
        self.notice.as_deref().map(|text| (text, self.suppressed))
    }

    /// Entries, oldest first.
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    #[must_use]
    pub fn get(&self, request_id: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.request_id() == request_id)
    }

    fn find_mut(&mut self, request_id: &str) -> Option<&mut Entry> {
        self.entries
            .iter_mut()
            .find(|e| e.request_id() == request_id)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
pub(crate) mod fixtures {
    use super::*;

    pub(crate) fn option(id: &str, kind: a2abus::ApprovalOptionKind) -> a2abus::ApprovalOption {
        a2abus::ApprovalOption {
            option_id: id.to_owned(),
            name: id.to_owned(),
            kind: kind as i32,
        }
    }

    pub(crate) fn request(id: &str, expires_secs: i64) -> a2abus::ApprovalRequest {
        a2abus::ApprovalRequest {
            request_id: id.to_owned(),
            task_id: format!("task-{id}"),
            tool: "shell".to_owned(),
            working_directory: "/work/app".to_owned(),
            workspace_mode: "per-task".to_owned(),
            options: vec![
                option(ALLOW_ONCE_ID, a2abus::ApprovalOptionKind::AllowOnce),
                option(REJECT_ONCE_ID, a2abus::ApprovalOptionKind::RejectOnce),
            ],
            expires_at: Some(pbjson_types::Timestamp {
                seconds: expires_secs,
                nanos: 0,
            }),
            ..Default::default()
        }
    }

    pub(crate) fn pending(id: &str, expires_secs: i64) -> PendingApproval {
        pending_from(&request(id, expires_secs))
    }

    pub(crate) fn pending_from(request: &a2abus::ApprovalRequest) -> PendingApproval {
        let task = a2a_types::proto::a2a::Task {
            id: format!("task-{}", request.request_id),
            context_id: "ctx".to_owned(),
            status: Some(a2a_types::proto::a2a::TaskStatus {
                state: a2a_types::proto::a2a::TaskState::InputRequired as i32,
                message: Some(a2a_types::proto::a2a::Message {
                    metadata: Some(approval_metadata(request)),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        PendingApproval::from_task(&task)
            .expect("parse")
            .expect("pending")
    }

    fn approval_metadata(request: &a2abus::ApprovalRequest) -> pbjson_types::Struct {
        let value = serde_json::to_value(request).expect("json");
        let json = serde_json::json!({ a2a_types::vocab::ext::APPROVAL: value });
        serde_json::from_value(json).expect("struct")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::fixtures::{pending, request};
    use super::*;

    #[test]
    fn req_0012_046_repeat_observations_are_deduplicated() {
        let mut queue = Queue::default();
        queue.observe("omp", pending("r1", 100), 1_000);
        queue.observe("omp", pending("r1", 100), 1_500);
        assert_eq!(queue.entries().len(), 1);
    }

    #[test]
    fn req_0012_048_flood_is_throttled_and_burst_banner_appears() {
        let mut queue = Queue::default();
        queue.observe("omp", pending("a", 100), 1_000);
        queue.observe("omp", pending("b", 100), 1_100);
        queue.observe("omp", pending("c", 100), 1_200);
        let (text, suppressed) = queue.notice().expect("notice");
        assert_eq!(text, "approval requested by omp");
        assert_eq!(suppressed, 2);
        assert_eq!(queue.burst("omp"), Some(3));
        assert_eq!(queue.burst("other"), None);
    }

    #[test]
    fn req_0006_057_expired_approvals_are_closed_locally() {
        let mut queue = Queue::default();
        queue.observe("omp", pending("e", 10), 1_000);
        queue.expire(9_999);
        assert!(queue.get("e").expect("entry").is_open());
        queue.expire(10_000);
        assert_eq!(queue.get("e").expect("entry").phase, Phase::Expired);
    }

    #[test]
    fn req_0006_004_decision_resolves_only_for_matching_decision_id() {
        let mut queue = Queue::default();
        queue.observe("omp", pending("d", 100), 1_000);
        assert!(queue.begin("d", "id-1", ALLOW_ONCE_ID));
        queue.resolve("d", "stale", DecisionOutcome::Accepted);
        assert!(matches!(
            queue.get("d").expect("entry").phase,
            Phase::Sending { .. }
        ));
        queue.resolve("d", "id-1", DecisionOutcome::Accepted);
        assert_eq!(queue.get("d").expect("entry").phase, Phase::Allowed);
    }

    #[test]
    fn req_0006_004_unacknowledged_decision_is_unknown_not_retried() {
        let mut queue = Queue::default();
        queue.observe("omp", pending("u", 100), 1_000);
        assert!(queue.begin("u", "id-2", ALLOW_ONCE_ID));
        queue.resolve("u", "id-2", DecisionOutcome::Unknown("timeout".to_owned()));
        assert!(matches!(
            queue.get("u").expect("entry").phase,
            Phase::Unknown { .. }
        ));
        assert!(!queue.begin("u", "id-3", ALLOW_ONCE_ID));
    }

    #[test]
    fn req_0006_004_task_leaving_input_required_closes_the_approval() {
        let mut queue = Queue::default();
        queue.observe("omp", pending("k", 100), 1_000);
        queue.close_task("task-k", 1_100);
        assert_eq!(queue.get("k").expect("entry").phase, Phase::Closed);
    }

    #[test]
    fn req_0006_004_allow_once_option_is_looked_up_from_the_request() {
        let req = request("o", 100);
        assert_eq!(allow_once_id(&req), Some(ALLOW_ONCE_ID));
        assert_eq!(reject_once_id(&req), REJECT_ONCE_ID);
        assert_eq!(expires_ms(&req), Some(100_000));
    }
}
