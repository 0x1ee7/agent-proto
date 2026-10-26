//! Pure operator state: agents, tasks, approvals, audit tail, and link health.
//!
//! Nothing here touches the bus or the clock. Callers pass `now_ms` so every
//! transition is scriptable in tests.

pub mod agents;
pub mod approvals;
pub mod audit;
pub mod link;
pub mod tasks;

use a2a_client::Event;
use a2a_types::proto::a2a as pb;
use a2a_types::proto::a2abus;

use self::agents::{Observed, Roster};
use self::approvals::{DecisionOutcome, Queue};
use self::audit::Tail;
use self::link::Connection;
use self::tasks::Tasks;

/// Everything the console knows, in one place.
#[derive(Debug, Default)]
pub struct Board {
    pub link: Connection,
    pub agents: Roster,
    pub tasks: Tasks,
    pub approvals: Queue,
    pub audit: Tail,
}

impl Board {
    /// A successful directory sync: the console's heartbeat (RFC-0012 0012-050).
    pub fn heartbeat(&mut self, now_ms: u64) {
        self.link.ok(now_ms);
    }

    /// A failed directory sync; the reason is kept for the banner.
    pub fn link_failed(&mut self, reason: &str) {
        self.link.failed(reason);
    }

    /// Replaces the roster with the latest directory listing.
    pub fn apply_agents(&mut self, now_ms: u64, observed: Vec<Observed>) {
        self.agents.replace(now_ms, observed);
    }

    /// Applies task snapshots read from one agent.
    pub fn apply_tasks(&mut self, now_ms: u64, agent: &str, snapshots: &[pb::Task]) {
        for task in snapshots {
            self.apply_snapshot(now_ms, agent, task);
        }
    }

    /// Applies one streamed event from `agent`.
    pub fn apply_event(&mut self, now_ms: u64, agent: &str, event: &Event) {
        let task_id = event.task.as_str();
        self.tasks.track_seq(task_id, agent, event.seq, now_ms);
        match &event.response.payload {
            Some(pb::stream_response::Payload::Task(task)) => {
                self.apply_snapshot(now_ms, agent, task);
            }
            Some(pb::stream_response::Payload::StatusUpdate(update)) => {
                let Some(status) = update.status.as_ref() else {
                    return;
                };
                self.tasks.set_status(
                    task_id,
                    agent,
                    &update.context_id,
                    status,
                    update.metadata.as_ref(),
                    now_ms,
                );
                let as_task = pb::Task {
                    id: update.task_id.clone(),
                    context_id: update.context_id.clone(),
                    status: Some(status.clone()),
                    artifacts: Vec::new(),
                    history: Vec::new(),
                    metadata: update.metadata.clone(),
                };
                self.apply_approval(now_ms, agent, &as_task);
            }
            Some(pb::stream_response::Payload::ArtifactUpdate(_)) => {
                self.tasks.count_artifact(task_id, agent, now_ms);
            }
            _ => {}
        }
    }

    /// Records one audit event from the tail feed (RFC-0013).
    pub fn apply_audit(&mut self, event: &a2abus::AuditEvent) {
        if let Some(parent) = parent_of(event) {
            self.tasks.set_parent(&event.task_id, &parent);
        }
        if let Ok(kind) = a2abus::AuditEventType::try_from(event.r#type) {
            if kind == a2abus::AuditEventType::BudgetTripped {
                self.tasks.set_trip(&event.task_id, &event.reason);
            }
        }
        self.audit.push(audit::row_from(event));
    }

    /// Time-driven transitions: expiry of approvals and link health.
    pub fn tick(&mut self, now_ms: u64) {
        self.link.tick(now_ms);
        self.approvals.expire(now_ms);
    }

    fn apply_snapshot(&mut self, now_ms: u64, agent: &str, task: &pb::Task) {
        self.tasks.upsert_snapshot(agent, task, now_ms);
        self.apply_approval(now_ms, agent, task);
    }

    fn apply_approval(&mut self, now_ms: u64, agent: &str, task: &pb::Task) {
        match a2a_client::approval::PendingApproval::from_task(task) {
            Ok(Some(pending)) => self.approvals.observe(agent, pending, now_ms),
            _ => {
                let waiting = task
                    .status
                    .as_ref()
                    .is_some_and(|s| s.state == pb::TaskState::InputRequired as i32);
                if !waiting {
                    self.approvals.close_task(&task.id, now_ms);
                }
            }
        }
    }
}

/// The hop before `event.task_id` in the audit chain is its delegation parent.
fn parent_of(event: &a2abus::AuditEvent) -> Option<String> {
    let index = event
        .chain
        .iter()
        .position(|hop| hop.task_id == event.task_id)?;
    let parent = index.checked_sub(1)?;
    let hop = event.chain.get(parent)?;
    if hop.task_id.is_empty() {
        None
    } else {
        Some(hop.task_id.clone())
    }
}

/// Maps the decision outcome of an acknowledged or failed `decide` call.
pub fn outcome_text(outcome: &DecisionOutcome) -> String {
    match outcome {
        DecisionOutcome::Accepted => "accepted".to_owned(),
        DecisionOutcome::Refused(reason) => format!("refused: {reason}"),
        DecisionOutcome::Unknown(reason) => format!("unknown: {reason}"),
    }
}
