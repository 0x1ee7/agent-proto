//! JSON-RPC 2.0 envelopes, A2A wire parsing and error-class mapping (RFC-0009 4.6, 4.11; RFC-0008 0008-045, 0008-078, 0008-098).
//!
//! Mapping is by HTTP status and JSON-RPC code only; `error.data` / ErrorInfo is never
//! consulted (0008-045 deviation: Hermes does not send it, so it is optional).

use serde_json::{Value, json};
use thiserror::Error;

/// A2A method names used by the driver.
pub const SEND_MESSAGE: &str = "SendMessage";
/// Streaming send method (0009-043).
pub const SEND_STREAMING: &str = "SendStreamingMessage";
/// Task lookup.
pub const GET_TASK: &str = "GetTask";
/// Task cancel.
pub const CANCEL_TASK: &str = "CancelTask";
/// Task resubscribe.
pub const SUBSCRIBE_TASK: &str = "SubscribeToTask";

/// JSON-RPC error object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcError {
    /// Numeric code.
    pub code: i64,
    /// Peer message. Redacted before it leaves the driver.
    pub message: String,
}

/// A response body that does not match the expected shape.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("malformed peer response: {0}")]
pub struct Malformed(pub String);

/// Parsed JSON-RPC response envelope.
#[derive(Debug, Clone, PartialEq)]
pub enum Envelope {
    /// Successful `result`.
    Result(Value),
    /// JSON-RPC `error`.
    Error(RpcError),
}

/// Builds a JSON-RPC 2.0 request body.
pub fn request(id: &str, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

/// Parses a JSON-RPC response body. Accepts a missing `error.data` (0008-045).
///
/// # Errors
/// Returns [`Malformed`] for non-JSON, non-object, or envelopes with neither `result` nor `error`.
pub fn parse_envelope(body: &[u8]) -> Result<Envelope, Malformed> {
    let value: Value =
        serde_json::from_slice(body).map_err(|e| Malformed(format!("not JSON: {e}")))?;
    let Value::Object(mut map) = value else {
        return Err(Malformed("body is not an object".to_owned()));
    };
    if let Some(error) = map.remove("error") {
        let code = error
            .get("code")
            .and_then(Value::as_i64)
            .ok_or_else(|| Malformed("error without numeric code".to_owned()))?;
        let message = str_field(&error, "message");
        return Ok(Envelope::Error(RpcError { code, message }));
    }
    map.remove("result")
        .map(Envelope::Result)
        .ok_or_else(|| Malformed("neither result nor error".to_owned()))
}

/// Failure class used by the error mapping (RFC-0008 0008-078).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailClass {
    /// Credentials refused (HTTP 401/403, -32050, -32052).
    Auth,
    /// Rate limited (HTTP 429, -32051).
    RateLimited,
    /// The peer no longer knows the task (-32001 on a lookup).
    LostTask,
    /// Any other error.
    Other,
}

/// Classifies an HTTP status. Returns `None` for statuses that carry no class.
pub fn classify_status(status: u16) -> Option<FailClass> {
    match status {
        401 | 403 => Some(FailClass::Auth),
        429 => Some(FailClass::RateLimited),
        _ => None,
    }
}

/// Classifies a JSON-RPC code. `lookup` is true for GetTask and SubscribeToTask.
pub fn classify_code(code: i64, lookup: bool) -> FailClass {
    match code {
        -32050 | -32052 => FailClass::Auth,
        -32051 => FailClass::RateLimited,
        -32001 if lookup => FailClass::LostTask,
        _ => FailClass::Other,
    }
}

/// Peer task state, normalized from `TASK_STATE_*` (A2A 1.0) and bare or lowercase
/// names (pre-1.0 tolerance, 0009-040).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerState {
    /// Accepted, not started.
    Submitted,
    /// Running.
    Working,
    /// Finished with output.
    Completed,
    /// Canceled.
    Canceled,
    /// Refused by the peer.
    Rejected,
    /// Waiting for user input (0008-087).
    InputRequired,
    /// Needs authentication from the caller.
    AuthRequired,
    /// Failed.
    Failed,
    /// Any other value; the raw text is kept for the detail.
    Unknown(String),
}

