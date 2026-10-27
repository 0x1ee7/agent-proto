//! Machine-readable failure kinds for workspace operations.

use std::fmt;
use std::io;

/// Failure kind. [`ErrorKind::token`] is the stable machine-readable form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// Workspace would equal, contain, or be contained by another live workspace or by an
    /// operator checkout, or the lease is held by a live process (RFC-0012 `shared_checkout`).
    SharedCheckout,
    /// Path exceeds the platform budget (RFC-0012 `path_too_long`).
    PathTooLong,
    /// Copy mode exceeded its byte or file limit (RFC-0012 `copy_limit`).
    CopyLimit,
    /// Invalid policy, identifier, source directory or base reference (RFC-0012 `config`).
    Config,
    /// Source is not a git checkout and the policy is `Refuse` (RFC-0012 `non_git_refused`).
    NonGitRefused,
    /// Copy mode met a symbolic link, junction, reparse point or special file
    /// (RFC-0012 `copy_link_rejected`).
    CopyLinkRejected,
    /// Target workspace path already exists and is not owned by the lease.
    PathOccupied,
    /// The lease branch already exists and cannot be replaced safely.
    BranchExists,
    /// The lease lock no longer belongs to the caller.
    LeaseLost,
    /// The workspace registry lock could not be acquired in time.
    Busy,
    /// A git invocation failed.
    Git,
    /// Filesystem failure.
    Io,
    /// Persistent state (index or lock record) could not be parsed.
    StateInvalid,
    /// A path would leave the workspace root.
    PathEscape,
}

impl ErrorKind {
    /// Every kind, one per RFC-0012 sec 4.8 token. Tests check these against `a2a-types`.
    pub const ALL: [ErrorKind; 14] = [
        Self::SharedCheckout,
        Self::PathTooLong,
        Self::CopyLimit,
        Self::Config,
        Self::NonGitRefused,
        Self::CopyLinkRejected,
        Self::PathOccupied,
        Self::BranchExists,
        Self::LeaseLost,
        Self::Busy,
        Self::Git,
        Self::Io,
        Self::StateInvalid,
        Self::PathEscape,
    ];

    /// Stable token, exactly as RFC-0012 sec 4.8 spells it.
    pub const fn token(self) -> &'static str {
        match self {
            Self::SharedCheckout => "shared_checkout",
            Self::PathTooLong => "path_too_long",
            Self::CopyLimit => "copy_limit",
            Self::Config => "config",
            Self::NonGitRefused => "non_git_refused",
            Self::CopyLinkRejected => "copy_link_rejected",
            Self::PathOccupied => "path_occupied",
            Self::BranchExists => "branch_exists",
            Self::LeaseLost => "lease_lost",
            Self::Busy => "workspace_busy",
            Self::Git => "git_failed",
            Self::Io => "io",
            Self::StateInvalid => "state_invalid",
            Self::PathEscape => "path_escape",
        }
    }
}

/// Error returned by every fallible workspace operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceError {
    kind: ErrorKind,
    detail: String,
}

impl WorkspaceError {
    pub(crate) fn new(kind: ErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }

    /// Machine-readable failure kind.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// Human-readable detail. May contain local absolute paths; never sent to peers.
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind.token(), self.detail)
    }
}

impl std::error::Error for WorkspaceError {}

impl From<io::Error> for WorkspaceError {
    fn from(err: io::Error) -> Self {
        Self::new(ErrorKind::Io, err.to_string())
    }
}

/// Result alias used across the crate.
pub type Result<T, E = WorkspaceError> = std::result::Result<T, E>;

pub(crate) fn config_err(detail: impl Into<String>) -> WorkspaceError {
    WorkspaceError::new(ErrorKind::Config, detail)
}

pub(crate) fn io_err(context: &str, err: io::Error) -> WorkspaceError {
    WorkspaceError::new(ErrorKind::Io, format!("{context}: {err}"))
}
