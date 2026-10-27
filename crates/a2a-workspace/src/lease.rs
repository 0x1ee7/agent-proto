//! Lease handles, lease records and acquire requests.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::ids::{AgentId, ScopeKind, WorkScope};

/// Kind of workspace behind a lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LeaseKind {
    /// `git worktree` of the operator's repository (RFC-0012 0012-032).
    Worktree,
    /// Copy of a non-git directory with a baseline repository (RFC-0012 0012-034).
    Copy,
}

/// Request to lease a workspace for `agent` at `scope`, rooted at `source`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquireRequest {
    /// Agent that will run in the workspace.
    pub agent: AgentId,
    /// Session or task scope with its key.
    pub scope: WorkScope,
    /// Operator directory to base the workspace on.
    pub source: PathBuf,
}

/// Persisted description of a lease. Also the content of the lease lock file and of
/// `index.json` entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct LeaseRecord {
    pub tenant: String,
    pub agent: String,
    pub scope: ScopeKind,
    pub key: String,
    pub short_id: String,
    pub kind: LeaseKind,
    pub path: PathBuf,
    pub source: PathBuf,
    pub branch: String,
    pub base_commit: String,
    pub source_dirty: bool,
    /// Entries the copy excluded by denylist, `copy_exclude` or git ignore rules. Zero for
    /// worktrees. Only the count is kept, never the names (RFC-0012 0012-074).
    #[serde(default)]
    pub excluded_count: usize,
}

/// Handle to a leased workspace. Release it with [`crate::Workspace::release`]. Dropping it does
/// not remove the workspace; its lock stays until release or until the owning process exits.
#[derive(Debug)]
pub struct Lease {
    record: LeaseRecord,
}

impl Lease {
    pub(crate) fn new(record: LeaseRecord) -> Self {
        Self { record }
    }

    pub(crate) fn record(&self) -> &LeaseRecord {
        &self.record
    }

    pub(crate) fn into_record(self) -> LeaseRecord {
        self.record
    }

    /// Owning agent identifier.
    pub fn agent(&self) -> &str {
        &self.record.agent
    }

    /// Session or task scope.
    pub fn scope(&self) -> ScopeKind {
        self.record.scope
    }

    /// Canonical session or task UUID.
    pub fn key(&self) -> &str {
        &self.record.key
    }

    /// Short id: first 12 hex digits of the key.
    pub fn short_id(&self) -> &str {
        &self.record.short_id
    }

    /// Working directory for the worker (never the operator checkout).
    pub fn path(&self) -> &Path {
        &self.record.path
    }

    /// Worktree or copy.
    pub fn kind(&self) -> LeaseKind {
        self.record.kind
    }

    /// Branch `a2abus/<agent>/<short-id>`.
    pub fn branch(&self) -> &str {
        &self.record.branch
    }

    /// Full SHA the workspace started from; the diff base for artifacts.
    pub fn base_commit(&self) -> &str {
        &self.record.base_commit
    }

    /// Operator checkout (repository top level) or copied source directory.
    pub fn source(&self) -> &Path {
        &self.record.source
    }

    /// True when the operator checkout had uncommitted or untracked changes at acquire time.
    /// Those changes are not part of the workspace, which starts from the base commit.
    pub fn source_dirty(&self) -> bool {
        self.record.source_dirty
    }

    /// Number of source entries excluded from a copy (RFC-0012 0012-074). Zero for worktrees.
    /// Recorded as `excluded_count` in `WORKSPACE_CREATED`; names are never recorded.
    pub fn excluded_count(&self) -> usize {
        self.record.excluded_count
    }
}

/// Outcome of releasing a lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseOutcome {
    /// Workspace (and branch) removed.
    Removed,
    /// Workspace kept and listed in `index.json`, with the reasons.
    Retained {
        /// Reason tokens such as `uncommitted_changes`, `untracked_files`, `commits_beyond_base`.
        reasons: Vec<String>,
    },
}
