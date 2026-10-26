//! Failure classes and their process exit codes ([0012-055]).

use a2a_client::{ApprovalError, ClientError};
use a2a_core::errors::{LocalError, WireError};
use bus_api::error::BusError;

/// Process exit code ([0012-055]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Success.
    Success,
    /// Generic failure, including a broken audit chain.
    Failure,
    /// Usage error: bad arguments, or a confirmation that cannot be obtained.
    Usage,
    /// The named task, agent or request does not exist.
    NotFound,
    /// The request was rejected or the decision was denied.
    Rejected,
    /// The bound elapsed.
    Timeout,
    /// Supervisor lock held or bus unavailable.
    Unavailable,
}

impl Exit {
    /// Numeric process exit code.
    #[must_use]
    pub const fn code(self) -> i32 {
        match self {
            Self::Success => 0,
            Self::Failure => 1,
            Self::Usage => 2,
            Self::NotFound => 3,
            Self::Rejected => 4,
            Self::Timeout => 5,
            Self::Unavailable => 6,
        }
    }
}

/// A command failure with its class.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// Bad arguments or a missing confirmation.
    #[error("usage: {0}")]
    Usage(String),
    /// The named object does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// The request was refused or the decision denied.
    #[error("rejected: {0}")]
    Rejected(String),
    /// The bound elapsed.
    #[error("timed out: {0}")]
    Timeout(String),
    /// The bus or supervisor could not be reached.
    #[error("unavailable: {0}")]
    Unavailable(String),
    /// Any other failure.
    #[error("{0}")]
    Failed(String),
    /// A requester-runtime failure, classified by [`classify`].
    #[error(transparent)]
    Client(#[from] ClientError),
}

impl CliError {
    /// Exit code of this failure.
    #[must_use]
    pub fn exit(&self) -> Exit {
        match self {
            Self::Usage(_) => Exit::Usage,
            Self::NotFound(_) => Exit::NotFound,
            Self::Rejected(_) => Exit::Rejected,
            Self::Timeout(_) => Exit::Timeout,
            Self::Unavailable(_) => Exit::Unavailable,
            Self::Failed(_) => Exit::Failure,
            Self::Client(error) => classify(error),
        }
    }
}

/// Maps a requester-runtime error to its exit class ([0012-055]).
#[must_use]
pub fn classify(error: &ClientError) -> Exit {
    if let Some(wire) = error.wire() {
        return match wire {
            WireError::TaskNotFound => Exit::NotFound,
            WireError::PermissionDenied
            | WireError::Unauthenticated
            | WireError::TenantMismatch => Exit::Rejected,
            WireError::Overloaded | WireError::Draining => Exit::Unavailable,
            _ => Exit::Failure,
        };
    }
    if let Some(local) = error.local() {
        return match local {
            LocalError::DeadlineExceeded => Exit::Timeout,
            LocalError::Unavailable | LocalError::RetentionExceeded => Exit::Unavailable,
        };
    }
    match error {
        ClientError::Bus(bus) => match bus {
            BusError::Timeout => Exit::Timeout,
            BusError::NoResponder | BusError::Unavailable { .. } | BusError::Overloaded { .. } => {
                Exit::Unavailable
            }
            BusError::PermissionDenied => Exit::Rejected,
            _ => Exit::Failure,
        },
        ClientError::Approval(ApprovalError::Expired | ApprovalError::OptionNotOffered) => {
            Exit::Rejected
        }
        ClientError::Status(status) => match status.code {
            4 => Exit::Timeout,
            5 => Exit::NotFound,
            7 | 16 => Exit::Rejected,
            14 => Exit::Unavailable,
            _ => Exit::Failure,
        },
        _ => Exit::Failure,
    }
}
