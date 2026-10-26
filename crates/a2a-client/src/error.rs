//! Typed errors for the requester runtime.

use std::time::Duration;

use a2a_core::budget::DelegationRefused;
use a2a_core::envelope::EncodeError;
use a2a_core::errors::{self, LocalError, WireError};
use a2a_core::headers::HeaderError;
use a2a_security::error::SecurityError;
use a2a_types::lifecycle::LifecycleError;
use a2a_types::proto::rpc;
use bus_api::{error::BusError, name::InvalidName};

/// Errors returned by the requester runtime.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The request could not be encoded for the bus.
    #[error("encode: {0:?}")]
    Encode(EncodeError),
    /// A locally generated failure: no responder, timeout, or retention loss.
    #[error("local: {0:?}")]
    Local(LocalError),
    /// A bus failure that is not mapped to a local protocol error.
    #[error("bus: {0:?}")]
    Bus(BusError),
    /// An A2A or bus-level protocol error reply (`google.rpc.Status` with `ErrorInfo`).
    #[error("wire: {0:?}")]
    Wire(WireError, Option<Duration>),
    /// An error reply whose `ErrorInfo` is not a known A2A-Bus reason.
    #[error("status: {0:?}")]
    Status(rpc::Status),
    /// A reply was missing, malformed, or did not answer the request.
    #[error("invalid reply: {0}")]
    InvalidReply(&'static str),
    /// An event was malformed or did not belong to the followed task.
    #[error("invalid event: {0}")]
    InvalidEvent(&'static str),
    /// The local clock could not be read; the request fails closed.
    #[error("local clock unreadable")]
    Clock,
    /// A name or identifier was rejected.
    #[error("invalid name: {0:?}")]
    Name(InvalidName),
    /// A header value was rejected.
    #[error("invalid header: {0:?}")]
    Header(HeaderError),
    /// An approval request or decision failed a check.
    #[error("approval: {0}")]
    Approval(ApprovalError),
    /// Child delegation was refused by the budget rules.
    #[error("delegation refused: {0:?}")]
    Delegation(DelegationRefused),
    /// The child target already appears in the lineage of this task.
    #[error("loop detected in task lineage")]
    LoopDetected,
    /// Discovery state is missing or too old to route on.
    #[error("discovery: {0}")]
    Discovery(&'static str),
    /// A signing or verification hook refused the message.
    #[error("hook: {0}")]
    Hook(&'static str),
    /// A card or signature check failed.
    #[error("security: {0:?}")]
    Security(SecurityError),
    /// A task state value was not recognised.
    #[error("lifecycle: {0:?}")]
    Lifecycle(LifecycleError),
}

impl ClientError {
    /// Returns the A2A or bus-level reason for protocol errors.
    #[must_use]
    pub fn wire(&self) -> Option<WireError> {
        match self {
            Self::Wire(w, _) => Some(*w),
            _ => None,
        }
    }

    /// Returns the local protocol error, if any.
    #[must_use]
    pub fn local(&self) -> Option<LocalError> {
        match self {
            Self::Local(l) => Some(*l),
            _ => None,
        }
    }

    /// Returns the retry hint carried by the error, if any.
    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Bus(BusError::Overloaded { retry_after }) => Some(*retry_after),
            Self::Wire(_, retry) => *retry,
            Self::Status(status) => errors::retry_after(status),
            _ => None,
        }
    }
}

/// Maps a `google.rpc.Status` reply into a typed client error.
pub(crate) fn from_status(status: rpc::Status) -> ClientError {
    match errors::classify(&status) {
        Some(wire) => ClientError::Wire(wire, errors::retry_after(&status)),
        None => ClientError::Status(status),
    }
}

/// Maps a bus failure, promoting the local protocol errors of RFC-0002 section 9.
pub(crate) fn from_bus(error: BusError) -> ClientError {
    match LocalError::from_bus(&error) {
        Some(local) => ClientError::Local(local),
        None => ClientError::Bus(error),
    }
}

/// Reasons an approval request or decision is not applicable (RFC-0006 section 4.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ApprovalError {
    /// The request and decision do not bind (digest, nonce, ids, or option).
    #[error("approval not applicable")]
    NotApplicable,
    /// The request expired before the decision.
    #[error("approval request expired")]
    Expired,
    /// The option is not among the options of the request.
    #[error("option not offered by request")]
    OptionNotOffered,
    /// The message carries no approval request.
    #[error("message carries no approval request")]
    Missing,
    /// The approval data could not be encoded or decoded.
    #[error("approval data malformed")]
    Malformed,
}