impl PeerState {
    /// Parses a raw state string.
    pub fn parse(raw: &str) -> Self {
        let upper = raw.trim().to_ascii_uppercase().replace('-', "_");
        let name = upper.strip_prefix("TASK_STATE_").unwrap_or(&upper);
        match name {
            "SUBMITTED" => Self::Submitted,
            "WORKING" => Self::Working,
            "COMPLETED" => Self::Completed,
            "CANCELED" | "CANCELLED" => Self::Canceled,
            "REJECTED" => Self::Rejected,
            "INPUT_REQUIRED" => Self::InputRequired,
            "AUTH_REQUIRED" => Self::AuthRequired,
            "FAILED" => Self::Failed,
            _ => Self::Unknown(raw.to_owned()),
        }
    }
}

/// Task snapshot from a result or stream frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskView {
    /// Peer task id.
    pub id: String,
    /// Peer contextId (empty when absent).
    pub context_id: String,
    /// Normalized state.
    pub state: PeerState,
    /// Status message text (empty when absent).
    pub status_text: String,
    /// Concatenated text of all artifact parts (empty when absent).
    pub artifact_text: String,
}

/// One decoded result or stream update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Update {
    /// Full task snapshot.
    Task(TaskView),
    /// Status change for a task.
    Status {
        /// Peer task id.
        task_id: String,
        /// Peer contextId (empty when absent).
        context_id: String,
        /// Normalized state.
        state: PeerState,
        /// Status message text (empty when absent).
        text: String,
    },
    /// Artifact chunk.
    Artifact {
        /// Peer task id.
        task_id: String,
        /// Peer contextId (empty when absent).
        context_id: String,
        /// Artifact id, or empty.
        artifact_id: String,
        /// Chunk text.
        text: String,
        /// Whether this chunk appends to the previous chunk of the artifact.
        append: bool,
        /// Whether this is the last chunk of the artifact.
        last: bool,
    },
    /// Direct message reply without a task (0008-086).
    Message {
        /// Peer contextId (empty when absent).
        context_id: String,
        /// Message text.
        text: String,
    },
}

/// Decodes a `result` object into an [`Update`].
///
/// # Errors
/// Returns [`Malformed`] when the result has none of `task`, `statusUpdate`,
/// `artifactUpdate`, `message`, or a bare task shape.
pub fn parse_result(result: &Value) -> Result<Update, Malformed> {
    if let Some(task) = result.get("task") {
        return parse_task(task).map(Update::Task);
    }
    if let Some(update) = result.get("statusUpdate") {
        let (state_raw, text) = status_of(update.get("status"));
        return Ok(Update::Status {
            task_id: str_field(update, "taskId"),
            context_id: str_field(update, "contextId"),
            state: PeerState::parse(&state_raw),
            text,
        });
    }
    if let Some(update) = result.get("artifactUpdate") {
        let artifact = update.get("artifact");
        return Ok(Update::Artifact {
            task_id: str_field(update, "taskId"),
            context_id: str_field(update, "contextId"),
            artifact_id: artifact
                .map(|a| str_field(a, "artifactId"))
                .unwrap_or_default(),
            text: parts_text(artifact.and_then(|a| a.get("parts"))),
            append: update
                .get("append")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            last: update
                .get("lastChunk")
                .and_then(Value::as_bool)
                .unwrap_or(true),
        });
    }
    if let Some(message) = result.get("message") {
        return Ok(Update::Message {
            context_id: str_field(message, "contextId"),
            text: parts_text(message.get("parts")),
        });
    }
    if result.get("id").is_some() && result.get("status").is_some() {
        return parse_task(result).map(Update::Task);
    }
    Err(Malformed(
        "result has no task, update or message".to_owned(),
    ))
}

fn parse_task(value: &Value) -> Result<TaskView, Malformed> {
    let id = str_field(value, "id");
    if id.is_empty() {
        return Err(Malformed("task without id".to_owned()));
    }
    let (state_raw, status_text) = status_of(value.get("status"));
    Ok(TaskView {
        id,
        context_id: str_field(value, "contextId"),
        state: PeerState::parse(&state_raw),
        status_text,
        artifact_text: artifacts_text(value.get("artifacts")),
    })
}

/// Returns `(raw state, status text)`. A bare string status is accepted (pre-1.0 tolerance).
fn status_of(status: Option<&Value>) -> (String, String) {
    match status {
        Some(Value::String(state)) => (state.clone(), String::new()),
        Some(object) => (
            str_field(object, "state"),
            parts_text(object.get("message").and_then(|m| m.get("parts"))),
        ),
        None => (String::new(), String::new()),
    }
}

