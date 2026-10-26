//! Task rows (RFC-0012 4.10.1 tasks and task screens, RFC-0006 budgets).

use std::collections::{BTreeMap, HashMap};

use a2a_types::proto::a2a as pb;
use serde_json::Value;

use crate::redact;

/// Ext URI carrying `Budget` (limits) and `BudgetUsage` (consumption), RFC-0006 [0006-041].
pub const BUDGET_EXT: &str = "urn:a2abus:ext:budget:1";

/// Maximum delegation depth walked when computing display depth.
const MAX_DEPTH: usize = 32;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Budget {
    pub max_tokens: Option<u64>,
    pub max_wall_ms: Option<u64>,
    pub tokens_used: Option<u64>,
    pub wall_ms_used: Option<u64>,
}

impl Budget {
    /// Remaining tokens, when both the limit and the usage are known.
    #[must_use]
    pub fn tokens_remaining(&self) -> Option<u64> {
        Some(self.max_tokens?.saturating_sub(self.tokens_used?))
    }

    /// Remaining wall time in ms, when both the limit and the usage are known.
    #[must_use]
    pub fn wall_ms_remaining(&self) -> Option<u64> {
        Some(self.max_wall_ms?.saturating_sub(self.wall_ms_used?))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRow {
    pub id: String,
    pub agent: String,
    pub context_id: String,
    pub state: pb::TaskState,
    pub status_text: String,
    pub last_seq: u64,
    /// Sequence gaps observed on the task stream (RFC-0006 sequence ext).
    pub missing_seqs: u64,
    pub artifacts: u32,
    pub budget: Budget,
    pub last_trip: Option<String>,
    pub updated_ms: u64,
}

impl TaskRow {
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.state,
            pb::TaskState::Completed | pb::TaskState::Failed | pb::TaskState::Canceled
        )
    }

    #[must_use]
    pub fn state_label(&self) -> &'static str {
        state_label(self.state)
    }
}

#[derive(Debug, Default)]
pub struct Tasks {
    rows: BTreeMap<String, TaskRow>,
    parents: HashMap<String, String>,
}

impl Tasks {
    /// Full snapshot from `ListTasks` or a `Task` payload.
    pub fn upsert_snapshot(&mut self, agent: &str, task: &pb::Task, now_ms: u64) {
        let (state, text) = task
            .status
            .as_ref()
            .map_or((pb::TaskState::Unspecified, String::new()), |s| {
                (status_state(s), status_text(s))
            });
        let row = self.entry(&task.id, agent, &task.context_id, now_ms);
        row.state = state;
        row.status_text = text;
        row.artifacts = u32::try_from(task.artifacts.len()).unwrap_or(u32::MAX);
        apply_budget(&mut row.budget, task.metadata.as_ref());
        if let Some(status) = task.status.as_ref() {
            apply_budget(
                &mut row.budget,
                status.message.as_ref().and_then(|m| m.metadata.as_ref()),
            );
        }
    }

    /// One status update from the stream.
    pub fn set_status(
        &mut self,
        task_id: &str,
        agent: &str,
        context_id: &str,
        status: &pb::TaskStatus,
        metadata: Option<&pbjson_types::Struct>,
        now_ms: u64,
    ) {
        let row = self.entry(task_id, agent, context_id, now_ms);
        row.state = status_state(status);
        row.status_text = status_text(status);
        apply_budget(&mut row.budget, metadata);
    }

    pub fn count_artifact(&mut self, task_id: &str, agent: &str, now_ms: u64) {
        let row = self.entry(task_id, agent, "", now_ms);
        row.artifacts = row.artifacts.saturating_add(1);
    }

    /// Notes a sequence number. A jump past `last_seq + 1` counts as missing messages.
    pub fn track_seq(&mut self, task_id: &str, agent: &str, seq: u64, now_ms: u64) {
        let row = self.entry(task_id, agent, "", now_ms);
        if row.last_seq != 0 && seq > row.last_seq.saturating_add(1) {
            row.missing_seqs = row.missing_seqs.saturating_add(seq - row.last_seq - 1);
        }
        if seq > row.last_seq {
            row.last_seq = seq;
        }
    }

