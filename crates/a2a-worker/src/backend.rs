//! The `Backend` seam (RFC-0009 section 4.2, Appendix A; RFC-0001 L13).
//!
//! A driver is a worker-side view of one agent instance. It reports normalized
//! agent facts only: it never publishes to a bus, never assigns task ids,
//! sequence numbers or A2A states, and never answers a permission request on
//! its own choice (0009-001, 0009-002, 0009-033).
//!
//! Deviation from Appendix A (non-normative sketch, flagged for integrator
//! confirmation): the event sink is attached once with [`Backend::attach`]
//! instead of being passed to `start`. Events arrive asynchronously from the
//! agent process and a fault can occur between turns, so a per-call borrow
//! cannot carry them. Events still carry `turn` and `event_no` (0009-006).

use std::fmt;
use std::time::Duration;

use thiserror::Error;

/// Opaque driver-local reference to one turn. Unique within a driver instance.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct TurnRef(String);

impl TurnRef {
    /// Wraps a driver-local turn reference.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the reference text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for TurnRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TurnRef({})", self.0)
    }
}

/// Driver-local permission request identifier, unique within a driver instance.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct RequestId(String);

impl RequestId {
    /// Wraps a driver-local request identifier.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the identifier text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RequestId({})", self.0)
    }
}

/// Agent session handle, persisted by the worker per `context_id` (0009-012).
#[derive(Clone, PartialEq, Eq)]
pub struct SessionHandle(String);

impl SessionHandle {
    /// Wraps an agent session handle.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the handle text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SessionHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionHandle(<redacted>)")
    }
}

/// Wire protocol spoken by a driver (0009-011).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// ACP over stdio (`driver-acp`).
    Acp,
    /// A2A v1.0 over HTTP (`driver-a2a-http`).
    A2aHttp,
}

/// Usage metering source (0009-054).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageSource {
    /// Counters reported by the ACP agent.
    AcpUsage,
    /// Counters reported by an A2A peer.
    A2aPeer,
    /// Counters the driver counted itself (turns, wall time).
    DriverCounted,
}

/// Capability record answered by every driver (0009-011).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// Protocol the driver speaks.
    pub protocol: Protocol,
    /// Whether agent sessions can be restored after restart.
    pub resume: bool,
    /// Whether the agent honours `Cancel`.
    pub cancel: bool,
    /// Usage sources the driver can produce.
    pub usage_sources: Vec<UsageSource>,
    /// Canonical approval modes the agent runs under (0009-063).
    pub approval_modes: Vec<String>,
    /// Whether text arrives as streaming deltas.
    pub streaming_text: bool,
}

/// Liveness and restart state answered to `Health` (section 4.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Health {
    /// Whether the agent process or peer connection is up.
    pub live: bool,
    /// Number of driver-side restarts since the driver was created.
    pub restarts: u32,
    /// Why the backend is not live, if the driver knows (RFC-0005 0005-053). Unredacted; the
    /// worker redacts it before it reaches a failure record.
    pub reason: Option<String>,
}

/// Input content for a turn. M1 drivers forward text only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputPart {
    /// Plain text.
    Text(String),
}

/// Secret text that never prints. `Debug` and `Display` show `[redacted]`; the driver is the
/// only caller of [`SecretText::expose`] (RFC-0015 [0015-081]).
#[derive(Clone, PartialEq, Eq)]
pub struct SecretText(String);

impl SecretText {
    /// Wraps `value`.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the secret. Callers MUST NOT log the result.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

impl fmt::Display for SecretText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

/// Peer-call binding for one session (RFC-0015 [0015-084]). The driver serializes the fields
/// verbatim into the session's `mcpServers` stdio entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerTools {
    /// Session slot, base64url of 16 bytes.
    pub slot: String,
    /// Grant id, base64url of 16 bytes.
    pub grant_id: String,
    /// Grant credential, base64url of 32 bytes. Passed only in the entry `env`.
    pub grant_credential: SecretText,
    /// Absolute path of the delegation endpoint.
    pub endpoint: std::path::PathBuf,
    /// Absolute path of the MCP shim.
    pub shim_path: std::path::PathBuf,
}

/// `Start` command: begins one turn in the session for `context_id` (0009-020).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartTurn {
    /// Driver-local turn reference chosen by the worker.
    pub turn: TurnRef,
    /// A2A context identifier; the driver keys sessions by it (0009-012).
    pub context_id: String,
    /// Input parts of the message.
    pub input: Vec<InputPart>,
    /// Absolute deadline in worker clock milliseconds.
    pub deadline_ms: u64,
    /// Stored session handle to restore, if any (0009-013).
    pub session: Option<SessionHandle>,
    /// Origin agent of the submitting request: `A2AB-Origin` if set, else the sender id.
    pub origin: String,
    /// Peer-call binding for the session; `None` when the agent has no delegation table
    /// (RFC-0015 [0015-085]).
    pub peer_tools: Option<PeerTools>,
}

