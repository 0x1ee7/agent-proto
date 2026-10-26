//! Correlation identifiers (RFC-0013 sec 4.6): `traceparent`, context, task, message, delegation
//! chain and workspace id. One `traceparent` per task (0013-031) is enforced here.

use std::collections::{HashMap, VecDeque};

use a2a_types::ids::{AgentId, ContextId, MessageId, TaskId};
use a2a_types::proto::a2abus::{AuditEvent, ChainHop};

/// Maximum number of tasks whose `traceparent` is remembered per emitter.
pub const TRACE_BINDING_CAP: usize = 4096;

/// Correlation failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CorrelationError {
    /// The value is not a W3C `traceparent` (version-traceid-parentid-flags, lowercase hex).
    #[error("traceparent is not a W3C traceparent")]
    Traceparent,
    /// The workspace id is not a short token and could carry a path.
    #[error("workspace id is not a short token")]
    Workspace,
    /// The reason code is not a short machine-readable token.
    #[error("reason code is not a short token")]
    Reason,
    /// The task was already bound to another `traceparent` (0013-031).
    #[error("task {task} already carries a different traceparent")]
    TraceConflict { task: String },
}

/// Correlation fields of one event. Ids are typed, so only the free-form fields need checks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Correlation {
    /// Agent the fact is about.
    pub agent: Option<AgentId>,
    /// A2A context id.
    pub context: Option<ContextId>,
    /// A2A task id.
    pub task: Option<TaskId>,
    /// Value of `A2AB-Msg-Id` for the message that caused the fact.
    pub message: Option<MessageId>,
    /// W3C traceparent, carried unchanged across hops.
    pub traceparent: Option<String>,
    /// Root-to-leaf delegation chain of (agent, task).
    pub chain: Vec<(AgentId, TaskId)>,
    /// RFC-0012 workspace short id.
    pub workspace: Option<String>,
    /// Machine-readable reason code.
    pub reason: Option<String>,
}

impl Correlation {
    /// Checks the free-form fields.
    pub fn validate(&self) -> Result<(), CorrelationError> {
        if let Some(tp) = &self.traceparent {
            if !is_traceparent(tp) {
                return Err(CorrelationError::Traceparent);
            }
        }
        if let Some(ws) = &self.workspace {
            if !is_short_token(ws, true) {
                return Err(CorrelationError::Workspace);
            }
        }
        if let Some(reason) = &self.reason {
            if !is_short_token(reason, false) {
                return Err(CorrelationError::Reason);
            }
        }
        Ok(())
    }

    /// Copies the correlation fields into `event`.
    pub fn apply(&self, event: &mut AuditEvent) {
        event.agent_id = self
            .agent
            .as_ref()
            .map(|a| a.as_str().to_owned())
            .unwrap_or_default();
        event.context_id = self
            .context
            .as_ref()
            .map(|c| c.as_str().to_owned())
            .unwrap_or_default();
        event.task_id = self
            .task
            .as_ref()
            .map(|t| t.as_str().to_owned())
            .unwrap_or_default();
        event.message_id = self
            .message
            .as_ref()
            .map(|m| m.as_str().to_owned())
            .unwrap_or_default();
        event.traceparent = self.traceparent.clone().unwrap_or_default();
        event.chain = self
            .chain
            .iter()
            .map(|(agent, task)| ChainHop {
                agent_id: agent.as_str().to_owned(),
                task_id: task.as_str().to_owned(),
            })
            .collect();
        event.workspace_id = self.workspace.clone().unwrap_or_default();
        event.reason = self.reason.clone().unwrap_or_default();
    }
}

/// Remembers the `traceparent` of each task so that every event of that task carries the same
/// value (0013-031). Bounded by [`TRACE_BINDING_CAP`]; the oldest binding is dropped first.
#[derive(Debug, Default)]
pub struct TraceBindings {
    by_task: HashMap<String, String>,
    order: VecDeque<String>,
}

impl TraceBindings {
    /// The `traceparent` an event of `task` must carry. An event without one inherits the bound
    /// value. A different explicit value is a conflict.
    pub fn resolve(
        &self,
        task: Option<&str>,
        traceparent: Option<&str>,
    ) -> Result<Option<String>, CorrelationError> {
        let bound = task.and_then(|t| self.by_task.get(t));
        match (bound, traceparent) {
            (Some(bound), Some(tp)) if bound != tp => Err(CorrelationError::TraceConflict {
                task: task.unwrap_or_default().to_owned(),
            }),
            (Some(bound), _) => Ok(Some(bound.clone())),
            (None, tp) => Ok(tp.map(str::to_owned)),
        }
    }

    /// Records the binding after the event was durably spooled.
    pub fn bind(&mut self, task: &str, traceparent: &str) {
        if self.by_task.contains_key(task) {
            return;
        }
        self.by_task.insert(task.to_owned(), traceparent.to_owned());
        self.order.push_back(task.to_owned());
        while self.order.len() > TRACE_BINDING_CAP {
            if let Some(old) = self.order.pop_front() {
                self.by_task.remove(&old);
            }
        }
    }
}