    pub fn set_parent(&mut self, child: &str, parent: &str) {
        if !child.is_empty() && !parent.is_empty() && child != parent {
            self.parents
                .entry(child.to_owned())
                .or_insert_with(|| parent.to_owned());
        }
    }

    pub fn set_trip(&mut self, task_id: &str, reason: &str) {
        if let Some(row) = self.rows.get_mut(task_id) {
            row.last_trip = Some(redact::summary(reason));
        }
    }

    /// Delegation parent recorded from the audit chain.
    #[must_use]
    pub fn parent_of(&self, task_id: &str) -> Option<&str> {
        self.parents.get(task_id).map(String::as_str)
    }

    /// Display depth in the delegation tree (root = 0). Cycles and very deep
    /// chains are cut off at `MAX_DEPTH`.
    #[must_use]
    pub fn depth(&self, task_id: &str) -> usize {
        let mut depth = 0;
        let mut current = task_id;
        while let Some(parent) = self.parent_of(current) {
            depth += 1;
            if depth >= MAX_DEPTH || parent == task_id {
                break;
            }
            current = parent;
        }
        depth
    }

    pub fn get(&self, task_id: &str) -> Option<&TaskRow> {
        self.rows.get(task_id)
    }

    /// Tasks ordered by context, then delegation depth, then id.
    pub fn ordered(&self) -> Vec<&TaskRow> {
        let mut rows: Vec<&TaskRow> = self.rows.values().collect();
        rows.sort_by(|a, b| {
            a.context_id
                .cmp(&b.context_id)
                .then(self.depth(&a.id).cmp(&self.depth(&b.id)))
                .then(a.id.cmp(&b.id))
        });
        rows
    }

    pub fn non_terminal_ids(&self) -> impl Iterator<Item = (&str, &str)> {
        self.rows
            .values()
            .filter(|row| !row.is_terminal())
            .map(|row| (row.agent.as_str(), row.id.as_str()))
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    fn entry(&mut self, task_id: &str, agent: &str, context_id: &str, now_ms: u64) -> &mut TaskRow {
        let row = self
            .rows
            .entry(task_id.to_owned())
            .or_insert_with(|| TaskRow {
                id: redact::truncate_chars(&redact::escape(task_id), 128),
                agent: redact::truncate_chars(&redact::escape(agent), 128),
                context_id: redact::truncate_chars(&redact::escape(context_id), 128),
                state: pb::TaskState::Unspecified,
                status_text: String::new(),
                last_seq: 0,
                missing_seqs: 0,
                artifacts: 0,
                budget: Budget::default(),
                last_trip: None,
                updated_ms: now_ms,
            });
        if row.context_id.is_empty() && !context_id.is_empty() {
            row.context_id = redact::truncate_chars(&redact::escape(context_id), 128);
        }
        row.updated_ms = now_ms;
        row
    }
}

/// Label used in tables; unknown values render as `UNSPECIFIED`.
#[must_use]
pub const fn state_label(state: pb::TaskState) -> &'static str {
    match state {
        pb::TaskState::Submitted => "SUBMITTED",
        pb::TaskState::Working => "WORKING",
        pb::TaskState::Completed => "COMPLETED",
        pb::TaskState::Failed => "FAILED",
        pb::TaskState::Canceled => "CANCELED",
        pb::TaskState::InputRequired => "INPUT_REQUIRED",
        _ => "UNSPECIFIED",
    }
}

fn status_state(status: &pb::TaskStatus) -> pb::TaskState {
    pb::TaskState::try_from(status.state).unwrap_or(pb::TaskState::Unspecified)
}

