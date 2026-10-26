//! Workspace isolation for supervised agents (RFC-0012).
//!
//! A worker never runs in the operator's checkout. [`Workspace::acquire`] leases a `git worktree`
//! (or, for non-git sources under the copy policy, a baseline copy) per session or per task. The
//! lease is guarded by a lock file that names the owning process, and [`Workspace::release`]
//! removes clean workspaces and retains dirty ones with reasons. The API is blocking; call it
//! from a blocking thread.

#![forbid(unsafe_code)]

mod artifacts;
mod config;
mod copy;
mod error;
mod git;
mod ids;
mod index;
mod lease;
mod owner;
mod platform;
mod workspace;

pub use artifacts::{ArtifactSummary, FileSummary};
pub use config::{
    CopyLimits, GIB, NonGitPolicy, PATCH_CAP_BYTES, PATCH_TRUNCATED_MARKER, Retention,
    WorkspaceConfig, WorkspaceMode,
};
pub use error::{ErrorKind, Result, WorkspaceError};
pub use ids::{AgentId, ScopeKind, WorkKey, WorkScope, WorkspaceRef};
pub use lease::{AcquireRequest, Lease, LeaseKind, ReleaseOutcome};
pub use workspace::{GcReport, Workspace};
