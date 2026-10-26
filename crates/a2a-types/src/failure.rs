//! Failure reason registry and carrier (RFC-0005 sec 4.9).
//!
//! A FAILED or REJECTED status carries exactly one [`FailureCode`] under the status message
//! metadata key [`crate::vocab::FAILURE_KEY`] as `{"code": "<CODE>", "detail": {...}}`
//! ([0005-049]). The status message also carries a readable text part so that stock peers see
//! the reason ([0005-049]).

use std::fmt;
use std::str::FromStr;

use pbjson_types::{Struct, Value, value::Kind};
use thiserror::Error;

use crate::proto::a2a::{Message, Part, TaskState, part};
use crate::vocab::FAILURE_KEY;

/// Failure codes of the M1 registry (RFC-0005 [0005-048], table sec 4.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FailureCode {
    /// Task deadline reached.
    DeadlineExceeded,
    /// Approval expiry reached with no decision.
    ApprovalTimeout,
    /// Operator denied and the backend ended the turn without success.
    ApprovalDenied,
    /// A budget dimension is spent; the dimension is in detail.
    BudgetExhausted,
    /// Cycle or repeated task fingerprint in the chain.
    LoopDetected,
    /// A delegated child ended FAILED; its code is in detail.
    ChildFailed,
    /// Backend exited, stopped responding, or was found mid-turn after restart.
    BackendCrashed,
    /// Backend could not be started or initialized.
    BackendUnavailable,
    /// JSON-RPC error or unknown stop reason from the backend, or peer auth failure.
    BackendError,
    /// Backend turn idle timeout or peer reply-timeout marker.
    BackendTimeout,
    /// Peer reports the task unknown on poll or resubscribe (`-32001`).
    BackendLostTask,
    /// Backend stop reason refusal.
    AgentRefused,
    /// Backend stop reason max_tokens or max_turn_requests.
    AgentLimit,
    /// Per-peer rate limit exceeded (final state REJECTED).
    RateLimited,
    /// Queue quota exceeded (final state REJECTED).
    QueueFull,
}

impl FailureCode {
    /// Every registry code, in table order.
    pub const ALL: [FailureCode; 15] = [
        FailureCode::DeadlineExceeded,
        FailureCode::ApprovalTimeout,
        FailureCode::ApprovalDenied,
        FailureCode::BudgetExhausted,
        FailureCode::LoopDetected,
        FailureCode::ChildFailed,
        FailureCode::BackendCrashed,
        FailureCode::BackendUnavailable,
        FailureCode::BackendError,
        FailureCode::BackendTimeout,
        FailureCode::BackendLostTask,
        FailureCode::AgentRefused,
        FailureCode::AgentLimit,
        FailureCode::RateLimited,
        FailureCode::QueueFull,
    ];

    /// Wire token, exactly as the RFC table spells it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            FailureCode::DeadlineExceeded => "DEADLINE_EXCEEDED",
            FailureCode::ApprovalTimeout => "APPROVAL_TIMEOUT",
            FailureCode::ApprovalDenied => "APPROVAL_DENIED",
            FailureCode::BudgetExhausted => "BUDGET_EXHAUSTED",
            FailureCode::LoopDetected => "LOOP_DETECTED",
            FailureCode::ChildFailed => "CHILD_FAILED",
            FailureCode::BackendCrashed => "BACKEND_CRASHED",
            FailureCode::BackendUnavailable => "BACKEND_UNAVAILABLE",
            FailureCode::BackendError => "BACKEND_ERROR",
            FailureCode::BackendTimeout => "BACKEND_TIMEOUT",
            FailureCode::BackendLostTask => "BACKEND_LOST_TASK",
            FailureCode::AgentRefused => "AGENT_REFUSED",
            FailureCode::AgentLimit => "AGENT_LIMIT",
            FailureCode::RateLimited => "RATE_LIMITED",
            FailureCode::QueueFull => "QUEUE_FULL",
        }
    }

    /// The final state a code belongs to: REJECTED for admission refusals, FAILED otherwise.
    #[must_use]
    pub const fn final_state(self) -> TaskState {
        match self {
            FailureCode::RateLimited | FailureCode::QueueFull => TaskState::Rejected,
            _ => TaskState::Failed,
        }
    }

    /// Parses an exact wire token. Lowercase or unknown tokens are rejected.
    pub fn parse(token: &str) -> Result<Self, FailureError> {
        FailureCode::ALL
            .iter()
            .copied()
            .find(|code| code.as_str() == token)
            .ok_or_else(|| FailureError::UnknownCode(token.to_owned()))
    }
}

