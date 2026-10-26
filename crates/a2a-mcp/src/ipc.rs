//! Worker delegation IPC protocol (RFC-0015 section 4.5).
//!
//! Runtime-agnostic: message types, length-prefixed framing, the grant
//! handshake, argument validation and a constant-time comparison helper. The
//! shim and the worker's delegation endpoint both use this module.
//!
//! Frame: 4-byte unsigned big-endian length, then one UTF-8 JSON object of at
//! most [`MAX_FRAME_BYTES`] bytes. A zero length is refused.

use std::collections::BTreeMap;
use std::fmt;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// IPC protocol version carried in `hello` (RFC-0015 [0015-041]).
pub const IPC_VERSION: u32 = 1;
/// Largest accepted frame payload in bytes (RFC-0015 [0015-040]).
pub const MAX_FRAME_BYTES: usize = 262_144;
/// Largest `deadlineMs` and `timeoutMs` value (RFC-0015 [0015-044], [0015-021]).
pub const MAX_DEADLINE_MS: u64 = 3_600_000;
/// Grant id length in bytes before base64url encoding (RFC-0015 [0015-042]).
pub const GRANT_ID_BYTES: usize = 16;
/// Grant credential length in bytes before base64url encoding (RFC-0015 [0015-042]).
pub const GRANT_CREDENTIAL_BYTES: usize = 32;
/// Session slot length in bytes before base64url encoding (RFC-0015 [0015-042]).
pub const SESSION_SLOT_BYTES: usize = 16;
/// Largest `a2a_send` text in bytes (RFC-0015 [0015-021]).
pub const MAX_SEND_TEXT_BYTES: usize = 32_768;
/// Largest tool result text in bytes (RFC-0015 [0015-032]).
pub const MAX_RESULT_TEXT_BYTES: usize = 65_536;

/// Reason tokens carried in `error.reason` (RFC-0015 [0015-045]).
pub mod reason {
    // Bus-level (RFC-0002 [0002-092]).
    pub const UNAUTHENTICATED: &str = "UNAUTHENTICATED";
    pub const PERMISSION_DENIED: &str = "PERMISSION_DENIED";
    pub const REQUEST_EXPIRED: &str = "REQUEST_EXPIRED";
    pub const MALFORMED_MESSAGE: &str = "MALFORMED_MESSAGE";
    pub const OVERLOADED: &str = "OVERLOADED";
    pub const UNAVAILABLE: &str = "UNAVAILABLE";
    pub const DEADLINE_EXCEEDED: &str = "DEADLINE_EXCEEDED";
    // A2A (RFC-0002 [0002-091]).
    pub const TASK_NOT_FOUND: &str = "TASK_NOT_FOUND";
    // Failure and rejection codes (RFC-0005, RFC-0006).
    pub const BUDGET_EXHAUSTED: &str = "BUDGET_EXHAUSTED";
    pub const LOOP_DETECTED: &str = "LOOP_DETECTED";
    pub const CHILD_FAILED: &str = "CHILD_FAILED";
    pub const RATE_LIMITED: &str = "RATE_LIMITED";
    pub const QUEUE_FULL: &str = "QUEUE_FULL";
    pub const BACKEND_TIMEOUT: &str = "BACKEND_TIMEOUT";
    // IPC-local (RFC-0015 section 4.5).
    pub const IPC_VERSION_UNSUPPORTED: &str = "IPC_VERSION_UNSUPPORTED";
    pub const SLOT_UNBOUND: &str = "SLOT_UNBOUND";
    pub const GRANT_REVOKED: &str = "GRANT_REVOKED";
}

/// Fixed short English message for a reason token (RFC-0002 [0002-095]).
///
/// Unknown tokens map to a generic sentence, so no peer text reaches the caller.
pub fn message_for(reason: &str) -> &'static str {
    match reason {
        reason::UNAUTHENTICATED => "Delegation credential was not accepted.",
        reason::PERMISSION_DENIED => "The grant does not allow this operation.",
        reason::REQUEST_EXPIRED => "The request deadline passed.",
        reason::MALFORMED_MESSAGE => "The request does not match the protocol.",
        reason::OVERLOADED => "The bus is overloaded; retry later.",
        reason::UNAVAILABLE => "The delegation endpoint is unavailable.",
        reason::DEADLINE_EXCEEDED => "The call deadline passed before a reply.",
        reason::TASK_NOT_FOUND => "No task with that id is visible to this grant.",
        reason::BUDGET_EXHAUSTED => "A delegation budget is spent.",
        reason::LOOP_DETECTED => "The delegation would repeat a chain entry.",
        reason::CHILD_FAILED => "The delegated task ended in FAILED.",
        reason::RATE_LIMITED => "The per-peer rate limit was exceeded.",
        reason::QUEUE_FULL => "The peer queue is full.",
        reason::BACKEND_TIMEOUT => "The peer backend timed out.",
        reason::IPC_VERSION_UNSUPPORTED => "The IPC protocol version is not supported.",
        reason::SLOT_UNBOUND => "The session slot is not bound to this grant.",
        reason::GRANT_REVOKED => "The grant has been revoked.",
        _ => "The delegation call failed.",
    }
}

