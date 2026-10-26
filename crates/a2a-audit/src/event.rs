//! Event construction: the typed builder that callers fill, and the details size limit
//! (RFC-0013 0013-024).

use serde_json::{Map, Value};

use a2a_types::ids::{AgentId, ContextId, MessageId, TaskId};
use a2a_types::proto::a2abus::AuditEventType;

use crate::canon::{CanonError, canonical_bytes};
use crate::correlation::Correlation;

/// Upper bound of the canonical `details` object after redaction (0013-024).
pub const DETAILS_MAX_BYTES: usize = 4096;

/// Collects the caller's facts for one event. The emitter validates, redacts, and seals it.
#[derive(Debug, Clone)]
pub struct EventBuilder {
    ty: AuditEventType,
    correlation: Correlation,
    details: Map<String, Value>,
}

impl EventBuilder {
    /// An event of type `ty` with no correlation and no details.
    pub fn new(ty: AuditEventType) -> Self {
        Self {
            ty,
            correlation: Correlation::default(),
            details: Map::new(),
        }
    }

    /// Sets the agent the fact is about.
    pub fn agent(mut self, agent: AgentId) -> Self {
        self.correlation.agent = Some(agent);
        self
    }

    /// Sets the context id.
    pub fn context(mut self, context: ContextId) -> Self {
        self.correlation.context = Some(context);
        self
    }

    /// Sets the task id.
    pub fn task(mut self, task: TaskId) -> Self {
        self.correlation.task = Some(task);
        self
    }

    /// Sets the message id that caused the fact.
    pub fn message(mut self, message: MessageId) -> Self {
        self.correlation.message = Some(message);
        self
    }

    /// Sets the W3C traceparent. Validated when the event is sealed.
    pub fn traceparent(mut self, traceparent: impl Into<String>) -> Self {
        self.correlation.traceparent = Some(traceparent.into());
        self
    }

    /// Sets the root-to-leaf delegation chain.
    pub fn chain(mut self, hops: Vec<(AgentId, TaskId)>) -> Self {
        self.correlation.chain = hops;
        self
    }

    /// Sets the RFC-0012 workspace short id.
    pub fn workspace(mut self, workspace: impl Into<String>) -> Self {
        self.correlation.workspace = Some(workspace.into());
        self
    }

    /// Sets the machine-readable reason code.
    pub fn reason(mut self, reason: impl Into<String>) -> Self {
        self.correlation.reason = Some(reason.into());
        self
    }

    /// Adds a raw detail value. Schema and redaction checks run when the event is sealed.
    pub fn detail(mut self, key: &str, value: Value) -> Self {
        self.details.insert(key.to_owned(), value);
        self
    }

    /// Adds a string detail.
    pub fn detail_str(self, key: &str, value: &str) -> Self {
        self.detail(key, Value::String(value.to_owned()))
    }

    /// Adds an integer detail as a decimal string (0013-008).
    pub fn detail_count(self, key: &str, count: u64) -> Self {
        self.detail(key, Value::String(count.to_string()))
    }

    /// Adds a boolean detail.
    pub fn detail_bool(self, key: &str, value: bool) -> Self {
        self.detail(key, Value::Bool(value))
    }

    /// The event type.
    pub fn event_type(&self) -> AuditEventType {
        self.ty
    }

    /// Splits the builder into its parts.
    pub fn into_parts(self) -> (AuditEventType, Correlation, Map<String, Value>) {
        (self.ty, self.correlation, self.details)
    }
}

/// Shrinks `details` until its canonical form fits [`DETAILS_MAX_BYTES`]. The longest top-level
/// string is halved first. When no string is left, the largest key is dropped. Returns `true`
/// when anything was cut, so the caller sets `truncated`.
pub fn fit_details(details: &mut Map<String, Value>) -> Result<bool, CanonError> {
    let mut truncated = false;
    loop {
        let size = canonical_bytes(&Value::Object(details.clone()))?.len();
        if size <= DETAILS_MAX_BYTES || details.is_empty() {
            return Ok(truncated);
        }
        truncated = true;
        let longest = details
            .iter()
            .filter_map(|(key, value)| match value {
                Value::String(text) if !text.is_empty() => {
                    Some((key.clone(), text.chars().count()))
                }
                _ => None,
            })
            .max_by_key(|(_, chars)| *chars);
        if let Some((key, chars)) = longest {
            if let Some(Value::String(text)) = details.get_mut(&key) {
                *text = text.chars().take(chars / 2).collect();
            }
            continue;
        }
        let victim = details
            .iter()
            .map(|(key, value)| {
                let size = canonical_bytes(value).map_or(0, |bytes| bytes.len());
                (key.clone(), size)
            })
            .max_by_key(|(_, size)| *size)
            .map(|(key, _)| key);
        if let Some(key) = victim {
            details.remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn map(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn req_0013_024_small_details_are_left_alone() {
        let mut details = map(json!({"count": "3", "ok": true}));
        let before = details.clone();
        let truncated = fit_details(&mut details).unwrap_or(true);
        assert!(!truncated);
        assert_eq!(details, before);
    }

    #[test]
    fn req_0013_024_oversized_string_is_cut_and_flagged() {
        let mut details = map(json!({"args_summary": "a".repeat(9000), "tag": "keep"}));
        let truncated = fit_details(&mut details).unwrap_or(false);
        assert!(truncated);
        let size = canonical_bytes(&Value::Object(details.clone())).map_or(usize::MAX, |b| b.len());
        assert!(size <= DETAILS_MAX_BYTES, "size {size}");
        assert_eq!(details["tag"], json!("keep"));
        assert!(
            details["args_summary"]
                .as_str()
                .is_some_and(|s| s.len() < 9000)
        );
    }

    #[test]
    fn req_0013_024_keys_are_dropped_when_strings_cannot_shrink() {
        let mut details = map(json!({
            "a": ["x".repeat(3000)],
            "b": ["y".repeat(3000)],
        }));
        let truncated = fit_details(&mut details).unwrap_or(false);
        assert!(truncated);
        assert_eq!(details.len(), 1);
        let size = canonical_bytes(&Value::Object(details.clone())).map_or(usize::MAX, |b| b.len());
        assert!(size <= DETAILS_MAX_BYTES);
    }

    #[test]
    fn req_0013_008_integers_are_decimal_strings_in_the_builder() {
        let (_, _, details) = EventBuilder::new(AuditEventType::AuditGap)
            .detail_count("first_seq", 42)
            .into_parts();
        assert_eq!(details["first_seq"], json!("42"));
    }

    #[test]
    fn req_0013_030_builder_fills_correlation_fields() {
        let agent = AgentId::parse("omp").unwrap_or_else(|_| AgentId::gateway());
        let (ty, correlation, _) = EventBuilder::new(AuditEventType::AgentStarted)
            .agent(agent.clone())
            .workspace("ws1")
            .reason("ok")
            .into_parts();
        assert_eq!(ty, AuditEventType::AgentStarted);
        assert_eq!(correlation.agent, Some(agent));
        assert_eq!(correlation.workspace.as_deref(), Some("ws1"));
        assert_eq!(correlation.reason.as_deref(), Some("ok"));
        assert!(correlation.task.is_none());
    }
}