impl fmt::Display for FailureCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for FailureCode {
    type Err = FailureError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        FailureCode::parse(s)
    }
}

/// Errors from reading or writing the `a2abus.failure` metadata.
#[derive(Debug, Error)]
pub enum FailureError {
    /// The code token is not in the registry.
    #[error("unknown failure code {0:?}")]
    UnknownCode(String),
    /// The metadata value does not match `{"code": "<CODE>", "detail": {...}}`.
    #[error("malformed failure data: {0}")]
    Malformed(String),
    /// The value could not be converted between the JSON and `Struct` forms.
    #[error("failure data could not be converted: {0}")]
    Convert(#[source] serde_json::Error),
}

/// A failure reason: one registry code and optional redacted detail (RFC-0005 [0005-049]).
#[derive(Debug, Clone, PartialEq)]
pub struct FailureReason {
    /// The registry code.
    pub code: FailureCode,
    /// Optional structured detail. The producer MUST redact it (RFC-0005 [0005-049]).
    pub detail: Option<Struct>,
}

impl FailureReason {
    /// Builds a reason with no detail.
    #[must_use]
    pub fn new(code: FailureCode) -> Self {
        Self { code, detail: None }
    }

    /// Readable text for the status message part, for example `task failed: deadline exceeded`
    /// ([0005-049]).
    #[must_use]
    pub fn text(&self) -> String {
        let word = match self.code.final_state() {
            TaskState::Rejected => "rejected",
            _ => "failed",
        };
        let phrase = self.code.as_str().to_ascii_lowercase().replace('_', " ");
        format!("task {word}: {phrase}")
    }

    /// Encodes the value stored under [`FAILURE_KEY`]: `{"code": "<CODE>", "detail": {...}}`.
    pub fn to_struct(&self) -> Result<Struct, FailureError> {
        let mut object = serde_json::Map::new();
        object.insert(
            "code".to_owned(),
            serde_json::Value::String(self.code.as_str().to_owned()),
        );
        if let Some(detail) = &self.detail {
            let detail = serde_json::to_value(detail).map_err(FailureError::Convert)?;
            object.insert("detail".to_owned(), detail);
        }
        serde_json::from_value(serde_json::Value::Object(object)).map_err(FailureError::Convert)
    }

    /// Decodes a value stored under [`FAILURE_KEY`]. Unknown keys and non-object detail are
    /// rejected, so the producer's schema is checked exactly.
    pub fn from_struct(value: &Struct) -> Result<Self, FailureError> {
        let json = serde_json::to_value(value).map_err(FailureError::Convert)?;
        let serde_json::Value::Object(mut object) = json else {
            return Err(FailureError::Malformed("expected an object".to_owned()));
        };
        let code = match object.remove("code") {
            Some(serde_json::Value::String(token)) => FailureCode::parse(&token)?,
            _ => return Err(FailureError::Malformed("code must be a string".to_owned())),
        };
        let detail = match object.remove("detail") {
            None => None,
            Some(detail) => Some(
                serde_json::from_value::<Struct>(detail)
                    .map_err(|e| FailureError::Malformed(format!("detail: {e}")))?,
            ),
        };
        if let Some(key) = object.keys().next() {
            return Err(FailureError::Malformed(format!("unknown key {key:?}")));
        }
        Ok(Self { code, detail })
    }

