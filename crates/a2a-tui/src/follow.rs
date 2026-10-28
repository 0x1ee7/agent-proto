//! Headless follow mode: turns the driver's messages into plain log lines.
//!
//! The follower keeps a [`Board`] like the console does, and reports only changes: an agent whose
//! presence changed, a task whose state or status text changed, a new artifact, an approval that
//! appeared or changed phase, and every audit record. Repeated directory syncs and task lists
//! produce nothing. Every field that came from the bus passes through `redact`.

use std::collections::HashMap;

use a2a_types::proto::a2a as pb;

use crate::app::phase_label;
use crate::driver::Msg;
use crate::model::Board;
use crate::model::agents::Presence;
use crate::model::audit::row_from;
use crate::redact;
use crate::render::tier_label;

/// Longest artifact text shown on a line, in characters.
const ARTIFACT_CHARS: usize = 200;

/// Change detector over the driver's messages.
#[derive(Debug, Default)]
pub struct Follower {
    board: Board,
    tasks: HashMap<String, (pb::TaskState, String)>,
    agents: HashMap<String, String>,
    approvals: HashMap<String, &'static str>,
    link_failure: Option<String>,
}

impl Follower {
    /// Applies one message and returns the log lines it caused, oldest first.
    pub fn lines(&mut self, now_ms: u64, msg: Msg) -> Vec<String> {
        let mut out = Vec::new();
        match msg {
            Msg::Synced(observed) => {
                if self.link_failure.take().is_some() {
                    out.push("link restored".to_owned());
                }
                self.board.heartbeat(now_ms);
                self.board.apply_agents(now_ms, observed);
                self.report_agents(&mut out);
            }
            Msg::LinkFailed(reason) => {
                let reason = redact::summary(&reason);
                if self.link_failure.as_deref() != Some(reason.as_str()) {
                    out.push(format!("link failed: {reason}"));
                    self.link_failure = Some(reason.clone());
                }
                self.board.link_failed(&reason);
            }
            Msg::Tasks { agent, tasks } => {
                self.board.apply_tasks(now_ms, &agent, &tasks);
                for task in &tasks {
                    self.report_task(&task.id, &mut out);
                }
            }
            Msg::Event { agent, event } => {
                self.board.apply_event(now_ms, &agent, &event);
                let id = event.task.as_str();
                self.report_task(id, &mut out);
                if let Some(pb::stream_response::Payload::ArtifactUpdate(update)) =
                    &event.response.payload
                {
                    out.push(artifact_line(&agent, id, update));
                }
            }
            Msg::Audit(event) => {
                self.board.apply_audit(&event);
                out.push(format!("audit {}", row_from(&event).log_line()));
            }
            Msg::Decision { .. } => {}
            Msg::Notice(text) => out.push(format!("notice {}", redact::summary(&text))),
        }
        self.board.tick(now_ms);
        self.report_approvals(&mut out);
        out
    }

    fn report_agents(&mut self, out: &mut Vec<String>) {
        for row in self.board.agents.iter() {
            let label = row.presence.map_or("absent", Presence::label);
            if self.agents.get(&row.agent).map(String::as_str) != Some(label) {
                out.push(format!("agent {} {label}", redact::escape(&row.agent)));
                self.agents.insert(row.agent.clone(), label.to_owned());
            }
        }
    }

    fn report_task(&mut self, task_id: &str, out: &mut Vec<String>) {
        let Some(row) = self.board.tasks.get(task_id) else {
            return;
        };
        let key = (row.state, row.status_text.clone());
        if self.tasks.get(task_id) == Some(&key) {
            return;
        }
        let mut line = format!(
            "task {} agent={} state={}",
            redact::escape(&row.id),
            redact::escape(&row.agent),
            row.state_label()
        );
        if !row.status_text.is_empty() {
            line.push_str(&format!(" text={}", row.status_text));
        }
        out.push(line);
        self.tasks.insert(task_id.to_owned(), key);
    }