/// One offered permission option, as the agent sent it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionOption {
    /// Option identifier. The driver MUST NOT depend on it (0009-032).
    pub option_id: String,
    /// Option kind: `allow_once`, `allow_always`, `reject_once` or `reject_always`.
    pub kind: String,
}

/// Permission request from the agent (0009-032). Tool text is already redacted by the driver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRequest {
    /// Driver-local request identifier.
    pub request_id: RequestId,
    /// Tool kind, for example `execute`.
    pub tool_kind: String,
    /// Redacted tool title.
    pub tool_title: String,
    /// Options offered by the agent.
    pub options: Vec<PermissionOption>,
}

/// Answer to a permission request (0009-033).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionAnswer {
    /// Select one of the offered `option_id` values.
    Selected {
        /// Option identifier that was in the offered list.
        option_id: String,
    },
    /// Cancel the request.
    Cancelled,
}

/// Status event kinds (section 4.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusPayload {
    /// Non-terminal progress text.
    Progress(String),
    /// Operator-visible notice text.
    Notice(String),
    /// Agent session handle to persist (0009-012).
    SessionBound(SessionHandle),
}

/// Cost in integer minor units of an ISO 4217 currency (section 4.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cost {
    /// Cumulative amount in minor units of `currency`.
    pub units: u64,
    /// ISO 4217 currency code.
    pub currency: String,
}

/// Gauge values, reported separately from counters and never summed (section 4.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gauges {
    /// Context tokens currently in use.
    pub used: u64,
    /// Context window size.
    pub size: u64,
}

/// Metering sample. Only the fields the source provides are `Some` (0009-054).
///
/// Counters are per-turn deltas, except `cost`, which is cumulative for the
/// agent session; the worker takes high-water differences for cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageSample {
    /// Metering source.
    pub source: UsageSource,
    /// Turns counted.
    pub turns: Option<u64>,
    /// Wall time in milliseconds.
    pub wall_ms: Option<u64>,
    /// Input tokens.
    pub tokens_in: Option<u64>,
    /// Output tokens.
    pub tokens_out: Option<u64>,
    /// Cumulative cost.
    pub cost: Option<Cost>,
    /// Gauges, if the source reports them.
    pub gauges: Option<Gauges>,
}

/// Closed set of reason tokens (section 4.11, 0009-072). Adding one needs a revision of this RFC and RFC-0005.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReasonToken {
    /// Agent process exit or stdout EOF during an open turn; restart with an open turn.
    BackendCrashed,
    /// Agent could not be spawned, or `initialize` failed.
    BackendUnavailable,
    /// JSON-RPC error, unknown stop reason, auth failure or context mismatch.
    BackendError,
    /// Turn idle timeout, reply-timeout marker or client timeout.
    BackendTimeout,
    /// Peer lost the task (`-32001`, `-32002`).
    BackendLostTask,
    /// `max_tokens` or `max_turn_requests` stop reason.
    AgentLimit,
    /// `refusal` stop reason or peer `REJECTED`.
    AgentRefused,
    /// Peer HTTP 429 or `-32051`.
    RateLimited,
}

impl ReasonToken {
    /// Returns the bus token text.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BackendCrashed => "BACKEND_CRASHED",
            Self::BackendUnavailable => "BACKEND_UNAVAILABLE",
            Self::BackendError => "BACKEND_ERROR",
            Self::BackendTimeout => "BACKEND_TIMEOUT",
            Self::BackendLostTask => "BACKEND_LOST_TASK",
            Self::AgentLimit => "AGENT_LIMIT",
            Self::AgentRefused => "AGENT_REFUSED",
            Self::RateLimited => "RATE_LIMITED",
        }
    }

    /// Returns whether a driver of `protocol` may emit this token (section 4.11 producer column).
    pub fn producible_by(self, protocol: Protocol) -> bool {
        match self {
            Self::BackendCrashed | Self::BackendError | Self::BackendTimeout => true,
            Self::AgentRefused => true,
            Self::BackendUnavailable | Self::AgentLimit => protocol == Protocol::Acp,
            Self::BackendLostTask | Self::RateLimited => protocol == Protocol::A2aHttp,
        }
    }
}

/// Driver-local classification for bridge health. Never a bus token (section 4.11, 0009-072).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthClass {
    /// Peer rejected the credentials.
    PeerAuthFailed,
    /// Peer rate limited the driver.
    PeerRateLimited,
}