/// Errors from framing and JSON coding. Messages carry no frame content.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    #[error("frame length {0} is outside 1..=262144")]
    BadLength(usize),
    #[error("frame is not a valid JSON message of the expected shape")]
    Decode,
    #[error("message could not be encoded as JSON")]
    Encode,
}

/// Encodes one frame: 4-byte big-endian length followed by the JSON body.
pub fn encode_frame<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, FrameError> {
    let mut out = vec![0u8; 4];
    serde_json::to_writer(&mut out, value).map_err(|_| FrameError::Encode)?;
    let len = out.len() - 4;
    if len == 0 || len > MAX_FRAME_BYTES {
        return Err(FrameError::BadLength(len));
    }
    let header = u32::try_from(len).map_err(|_| FrameError::BadLength(len))?;
    out[..4].copy_from_slice(&header.to_be_bytes());
    Ok(out)
}

/// Validates a frame header and returns the payload length.
pub fn decode_len(header: [u8; 4]) -> Result<usize, FrameError> {
    let len = u32::from_be_bytes(header) as usize;
    if len == 0 || len > MAX_FRAME_BYTES {
        return Err(FrameError::BadLength(len));
    }
    Ok(len)
}

/// Parses one frame payload (without the length header).
pub fn decode_payload<T: DeserializeOwned>(payload: &[u8]) -> Result<T, FrameError> {
    serde_json::from_slice(payload).map_err(|_| FrameError::Decode)
}

/// Grant credential material. `Debug` never prints the value.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrows the raw value. Callers must not log it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// Failure detail carried in `error` (RFC-0015 [0015-045]).
///
/// `metadata` holds string values such as `taskId`, `state`, `dimension` or
/// `breaker`, mirroring `ErrorInfo.metadata` (RFC-0002 [0002-096]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcError {
    pub reason: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

impl IpcError {
    /// Error with the fixed message for `reason`.
    pub fn new(reason: &str) -> Self {
        Self {
            reason: reason.to_owned(),
            message: message_for(reason).to_owned(),
            metadata: BTreeMap::new(),
        }
    }

    /// Adds one metadata entry.
    pub fn with_meta(mut self, key: &str, value: &str) -> Self {
        self.metadata.insert(key.to_owned(), value.to_owned());
        self
    }
}

/// First frame from the shim (RFC-0015 [0015-041]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hello {
    pub ipc_version: u32,
    #[serde(default)]
    pub shim: String,
}

/// Server answer to `hello`: its version on accept, or an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HelloReply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ipc_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<IpcError>,
}

impl HelloReply {
    /// Accepts the handshake and advertises [`IPC_VERSION`].
    pub fn accept() -> Self {
        Self {
            ok: true,
            ipc_version: Some(IPC_VERSION),
            error: None,
        }
    }

    /// Rejects the handshake.
    pub fn reject(error: IpcError) -> Self {
        Self {
            ok: false,
            ipc_version: None,
            error: Some(error),
        }
    }
}

/// Second frame from the shim (RFC-0015 [0015-042]). Base64url fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Auth {
    pub grant_id: String,
    pub grant_credential: Secret,
    pub session_slot: String,
}

/// Server answer to `auth`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthReply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<IpcError>,
}

impl AuthReply {
    pub fn accept() -> Self {
        Self {
            ok: true,
            error: None,
        }
    }

    pub fn reject(error: IpcError) -> Self {
        Self {
            ok: false,
            error: Some(error),
        }
    }
}

/// Wait mode of `a2a_send` (RFC-0015 [0015-021]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Wait {
    #[default]
    Blocking,
    Return,
}