    fn report_approvals(&mut self, out: &mut Vec<String>) {
        for entry in self.board.approvals.entries() {
            let phase = phase_label(&entry.phase);
            let id = entry.request_id();
            match self.approvals.get(id) {
                Some(seen) if *seen == phase => continue,
                Some(_) => out.push(format!("approval {} {phase}", redact::escape(id))),
                None => {
                    let request = entry.request();
                    out.push(format!(
                        "approval {} {phase} agent={} task={} tool={} tier={} summary={}",
                        redact::escape(id),
                        redact::escape(&entry.agent),
                        redact::escape(&request.task_id),
                        redact::escape(&request.tool),
                        tier_label(request.tier),
                        redact::summary(&request.summary)
                    ));
                }
            }
            self.approvals.insert(id.to_owned(), phase);
        }
    }
}

fn artifact_line(agent: &str, task: &str, update: &pb::TaskArtifactUpdateEvent) -> String {
    let (name, text) = update.artifact.as_ref().map_or_else(
        || (String::new(), String::new()),
        |artifact| {
            let text = artifact
                .parts
                .iter()
                .filter_map(|part| match &part.content {
                    Some(pb::part::Content::Text(text)) => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ");
            let name = if artifact.name.is_empty() {
                artifact.artifact_id.clone()
            } else {
                artifact.name.clone()
            };
            (name, text)
        },
    );
    let mut line = format!(
        "artifact {} agent={} name={}",
        redact::escape(task),
        redact::escape(agent),
        redact::escape(&name)
    );
    if !text.is_empty() {
        line.push_str(&format!(
            " text={}",
            redact::truncate_chars(&redact::summary(&text), ARTIFACT_CHARS)
        ));
    }
    line
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::model::agents::Observed;

    fn task(id: &str, state: pb::TaskState) -> pb::Task {
        pb::Task {
            id: id.to_owned(),
            status: Some(pb::TaskStatus {
                state: state as i32,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn tasks_msg(agent: &str, state: pb::TaskState) -> Msg {
        Msg::Tasks {
            agent: agent.to_owned(),
            tasks: vec![task("t1", state)],
        }
    }

    // req_0012: a task list that repeats every sync must not repeat log lines.
    #[test]
    fn req_0012_follow_reports_task_changes_once() {
        let mut follower = Follower::default();
        let first = follower.lines(0, tasks_msg("omp", pb::TaskState::Working));
        assert_eq!(first, vec!["task t1 agent=omp state=WORKING".to_owned()]);
        assert!(
            follower
                .lines(1_000, tasks_msg("omp", pb::TaskState::Working))
                .is_empty()
        );
        let done = follower.lines(2_000, tasks_msg("omp", pb::TaskState::Completed));
        assert_eq!(done, vec!["task t1 agent=omp state=COMPLETED".to_owned()]);
    }

    // req_0004: a presence change is a line; an unchanged roster is not.
    #[test]
    fn req_0004_follow_reports_presence_changes_once() {
        let observed = |presence| {
            vec![Observed {
                agent: "claude".to_owned(),
                card_name: None,
                card_version: None,
                presence,
                active_tasks: 0,
                approval_mode: None,
            }]
        };
        let mut follower = Follower::default();
        let up = follower.lines(0, Msg::Synced(observed(Some(Presence::Ready))));
        assert_eq!(up, vec!["agent claude READY".to_owned()]);
        assert!(
            follower
                .lines(1_000, Msg::Synced(observed(Some(Presence::Ready))))
                .is_empty()
        );
        let draining = follower.lines(2_000, Msg::Synced(observed(Some(Presence::Draining))));
        assert_eq!(draining, vec!["agent claude DRAINING".to_owned()]);
    }

    // req_0013: control characters from the bus never reach a log line.
    #[test]
    fn req_0013_follow_lines_carry_no_control_characters() {
        let mut follower = Follower::default();
        let lines = follower.lines(0, tasks_msg("omp\u{1b}[2J", pb::TaskState::Working));
        let line = lines.first().expect("line");
        assert!(!line.chars().any(char::is_control), "{line:?}");
    }
}