fn status_text(status: &pb::TaskStatus) -> String {
    let text = status
        .message
        .as_ref()
        .map(|m| {
            m.parts
                .iter()
                .filter_map(|p| match &p.content {
                    Some(pb::part::Content::Text(t)) => Some(t.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    redact::truncate_chars(&redact::summary(&text), 200)
}

/// Reads the budget extension from `metadata`. `Budget` (limits) and `BudgetUsage`
/// (consumption) share the ext key, so either shape may appear.
fn apply_budget(budget: &mut Budget, metadata: Option<&pbjson_types::Struct>) {
    let Some(metadata) = metadata else {
        return;
    };
    let Some(value) = metadata.fields.get(BUDGET_EXT) else {
        return;
    };
    let Ok(json) = serde_json::to_value(value) else {
        return;
    };
    let read = |key: &str| json.get(key).and_then(parse_u64);
    if let Some(v) = read("maxTokens") {
        budget.max_tokens = Some(v);
    }
    if let Some(v) = read("maxWallMs") {
        budget.max_wall_ms = Some(v);
    }
    if let Some(v) = read("tokensUsed") {
        budget.tokens_used = Some(v);
    }
    if let Some(v) = read("wallMsUsed") {
        budget.wall_ms_used = Some(v);
    }
}

/// proto3 JSON carries uint64 as a string; a plain number is also accepted (RFC-0006 [0006-030]).
fn parse_u64(value: &Value) -> Option<u64> {
    match value {
        Value::String(text) => text.parse().ok(),
        Value::Number(number) => number.as_u64(),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn snapshot(id: &str, state: pb::TaskState, text: &str) -> pb::Task {
        pb::Task {
            id: id.to_owned(),
            context_id: "ctx".to_owned(),
            status: Some(pb::TaskStatus {
                state: state as i32,
                message: Some(pb::Message {
                    parts: vec![pb::Part {
                        content: Some(pb::part::Content::Text(text.to_owned())),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn req_0012_043_task_status_text_is_redacted_and_escaped() {
        let mut tasks = Tasks::default();
        let task = snapshot("t1", pb::TaskState::Working, "token=abc123\u{7}done");
        tasks.upsert_snapshot("omp", &task, 1);
        let row = tasks.get("t1").expect("row");
        assert_eq!(row.state_label(), "WORKING");
        assert!(!row.status_text.contains('\u{7}'));
        assert!(row.status_text.contains("[redacted]"));
    }

    #[test]
    fn req_0012_043_sequence_gaps_are_counted() {
        let mut tasks = Tasks::default();
        tasks.track_seq("t1", "omp", 1, 1);
        tasks.track_seq("t1", "omp", 2, 2);
        tasks.track_seq("t1", "omp", 5, 3);
        assert_eq!(tasks.get("t1").expect("row").missing_seqs, 2);
        assert_eq!(tasks.get("t1").expect("row").last_seq, 5);
    }

    #[test]
    fn req_0012_043_delegation_depth_follows_audit_parents() {
        let mut tasks = Tasks::default();
        tasks.set_parent("child", "root");
        tasks.set_parent("leaf", "child");
        assert_eq!(tasks.depth("leaf"), 2);
        assert_eq!(tasks.depth("root"), 0);
    }

    #[test]
    fn req_0012_043_delegation_depth_is_bounded_on_cycles() {
        let mut tasks = Tasks::default();
        tasks.set_parent("a", "b");
        tasks.set_parent("b", "a");
        assert!(tasks.depth("a") <= 32);
    }

    #[test]
    fn req_0006_041_budget_remaining_comes_from_limits_and_usage() {
        let budget = Budget {
            max_tokens: Some(1_000),
            tokens_used: Some(250),
            max_wall_ms: Some(10),
            wall_ms_used: Some(20),
        };
        assert_eq!(budget.tokens_remaining(), Some(750));
        assert_eq!(budget.wall_ms_remaining(), Some(0));
        assert_eq!(Budget::default().tokens_remaining(), None);
    }

    #[test]
    fn req_0006_030_budget_accepts_string_and_number_uint64() {
        assert_eq!(parse_u64(&serde_json::json!("12")), Some(12));
        assert_eq!(parse_u64(&serde_json::json!(12)), Some(12));
        assert_eq!(parse_u64(&serde_json::json!("x")), None);
    }

    #[test]
    fn req_0012_043_non_terminal_tasks_are_followed() {
        let mut tasks = Tasks::default();
        tasks.upsert_snapshot("omp", &snapshot("live", pb::TaskState::Working, "a"), 1);
        tasks.upsert_snapshot("omp", &snapshot("done", pb::TaskState::Completed, "b"), 1);
        let ids: Vec<&str> = tasks.non_terminal_ids().map(|(_, id)| id).collect();
        assert_eq!(ids, vec!["live"]);
    }
}