/// `discover` parameters (RFC-0015 [0015-020]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DiscoverParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// `send` parameters: the tool arguments plus the idempotency key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SendParams {
    pub agent: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    #[serde(default)]
    pub wait: Wait,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// MCP request id joined with the session slot (RFC-0015 [0015-047]).
    pub idempotency_key: String,
}

/// `get` parameters (RFC-0015 [0015-022]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GetParams {
    pub task_id: String,
    #[serde(default = "default_include_text")]
    pub include_text: bool,
}

fn default_include_text() -> bool {
    true
}

/// `cancel` parameters (RFC-0015 [0015-023]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CancelParams {
    pub task_id: String,
}

/// Client frames after authentication (RFC-0015 [0015-044], [0015-046]).
///
/// Internally tagged by `method`. `cancel-call` names the in-flight request
/// `call_id`; the server answers it with `ok: true`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all_fields = "camelCase")]
pub enum ClientFrame {
    #[serde(rename = "discover")]
    Discover {
        id: u64,
        deadline_ms: u64,
        params: DiscoverParams,
    },
    #[serde(rename = "send")]
    Send {
        id: u64,
        deadline_ms: u64,
        params: SendParams,
    },
    #[serde(rename = "get")]
    Get {
        id: u64,
        deadline_ms: u64,
        params: GetParams,
    },
    #[serde(rename = "cancel")]
    Cancel {
        id: u64,
        deadline_ms: u64,
        params: CancelParams,
    },
    #[serde(rename = "cancel-call")]
    CancelCall { id: u64, call_id: u64 },
}

impl ClientFrame {
    /// Frame id; unique per connection and nonzero.
    pub fn id(&self) -> u64 {
        match self {
            Self::Discover { id, .. }
            | Self::Send { id, .. }
            | Self::Get { id, .. }
            | Self::Cancel { id, .. }
            | Self::CancelCall { id, .. } => *id,
        }
    }

    /// Deadline of a request; `None` for `cancel-call`.
    pub fn deadline_ms(&self) -> Option<u64> {
        match self {
            Self::Discover { deadline_ms, .. }
            | Self::Send { deadline_ms, .. }
            | Self::Get { deadline_ms, .. }
            | Self::Cancel { deadline_ms, .. } => Some(*deadline_ms),
            Self::CancelCall { .. } => None,
        }
    }
}

/// Server response to a request (RFC-0015 [0015-045]).
///
/// `id` is 0 only when the server could not read the request id (for example
/// after a malformed frame).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<IpcError>,
}

impl Response {
    /// Successful response carrying `result`.
    pub fn success<T: Serialize>(id: u64, result: &T) -> Result<Self, FrameError> {
        let value = serde_json::to_value(result).map_err(|_| FrameError::Encode)?;
        Ok(Self {
            id,
            ok: true,
            result: Some(value),
            error: None,
        })
    }

    /// Failed response.
    pub fn failure(id: u64, error: IpcError) -> Self {
        Self {
            id,
            ok: false,
            result: None,
            error: Some(error),
        }
    }

    /// Converts to the typed result, or the error the server reported.
    pub fn into_result<T: DeserializeOwned>(self) -> Result<T, IpcError> {
        if !self.ok {
            return Err(self
                .error
                .unwrap_or_else(|| IpcError::new(reason::MALFORMED_MESSAGE)));
        }
        let value = self
            .result
            .ok_or_else(|| IpcError::new(reason::MALFORMED_MESSAGE))?;
        serde_json::from_value(value).map_err(|_| IpcError::new(reason::MALFORMED_MESSAGE))
    }
}

/// One peer in a `discover` result (RFC-0015 [0015-033]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerEntry {
    pub agent: String,
    pub name: String,
    pub description: String,
    pub skills: Vec<SkillRef>,
    pub tags: Vec<String>,
}

/// Skill reference inside a [`PeerEntry`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillRef {
    pub id: String,
    pub name: String,
}

/// `discover` result. `text` is the framed rendering (RFC-0007 [0007-125]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoverResult {
    pub text: String,
    pub peers: Vec<PeerEntry>,
}

/// Lineage of a delegated task (RFC-0015 [0015-034]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Provenance {
    pub source_agent: String,
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegating_task_id: Option<String>,
    pub chain_length: u32,
}

/// Artifact summary in a task result (RFC-0015 [0015-034]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactSummary {
    pub artifact_id: String,
    pub name: String,
    pub media_type: String,
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob_ref: Option<String>,
}