/// Terminal outcome of a turn (0009-007).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Turn ended normally.
    Completed,
    /// Turn ended after `Cancel`.
    Canceled,
    /// Peer or agent refused the turn.
    Refused {
        /// Reason token.
        reason: ReasonToken,
        /// Driver-local health class, optional.
        health_class: Option<HealthClass>,
    },
    /// Turn failed.
    Failed {
        /// Reason token.
        reason: ReasonToken,
        /// Driver-local health class, optional.
        health_class: Option<HealthClass>,
        /// Redacted operator-facing detail.
        detail: String,
    },
}

/// Driver-level fault kinds not tied to a turn outcome (section 4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultKind {
    /// Operator configuration refused at `Start` (0009-064), for example a dangerous mode.
    ConfigRefused,
    /// Peer answered a protocol version other than the one negotiated (0009-016).
    UnsupportedProtocolVersion,
    /// Agent stream ended without a turn outcome.
    StreamEnded,
    /// Driver-internal fault.
    Internal,
}

/// Event emitted by a driver. Turn-scoped events carry `turn` and `event_no` (0009-006).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendEvent {
    /// Non-terminal progress, notice or session binding.
    Status {
        /// Turn reference.
        turn: TurnRef,
        /// Per-turn sequence number, starts at 1.
        event_no: u64,
        /// Payload; the kind is derived from it.
        payload: StatusPayload,
    },
    /// Text delta for the turn's output artifact (0009-025).
    ArtifactDelta {
        /// Turn reference.
        turn: TurnRef,
        /// Per-turn sequence number.
        event_no: u64,
        /// Artifact key within the turn.
        artifact_key: String,
        /// Text.
        text: String,
        /// Append to the artifact rather than replace.
        append: bool,
        /// Final delta of the artifact.
        last: bool,
    },
    /// Metering sample (0009-054).
    Usage {
        /// Turn reference.
        turn: TurnRef,
        /// Per-turn sequence number.
        event_no: u64,
        /// Sample.
        sample: UsageSample,
    },
    /// Agent asks for permission (0009-032).
    PermissionRequest {
        /// Turn reference.
        turn: TurnRef,
        /// Per-turn sequence number.
        event_no: u64,
        /// Request.
        request: PermissionRequest,
    },
    /// Peer interrupted the turn and needs input (driver-a2a-http only).
    InputRequired {
        /// Turn reference.
        turn: TurnRef,
        /// Per-turn sequence number.
        event_no: u64,
        /// Prompt text for the operator.
        text: String,
    },
    /// Ends the turn; exactly one per turn (0009-007).
    Terminal {
        /// Turn reference.
        turn: TurnRef,
        /// Per-turn sequence number.
        event_no: u64,
        /// Outcome.
        outcome: Outcome,
    },
    /// Driver-level fault not tied to a turn outcome.
    Fault {
        /// Fault kind.
        kind: FaultKind,
        /// Redacted detail.
        detail: String,
    },
}

impl BackendEvent {
    /// Returns the turn reference, or `None` for a driver-level fault.
    pub fn turn(&self) -> Option<&TurnRef> {
        match self {
            Self::Status { turn, .. }
            | Self::ArtifactDelta { turn, .. }
            | Self::Usage { turn, .. }
            | Self::PermissionRequest { turn, .. }
            | Self::InputRequired { turn, .. }
            | Self::Terminal { turn, .. } => Some(turn),
            Self::Fault { .. } => None,
        }
    }

    /// Returns the per-turn event number, or `None` for a driver-level fault.
    pub fn event_no(&self) -> Option<u64> {
        match self {
            Self::Status { event_no, .. }
            | Self::ArtifactDelta { event_no, .. }
            | Self::Usage { event_no, .. }
            | Self::PermissionRequest { event_no, .. }
            | Self::InputRequired { event_no, .. }
            | Self::Terminal { event_no, .. } => Some(*event_no),
            Self::Fault { .. } => None,
        }
    }
}

/// Errors returned by command calls on a [`Backend`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BackendError {
    /// The agent process or peer connection is down.
    #[error("backend is closed")]
    Closed,
    /// A turn is already open and the driver accepts one at a time (0009-020).
    #[error("a turn is already open")]
    Busy,
    /// The driver does not support the requested feature.
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
    /// No event sink is attached; call [`Backend::attach`] first.
    #[error("no event sink attached")]
    NoSink,
}

/// Worker-side view of one agent instance. One implementation per driver crate (0009-001).
pub trait Backend: Send {
    /// Returns the capability record (0009-011).
    fn capabilities(&self) -> Capabilities;

    /// Returns liveness and restart state.
    fn health(&self) -> Health;

    /// Returns whether a turn can be accepted now and started later, after a lazy spawn or a
    /// respawn. `false` means the backend is closed, failed, or has given up until operator action;
    /// while it is `false` and the backend is not live, the Worker refuses requests by withdrawing
    /// its RPC subscription (0014-029). Defaults to `true`, so a backend without a stricter rule
    /// keeps serving as before.
    fn can_start(&self) -> bool {
        true
    }