fn artifacts_text(artifacts: Option<&Value>) -> String {
    let Some(Value::Array(items)) = artifacts else {
        return String::new();
    };
    items.iter().map(|a| parts_text(a.get("parts"))).collect()
}

/// Concatenates the `text` of every part in a `parts` array.
fn parts_text(parts: Option<&Value>) -> String {
    let Some(Value::Array(items)) = parts else {
        return String::new();
    };
    items
        .iter()
        .filter_map(|p| p.get("text").and_then(Value::as_str))
        .collect()
}

fn str_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Builds the `params` object for GetTask, CancelTask, and SubscribeToTask. The tenant is added by the transport.
pub fn task_params(task_id: &str) -> Value {
    json!({"id": task_id})
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// 0009-047: TASK_STATE_ names and bare lowercase names map to the same state.
    #[test]
    fn req_0009_047_state_names_are_normalized() {
        assert_eq!(
            PeerState::parse("TASK_STATE_COMPLETED"),
            PeerState::Completed
        );
        assert_eq!(PeerState::parse("completed"), PeerState::Completed);
        assert_eq!(PeerState::parse("input-required"), PeerState::InputRequired);
        assert_eq!(
            PeerState::parse("WAT"),
            PeerState::Unknown("WAT".to_owned())
        );
    }

    /// 0008-098: a missing error.data is accepted and the code is read.
    #[test]
    fn req_0008_098_error_without_data_is_accepted() {
        let body = br#"{"jsonrpc":"2.0","id":"x","error":{"code":-32602,"message":"bad"}}"#;
        assert_eq!(
            parse_envelope(body).unwrap(),
            Envelope::Error(RpcError {
                code: -32602,
                message: "bad".to_owned()
            })
        );
    }

    /// 0008-098: a non-object body is malformed.
    #[test]
    fn req_0008_098_non_object_body_is_malformed() {
        assert!(parse_envelope(b"[1,2]").is_err());
        assert!(parse_envelope(b"nope").is_err());
    }

    /// 0008-098, 0008-078: code classes map as specified.
    #[test]
    fn req_0008_078_error_code_classes() {
        assert_eq!(classify_code(-32050, false), FailClass::Auth);
        assert_eq!(classify_code(-32052, false), FailClass::Auth);
        assert_eq!(classify_code(-32051, false), FailClass::RateLimited);
        assert_eq!(classify_code(-32001, true), FailClass::LostTask);
        assert_eq!(classify_code(-32001, false), FailClass::Other);
        assert_eq!(classify_code(-32602, true), FailClass::Other);
        assert_eq!(classify_status(401), Some(FailClass::Auth));
        assert_eq!(classify_status(429), Some(FailClass::RateLimited));
        assert_eq!(classify_status(500), None);
    }

    /// 0008-086, 0009-046: a SendMessage result with a task parses; text is artifacts then status.
    #[test]
    fn req_0009_046_task_result_parses_text() {
        let result = json!({"task": {
            "id": "task-1", "contextId": "ctx-1",
            "status": {"state": "TASK_STATE_COMPLETED", "message": {"parts": [{"text": "status"}]}},
            "artifacts": [{"parts": [{"text": "STUB_REPLY: ok", "mediaType": "text/plain"}]}]
        }});
        match parse_result(&result).unwrap() {
            Update::Task(view) => {
                assert_eq!(view.id, "task-1");
                assert_eq!(view.context_id, "ctx-1");
                assert_eq!(view.state, PeerState::Completed);
                assert_eq!(view.status_text, "status");
                assert_eq!(view.artifact_text, "STUB_REPLY: ok");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    /// 0009-046: a bare task (no task wrapper) is accepted.
    #[test]
    fn req_0009_046_bare_task_is_accepted() {
        let result = json!({"id": "t", "contextId": "c", "status": {"state": "completed"}});
        assert!(matches!(parse_result(&result), Ok(Update::Task(_))));
    }

    #[test]
    fn artifact_update_defaults_last_to_true() {
        let result = json!({"artifactUpdate": {
            "taskId": "t", "contextId": "c", "artifact": {"artifactId": "a", "parts": [{"text": "x"}]}
        }});
        match parse_result(&result).unwrap() {
            Update::Artifact {
                text, last, append, ..
            } => {
                assert_eq!(text, "x");
                assert!(last);
                assert!(!append);
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