/// `send` and `get` result (RFC-0015 [0015-032], [0015-034]).
///
/// `text` is framed and capped at [`MAX_RESULT_TEXT_BYTES`] by the worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskResult {
    pub task_id: String,
    pub context_id: String,
    pub state: String,
    pub text: String,
    pub artifacts: Vec<ArtifactSummary>,
    pub truncated: bool,
    pub omitted_bytes: u64,
    pub provenance: Provenance,
}

/// `cancel` result (RFC-0015 [0015-035]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelResult {
    pub task_id: String,
    pub state: String,
}

fn malformed() -> IpcError {
    IpcError::new(reason::MALFORMED_MESSAGE)
}

/// Checks a request `deadlineMs` (1..=3600000, RFC-0015 [0015-044]).
pub fn validate_deadline(deadline_ms: u64) -> Result<(), IpcError> {
    if (1..=MAX_DEADLINE_MS).contains(&deadline_ms) {
        Ok(())
    } else {
        Err(malformed())
    }
}

/// Checks an agent id against `^[a-z0-9][a-z0-9-]{0,47}$` (RFC-0015 [0015-021]).
pub fn is_agent_id(value: &str) -> bool {
    let Some((first, rest)) = value.as_bytes().split_first() else {
        return false;
    };
    rest.len() <= 47
        && (first.is_ascii_lowercase() || first.is_ascii_digit())
        && rest
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// Checks a lowercase UUIDv7 task id, 36 characters (RFC-0015 [0015-022]).
pub fn is_task_id(value: &str) -> bool {
    let b = value.as_bytes();
    if b.len() != 36 {
        return false;
    }
    for (i, &c) in b.iter().enumerate() {
        let ok = match i {
            8 | 13 | 18 | 23 => c == b'-',
            14 => c == b'7',
            _ => c.is_ascii_digit() || (b'a'..=b'f').contains(&c),
        };
        if !ok {
            return false;
        }
    }
    true
}

/// Checks text: 1..=32768 bytes, no control characters except tab, LF and CR
/// (RFC-0015 [0015-021], RFC-0007 [0007-125]).
pub fn validate_send_text(text: &str) -> Result<(), IpcError> {
    let len = text.len();
    if len == 0 || len > MAX_SEND_TEXT_BYTES {
        return Err(malformed());
    }
    if text
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r'))
    {
        return Err(malformed());
    }
    Ok(())
}

/// Checks a bounded printable-ASCII identifier of 1..=`max` bytes.
pub fn is_bounded_ascii(value: &str, max: usize) -> bool {
    (1..=max).contains(&value.len()) && value.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

/// Validates `discover` parameters (RFC-0015 [0015-020]).
pub fn validate_discover(params: &DiscoverParams) -> Result<(), IpcError> {
    if let Some(skill) = &params.skill {
        if !(1..=128).contains(&skill.len()) {
            return Err(malformed());
        }
    }
    if let Some(tag) = &params.tag {
        if !(1..=64).contains(&tag.len()) {
            return Err(malformed());
        }
    }
    if let Some(limit) = params.limit {
        if !(1..=50).contains(&limit) {
            return Err(malformed());
        }
    }
    Ok(())
}

/// Validates `send` parameters (RFC-0015 [0015-021], [0015-047]).
pub fn validate_send(params: &SendParams) -> Result<(), IpcError> {
    if !is_agent_id(&params.agent) {
        return Err(malformed());
    }
    validate_send_text(&params.text)?;
    if let Some(ctx) = &params.context_id {
        if !is_bounded_ascii(ctx, 128) {
            return Err(malformed());
        }
    }
    if let Some(ms) = params.timeout_ms {
        validate_deadline(ms)?;
    }
    if !is_bounded_ascii(&params.idempotency_key, 128) {
        return Err(malformed());
    }
    Ok(())
}

/// Validates `get` parameters (RFC-0015 [0015-022]).
pub fn validate_get(params: &GetParams) -> Result<(), IpcError> {
    if is_task_id(&params.task_id) {
        Ok(())
    } else {
        Err(malformed())
    }
}

/// Validates `cancel` parameters (RFC-0015 [0015-023]).
pub fn validate_cancel(params: &CancelParams) -> Result<(), IpcError> {
    if is_task_id(&params.task_id) {
        Ok(())
    } else {
        Err(malformed())
    }
}

/// Decodes a base64url (no padding) field and checks its byte length.
pub fn decode_b64url(value: &str, expected_len: usize) -> Option<Vec<u8>> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value)
        .ok()?;
    (bytes.len() == expected_len).then_some(bytes)
}

