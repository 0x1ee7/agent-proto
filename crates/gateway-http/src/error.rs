//! Gateway error model: JSON-RPC codes, reasons, domains, and HTTP status (RFC-0008 4.6).

use std::time::Duration;

use a2a_client::ClientError;
use a2a_core::errors::{BUS_DOMAIN, LocalError, WireError};
use bus_api::error::BusError;
use http::StatusCode;

/// JSON-RPC code of `INTERNAL`.
pub const INTERNAL: i64 = -32603;
/// JSON-RPC code of `INVALID_PARAMS`.
pub const INVALID_PARAMS: i64 = -32602;
/// JSON-RPC code of `INVALID_REQUEST`.
pub const INVALID_REQUEST: i64 = -32600;
/// JSON-RPC code of `METHOD_NOT_FOUND`.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC code of `PARSE_ERROR`.
pub const PARSE_ERROR: i64 = -32700;
/// Gateway-local `UNAUTHORIZED` (0008-047).
pub const UNAUTHORIZED: i64 = -32050;
/// Gateway-local `RATE_LIMITED` (0008-047).
pub const RATE_LIMITED: i64 = -32051;
/// Gateway-local `PEER_NOT_TRUSTED` (0008-047).
pub const PEER_NOT_TRUSTED: i64 = -32052;

/// A gateway-visible error: one JSON-RPC error object plus its HTTP envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GwError {
    /// HTTP status of the response.
    pub status: StatusCode,
    /// JSON-RPC error code.
    pub code: i64,
    /// Machine-readable reason (`ErrorInfo.reason`).
    pub reason: &'static str,
    /// Reason domain (`ErrorInfo.domain`).
    pub domain: &'static str,
    /// Human-readable message. Never carries tokens, subjects, or internals.
    pub message: String,
    /// Seconds for the `Retry-After` header.
    pub retry_after: Option<u64>,
    /// Whether to send `WWW-Authenticate: Bearer` (0008-021).
    pub challenge: bool,
}

impl GwError {
    /// Gateway-local or bus-only error with the `a2abus.v1` domain and HTTP 200.
    #[must_use]
    pub fn new(code: i64, reason: &'static str, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::OK,
            code,
            reason,
            domain: BUS_DOMAIN,
            message: message.into(),
            retry_after: None,
            challenge: false,
        }
    }

    /// Error with an explicit HTTP status.
    #[must_use]
    pub fn http(
        status: StatusCode,
        code: i64,
        reason: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            status,
            ..Self::new(code, reason, message)
        }
    }

    /// Error corresponding to a protocol error (0008-046).
    #[must_use]
    pub fn wire(error: WireError) -> Self {
        let status = match error {
            WireError::Overloaded => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::OK,
        };
        Self {
            status,
            code: jsonrpc_code(error),
            reason: error.reason(),
            domain: error.domain(),
            message: error.message().to_owned(),
            retry_after: None,
            challenge: false,
        }
    }

    /// `INTERNAL` with no detail (0008-046).
    #[must_use]
    pub fn internal() -> Self {
        Self::new(INTERNAL, "INTERNAL", "internal error")
    }

    /// `INVALID_PARAMS` (-32602, A2A domain) with a fixed, non-sensitive message.
    #[must_use]
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            ..Self::wire(WireError::InvalidParams)
        }
    }

    /// Sets the `Retry-After` value; a zero delay becomes one second.
    #[must_use]
    pub fn with_retry_after(mut self, delay: Duration) -> Self {
        self.retry_after = Some(ceil_secs(delay));
        self
    }

    /// Marks the error as an authentication challenge.
    #[must_use]
    pub fn challenged(mut self) -> Self {
        self.challenge = true;
        self
    }

    /// Maps an error returned by the A2A client (0008-046, 0008-047).
    #[must_use]
    pub fn from_client(error: &ClientError) -> Self {
        match error {
            ClientError::Wire(wire, retry) => {
                let mapped = Self::wire(*wire);
                match retry {
                    Some(delay) => mapped.with_retry_after(*delay),
                    None => mapped,
                }
            }
            ClientError::Local(local) => Self::from_local(*local),
            ClientError::Bus(BusError::Overloaded { retry_after }) => {
                Self::wire(WireError::Overloaded).with_retry_after(*retry_after)
            }
            ClientError::Discovery(_) => Self::unavailable(),
            ClientError::InvalidReply(_) | ClientError::InvalidEvent(_) => {
                Self::wire(WireError::InvalidAgentResponse)
            }
            _ => Self::internal(),
        }
    }

    /// `AGENT_UNAVAILABLE`: HTTP 503 with a one-second retry hint.
    #[must_use]
    pub fn unavailable() -> Self {
        Self::http(
            StatusCode::SERVICE_UNAVAILABLE,
            INTERNAL,
            "AGENT_UNAVAILABLE",
            "agent is not available",
        )
        .with_retry_after(Duration::from_secs(1))
    }

    fn from_local(local: LocalError) -> Self {
        match local {
            LocalError::Unavailable => Self::unavailable(),
            LocalError::DeadlineExceeded => {
                Self::new(INTERNAL, "DEADLINE_EXCEEDED", "deadline exceeded")
            }
            LocalError::RetentionExceeded => Self::new(
                INTERNAL,
                "RETENTION_EXCEEDED",
                "requested position is no longer retained",
            ),
        }
    }
}

/// JSON-RPC code for a protocol error (RFC-0008 4.6 table).
#[must_use]
pub fn jsonrpc_code(error: WireError) -> i64 {
    match error {
        WireError::TaskNotFound => -32001,
        WireError::TaskNotCancelable => -32002,
        WireError::PushNotificationNotSupported => -32003,
        WireError::UnsupportedOperation | WireError::UnknownOperation => -32004,
        WireError::ContentTypeNotSupported => -32005,
        WireError::InvalidAgentResponse => -32006,
        WireError::ExtendedAgentCardNotConfigured => -32007,
        WireError::ExtensionSupportRequired => -32008,
        WireError::VersionNotSupported => -32009,
        WireError::InvalidParams | WireError::ExtensionDataInvalid | WireError::TenantMismatch => {
            INVALID_PARAMS
        }
        WireError::MalformedMessage
        | WireError::UnsupportedMediaType
        | WireError::PayloadTooLarge => INVALID_PARAMS,
        _ => INTERNAL,
    }
}

fn ceil_secs(delay: Duration) -> u64 {
    let secs = delay.as_secs() + u64::from(delay.subsec_nanos() > 0);
    secs.max(1)
}