    /// Attaches the event sink. Called once before the first `start`.
    fn attach(&mut self, sink: Box<dyn BackendSink>);

    /// Begins one turn (section 4.2). The driver keys sessions by `context_id` (0009-012).
    fn start(&mut self, cmd: StartTurn) -> Result<(), BackendError>;

    /// Answers one pending permission request. Unknown ids are ignored (0009-010).
    fn resolve_permission(
        &mut self,
        request_id: &RequestId,
        answer: PermissionAnswer,
    ) -> Result<(), BackendError>;

    /// Stops the turn. Idempotent; the driver still delivers exactly one `Terminal` (0009-009).
    fn cancel(&mut self, turn: &TurnRef) -> Result<(), BackendError>;

    /// Ends the agent process tree within `grace` (0009-059, 0009-060).
    fn shutdown(&mut self, grace: Duration) -> Result<(), BackendError>;
}

/// Driver-to-worker event sink. Events for one turn arrive in `event_no` order (0009-006).
pub trait BackendSink: Send {
    /// Delivers one event.
    fn emit(&mut self, event: BackendEvent);
}

/// Sink that forwards events into a standard channel.
#[derive(Debug)]
pub struct ChannelSink(std::sync::mpsc::Sender<BackendEvent>);

impl ChannelSink {
    /// Creates a sink and the receiving end.
    pub fn channel() -> (Self, std::sync::mpsc::Receiver<BackendEvent>) {
        let (tx, rx) = std::sync::mpsc::channel();
        (Self(tx), rx)
    }
}

impl BackendSink for ChannelSink {
    fn emit(&mut self, event: BackendEvent) {
        // A closed receiver means the worker stopped; the event has no reader.
        let _ = self.0.send(event);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Guards 0009-072: only the listed producers may emit each reason token.
    #[test]
    fn reason_token_producers_follow_section_4_11() {
        assert!(ReasonToken::BackendCrashed.producible_by(Protocol::Acp));
        assert!(ReasonToken::BackendCrashed.producible_by(Protocol::A2aHttp));
        assert!(ReasonToken::BackendUnavailable.producible_by(Protocol::Acp));
        assert!(!ReasonToken::BackendUnavailable.producible_by(Protocol::A2aHttp));
        assert!(ReasonToken::AgentLimit.producible_by(Protocol::Acp));
        assert!(!ReasonToken::AgentLimit.producible_by(Protocol::A2aHttp));
        assert!(ReasonToken::BackendLostTask.producible_by(Protocol::A2aHttp));
        assert!(!ReasonToken::BackendLostTask.producible_by(Protocol::Acp));
        assert!(ReasonToken::RateLimited.producible_by(Protocol::A2aHttp));
        assert!(!ReasonToken::RateLimited.producible_by(Protocol::Acp));
    }

    /// Guards 0009-072: tokens are the exact bus strings.
    #[test]
    fn reason_token_strings_are_bus_tokens() {
        assert_eq!(ReasonToken::BackendCrashed.as_str(), "BACKEND_CRASHED");
        assert_eq!(ReasonToken::AgentRefused.as_str(), "AGENT_REFUSED");
        assert_eq!(ReasonToken::RateLimited.as_str(), "RATE_LIMITED");
    }

    /// Guards 0009-006: turn-scoped events expose turn and event_no; faults expose neither.
    #[test]
    fn event_accessors_expose_turn_scope() {
        let event = BackendEvent::Terminal {
            turn: TurnRef::new("turn-0001"),
            event_no: 3,
            outcome: Outcome::Completed,
        };
        assert_eq!(event.turn().map(TurnRef::as_str), Some("turn-0001"));
        assert_eq!(event.event_no(), Some(3));
        let fault = BackendEvent::Fault {
            kind: FaultKind::Internal,
            detail: String::new(),
        };
        assert!(fault.turn().is_none());
        assert!(fault.event_no().is_none());
    }

    /// Guards 0009-012: session handles never appear in Debug output.
    #[test]
    fn session_handle_debug_is_redacted() {
        let handle = SessionHandle::new("secret-session");
        assert!(!format!("{handle:?}").contains("secret-session"));
    }

    /// Guards 0009-006: the channel sink delivers events in order.
    #[test]
    fn channel_sink_preserves_order() {
        let (mut sink, rx) = ChannelSink::channel();
        sink.emit(BackendEvent::Fault {
            kind: FaultKind::StreamEnded,
            detail: "a".to_owned(),
        });
        sink.emit(BackendEvent::Fault {
            kind: FaultKind::Internal,
            detail: "b".to_owned(),
        });
        let first = rx.recv().expect("first event");
        let second = rx.recv().expect("second event");
        assert!(matches!(first, BackendEvent::Fault { detail, .. } if detail == "a"));
        assert!(matches!(second, BackendEvent::Fault { detail, .. } if detail == "b"));
    }
}