/// Compares two byte slices in time independent of their contents.
///
/// Returns `false` for unequal lengths. Used for grant credential checks.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    // RFC-0015 [0015-040]: framing, length limits.
    #[test]
    fn frame_round_trip_0015_040() {
        let frame = encode_frame(&Hello {
            ipc_version: IPC_VERSION,
            shim: "0.1.0".to_owned(),
        })
        .expect("encode");
        let header = [frame[0], frame[1], frame[2], frame[3]];
        let len = decode_len(header).expect("len");
        assert_eq!(len, frame.len() - 4);
        let hello: Hello = decode_payload(&frame[4..]).expect("decode");
        assert_eq!(hello.ipc_version, 1);
    }

    #[test]
    fn zero_and_oversized_lengths_refused_0015_040() {
        assert_eq!(decode_len([0, 0, 0, 0]), Err(FrameError::BadLength(0)));
        let too_big = u32::try_from(MAX_FRAME_BYTES + 1)
            .expect("fits")
            .to_be_bytes();
        assert!(decode_len(too_big).is_err());
        let max = u32::try_from(MAX_FRAME_BYTES).expect("fits").to_be_bytes();
        assert_eq!(decode_len(max), Ok(MAX_FRAME_BYTES));
    }

    // RFC-0015 [0015-042]: credential is never printed.
    #[test]
    fn secret_debug_redacted_0015_042() {
        let s = Secret::new("very-secret-value");
        assert!(!format!("{s:?}").contains("very-secret"));
    }

    // RFC-0015 [0015-044]: request shape and cancel-call.
    #[test]
    fn client_frame_wire_shape_0015_044() {
        let frame = ClientFrame::Send {
            id: 7,
            deadline_ms: 15_000,
            params: SendParams {
                agent: "reviewer".to_owned(),
                text: "hi".to_owned(),
                context_id: None,
                wait: Wait::Blocking,
                timeout_ms: Some(10_000),
                idempotency_key: "3:slot".to_owned(),
            },
        };
        let v = serde_json::to_value(&frame).expect("json");
        assert_eq!(v["method"], "send");
        assert_eq!(v["deadlineMs"], 15_000);
        assert_eq!(v["params"]["idempotencyKey"], "3:slot");
        let cancel: ClientFrame =
            serde_json::from_str(r#"{"id":9,"method":"cancel-call","callId":7}"#).expect("parse");
        assert_eq!(cancel, ClientFrame::CancelCall { id: 9, call_id: 7 });
    }

    // RFC-0015 [0015-021]: send validation.
    #[test]
    fn send_validation_0015_021() {
        let base = SendParams {
            agent: "reviewer".to_owned(),
            text: "hi".to_owned(),
            context_id: None,
            wait: Wait::Blocking,
            timeout_ms: None,
            idempotency_key: "1:s".to_owned(),
        };
        assert!(validate_send(&base).is_ok());
        let bad_agent = SendParams {
            agent: "Bad_Agent".to_owned(),
            ..base.clone()
        };
        assert!(validate_send(&bad_agent).is_err());
        let empty = SendParams {
            text: String::new(),
            ..base.clone()
        };
        assert!(validate_send(&empty).is_err());
        let control = SendParams {
            text: "a\u{0}b".to_owned(),
            ..base
        };
        assert!(validate_send(&control).is_err());
    }

    // RFC-0015 [0015-022]: UUIDv7 lowercase task ids.
    #[test]
    fn task_id_format_0015_022() {
        assert!(is_task_id("0193a6df-7c2e-7a1b-8c3d-4e5f60718293"));
        assert!(!is_task_id("0193A6DF-7C2E-7A1B-8C3D-4E5F60718293"));
        assert!(!is_task_id("0193a6df-7c2e-4a1b-8c3d-4e5f60718293"));
    }

    // RFC-0015 [0015-045]: response reason mapping.
    #[test]
    fn response_into_result_0015_045() {
        let err = Response::failure(3, IpcError::new(reason::CHILD_FAILED));
        let out: Result<CancelResult, IpcError> = err.into_result();
        assert_eq!(out.expect_err("error").reason, reason::CHILD_FAILED);
    }

    #[test]
    fn constant_time_eq_works() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