/// W3C traceparent: `00-<32 lowercase hex, not all zero>-<16 lowercase hex, not all zero>-<2 hex>`.
/// Version `ff` is invalid.
pub fn is_traceparent(value: &str) -> bool {
    let parts: Vec<&str> = value.split('-').collect();
    let [version, trace_id, parent_id, flags] = parts.as_slice() else {
        return false;
    };
    lower_hex(version, 2)
        && *version != "ff"
        && lower_hex(trace_id, 32)
        && !trace_id.bytes().all(|b| b == b'0')
        && lower_hex(parent_id, 16)
        && !parent_id.bytes().all(|b| b == b'0')
        && lower_hex(flags, 2)
}

fn lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `[A-Za-z0-9_-]{1,64}` for workspace ids (`allow_upper`), `[a-z0-9_]{1,64}` for reason codes.
fn is_short_token(value: &str, allow_upper: bool) -> bool {
    (1..=64).contains(&value.len())
        && value.bytes().all(|b| {
            b.is_ascii_lowercase()
                || b.is_ascii_digit()
                || b == b'_'
                || (allow_upper && (b.is_ascii_uppercase() || b == b'-'))
        })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const TP: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    /// 0013-030: free-form correlation fields are checked; the W3C form is strict.
    #[test]
    fn req_0013_030_traceparent_grammar() {
        assert!(is_traceparent(TP));
        assert!(!is_traceparent(
            "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01"
        ));
        assert!(!is_traceparent(
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01"
        ));
        assert!(!is_traceparent(
            "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
        ));
        assert!(!is_traceparent(
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7"
        ));
    }

    /// 0013-030: workspace ids and reason codes are short tokens, never paths.
    #[test]
    fn req_0013_030_workspace_and_reason_are_tokens() {
        let ok = Correlation {
            workspace: Some("ws-A_1".into()),
            reason: Some("shared_checkout".into()),
            ..Default::default()
        };
        assert!(ok.validate().is_ok());

        let path = Correlation {
            workspace: Some("/home/u/ws".into()),
            ..Default::default()
        };
        assert_eq!(path.validate(), Err(CorrelationError::Workspace));

        let upper_reason = Correlation {
            reason: Some("Shared".into()),
            ..Default::default()
        };
        assert_eq!(upper_reason.validate(), Err(CorrelationError::Reason));
    }

    /// 0013-031: every event of one task carries one traceparent; an event without one inherits
    /// it; a different explicit value is refused.
    #[test]
    fn req_0013_031_one_traceparent_per_task() {
        let mut bindings = TraceBindings::default();
        assert_eq!(
            bindings.resolve(Some("t1"), Some(TP)),
            Ok(Some(TP.to_owned()))
        );
        bindings.bind("t1", TP);
        assert_eq!(bindings.resolve(Some("t1"), None), Ok(Some(TP.to_owned())));
        assert_eq!(
            bindings.resolve(Some("t1"), Some(TP)),
            Ok(Some(TP.to_owned()))
        );
        assert_eq!(
            bindings.resolve(
                Some("t1"),
                Some("00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbbbbbbbbbb-01")
            ),
            Err(CorrelationError::TraceConflict { task: "t1".into() })
        );
        assert_eq!(bindings.resolve(Some("t2"), None), Ok(None));
    }

    /// 0013-031: the binding table is bounded; the oldest task is forgotten first.
    #[test]
    fn req_0013_031_binding_table_is_bounded() {
        let mut bindings = TraceBindings::default();
        for i in 0..=TRACE_BINDING_CAP {
            bindings.bind(&format!("t{i}"), TP);
        }
        assert_eq!(bindings.resolve(Some("t0"), None), Ok(None));
        assert_eq!(
            bindings.resolve(Some(&format!("t{TRACE_BINDING_CAP}")), None),
            Ok(Some(TP.to_owned()))
        );
    }

    /// 0013-030: correlation fields reach the proto message, the chain in root-to-leaf order.
    #[test]
    fn req_0013_030_apply_copies_fields_in_order() {
        let corr = Correlation {
            agent: Some(AgentId::parse("omp").unwrap()),
            task: Some(TaskId::parse("t-1").unwrap()),
            chain: vec![
                (
                    AgentId::parse("hermes").unwrap(),
                    TaskId::parse("root").unwrap(),
                ),
                (
                    AgentId::parse("omp").unwrap(),
                    TaskId::parse("t-1").unwrap(),
                ),
            ],
            ..Default::default()
        };
        let mut event = AuditEvent::default();
        corr.apply(&mut event);
        assert_eq!(event.agent_id, "omp");
        assert_eq!(event.task_id, "t-1");
        assert_eq!(event.chain.len(), 2);
        assert_eq!(event.chain[0].agent_id, "hermes");
        assert_eq!(event.chain[1].task_id, "t-1");
        assert!(event.context_id.is_empty());
    }
}