    /// Writes this reason under [`FAILURE_KEY`] in `metadata`, replacing any earlier value.
    pub fn write_to(&self, metadata: &mut Struct) -> Result<(), FailureError> {
        let value = Value {
            kind: Some(Kind::StructValue(self.to_struct()?)),
        };
        metadata.fields.insert(FAILURE_KEY.to_owned(), value);
        Ok(())
    }

    /// Reads the reason under [`FAILURE_KEY`]. `Ok(None)` means the key is absent.
    pub fn read_from(metadata: &Struct) -> Result<Option<Self>, FailureError> {
        match metadata.fields.get(FAILURE_KEY) {
            None => Ok(None),
            Some(Value {
                kind: Some(Kind::StructValue(value)),
            }) => Self::from_struct(value).map(Some),
            Some(_) => Err(FailureError::Malformed(
                "failure value must be an object".to_owned(),
            )),
        }
    }

    /// Applies this reason to a status message: sets the metadata key and appends the readable
    /// text part. Call it on a freshly built status message, since it appends the text part.
    pub fn apply_to_message(&self, message: &mut Message) -> Result<(), FailureError> {
        self.write_to(message.metadata.get_or_insert_with(Struct::default))?;
        message.parts.push(Part {
            content: Some(part::Content::Text(self.text())),
            ..Default::default()
        });
        Ok(())
    }

    /// Reads the reason from a status message. `Ok(None)` means the message carries no reason.
    pub fn from_message(message: &Message) -> Result<Option<Self>, FailureError> {
        match &message.metadata {
            None => Ok(None),
            Some(metadata) => Self::read_from(metadata),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn struct_of(value: serde_json::Value) -> Struct {
        serde_json::from_value(value).unwrap()
    }

    // [0005-048] the registry has exactly the 15 codes of the table.
    #[test]
    fn req_0005_048_registry_has_fifteen_unique_codes() {
        assert_eq!(FailureCode::ALL.len(), 15);
        let mut tokens: Vec<&str> = FailureCode::ALL.iter().map(|c| c.as_str()).collect();
        tokens.sort_unstable();
        tokens.dedup();
        assert_eq!(tokens.len(), 15);
    }

    // [0005-048] each code has the final state the table assigns to it.
    #[test]
    fn req_0005_048_codes_map_to_their_final_state() {
        let rejected = [FailureCode::RateLimited, FailureCode::QueueFull];
        for code in FailureCode::ALL {
            let expected = if rejected.contains(&code) {
                TaskState::Rejected
            } else {
                TaskState::Failed
            };
            assert_eq!(code.final_state(), expected, "{code}");
        }
    }

    // [0005-048] only exact registry tokens are accepted.
    #[test]
    fn req_0005_048_unknown_or_non_canonical_codes_are_rejected() {
        for bad in [
            "",
            "deadline_exceeded",
            "EXPIRED",
            "DEADLINE EXCEEDED",
            "BACKEND",
        ] {
            assert!(
                matches!(FailureCode::parse(bad), Err(FailureError::UnknownCode(_))),
                "{bad:?} must be rejected"
            );
        }
        assert_eq!(
            "BACKEND_LOST_TASK".parse::<FailureCode>().unwrap(),
            FailureCode::BackendLostTask
        );
    }

    // [0005-048] every code token round-trips through its wire form.
    #[test]
    fn req_0005_048_every_code_round_trips_through_its_token() {
        for code in FailureCode::ALL {
            assert_eq!(FailureCode::parse(code.as_str()).unwrap(), code);
            assert_eq!(code.to_string(), code.as_str());
        }
    }

    // [0005-049] the stored value is exactly {"code": "<CODE>"} when no detail is given.
    #[test]
    fn req_0005_049_code_only_value_has_the_rfc_shape() {
        let reason = FailureReason::new(FailureCode::RateLimited);
        let value = serde_json::to_value(reason.to_struct().unwrap()).unwrap();
        assert_eq!(value, json!({"code": "RATE_LIMITED"}));
    }

    // [0005-049] detail is carried and decodes back to the same reason.
    #[test]
    fn req_0005_049_detail_round_trips() {
        let reason = FailureReason {
            code: FailureCode::BudgetExhausted,
            detail: Some(struct_of(json!({"dimension": "tokens"}))),
        };
        let mut metadata = Struct::default();
        reason.write_to(&mut metadata).unwrap();
        let read = FailureReason::read_from(&metadata).unwrap().unwrap();
        assert_eq!(read, reason);
    }

    // [0005-049] the key is a plain metadata key a2abus.failure, not an extension URI.
    #[test]
    fn req_0005_049_failure_is_stored_under_the_plain_key() {
        let mut metadata = Struct::default();
        FailureReason::new(FailureCode::DeadlineExceeded)
            .write_to(&mut metadata)
            .unwrap();
        assert_eq!(metadata.fields.len(), 1);
        assert!(metadata.fields.contains_key("a2abus.failure"));
    }

    // [0005-049] an absent key reads as no reason, not as an error.
    #[test]
    fn req_0005_049_absent_key_reads_as_none() {
        let metadata = struct_of(json!({"urn:a2abus:ext:sequence:1": {"seq": "3"}}));
        assert_eq!(FailureReason::read_from(&metadata).unwrap(), None);
    }

    // [0005-049] malformed stored values are rejected: unknown code, missing code, extra keys,
    // non-object detail, and a non-object value.
    #[test]
    fn req_0005_049_malformed_values_are_rejected() {
        let bad = [
            json!({"code": "NOPE"}),
            json!({"detail": {"x": 1}}),
            json!({"code": 4}),
            json!({"code": "QUEUE_FULL", "extra": true}),
            json!({"code": "AGENT_LIMIT", "detail": "text"}),
            json!({"code": "AGENT_LIMIT", "detail": null}),
        ];
        for value in bad {
            let metadata = struct_of(json!({"a2abus.failure": value.clone()}));
            assert!(
                FailureReason::read_from(&metadata).is_err(),
                "{value} must be rejected"
            );
        }
        let not_object = struct_of(json!({"a2abus.failure": "DEADLINE_EXCEEDED"}));
        assert!(matches!(
            FailureReason::read_from(&not_object),
            Err(FailureError::Malformed(_))
        ));
    }

    // [0005-049] the readable text names the state and the reason in words.
    #[test]
    fn req_0005_049_text_is_readable_and_matches_the_state() {
        assert_eq!(
            FailureReason::new(FailureCode::DeadlineExceeded).text(),
            "task failed: deadline exceeded"
        );
        assert_eq!(
            FailureReason::new(FailureCode::RateLimited).text(),
            "task rejected: rate limited"
        );
    }

    // [0005-049] applying the reason to a status message sets the key and the text part.
    #[test]
    fn req_0005_049_apply_to_message_sets_metadata_and_text() {
        let mut message = Message::default();
        FailureReason::new(FailureCode::DeadlineExceeded)
            .apply_to_message(&mut message)
            .unwrap();
        assert_eq!(message.parts.len(), 1);
        assert_eq!(
            message.parts[0].content,
            Some(part::Content::Text(
                "task failed: deadline exceeded".to_owned()
            ))
        );
        let read = FailureReason::from_message(&message).unwrap().unwrap();
        assert_eq!(read.code, FailureCode::DeadlineExceeded);
    }

    // [0005-049] a message without metadata carries no reason.
    #[test]
    fn req_0005_049_message_without_metadata_has_no_reason() {
        assert_eq!(
            FailureReason::from_message(&Message::default()).unwrap(),
            None
        );
    }
}
