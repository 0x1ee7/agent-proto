//! Workspace policy, roots, retention and copy limits (RFC-0012 sections 4.8 and 4.9).

use std::path::PathBuf;
use std::time::Duration;

use crate::error::{Result, config_err};

/// One gibibyte in bytes.
pub const GIB: u64 = 1024 * 1024 * 1024;

/// RFC-0012 0012-041: diff artifacts are capped at 16 MiB.
pub const PATCH_CAP_BYTES: u64 = 16 * 1024 * 1024;

/// Final line of a truncated patch artifact (RFC-0012 0012-041).
pub const PATCH_TRUNCATED_MARKER: &str = "[truncated by a2ad at 16 MiB]";

/// Workspace lifetime policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceMode {
    /// One workspace per session; tasks reuse it (RFC-0012 0012-028 default).
    PerSession,
    /// One workspace per task (RFC-0012 0012-029 opt-in).
    PerTask,
}

/// What to do when the source directory is not a git checkout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NonGitPolicy {
    /// Refuse to start (RFC-0012 0012-034 default).
    Refuse,
    /// Copy into a fresh baseline repository within the limits (RFC-0012 0012-034).
    Copy,
}

/// Limits for copy mode (RFC-0012 0012-034).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyLimits {
    /// Maximum total bytes of regular files.
    pub max_bytes: u64,
    /// Maximum number of regular files.
    pub max_files: u64,
}

impl Default for CopyLimits {
    fn default() -> Self {
        Self {
            max_bytes: 2 * GIB,
            max_files: 100_000,
        }
    }
}

/// Retention of retained (dirty or unremovable) workspaces (RFC-0012 0012-036).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    /// Retained workspaces older than this are removed.
    pub max_age: Duration,
    /// Total bytes of retained workspaces above which the oldest are removed.
    pub max_retained_bytes: u64,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            max_age: Duration::from_secs(14 * 24 * 60 * 60),
            max_retained_bytes: 20 * GIB,
        }
    }
}

/// Per-agent workspace policy plus the shared roots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceConfig {
    /// Workspace root, `<state_dir>/ws` (RFC-0012 0012-038). Must be absolute.
    pub root: PathBuf,
    /// Artifact root, `<state_dir>/artifacts`. Must be absolute.
    pub artifacts_root: PathBuf,
    /// Tenant slug; first layout component under `root`.
    pub tenant: String,
    /// Lifetime policy for this agent.
    pub mode: WorkspaceMode,
    /// Revision the worktree or baseline starts from. Default `HEAD`.
    pub base_ref: String,
    /// Behaviour for non-git sources.
    pub non_git: NonGitPolicy,
    /// Copy-mode limits.
    pub copy_limits: CopyLimits,
    /// Operator glob patterns excluded from copy mode in addition to the built-in denylist
    /// (RFC-0012 0012-074). A pattern without `/` matches a base name at any depth; a pattern
    /// with `/` matches the path relative to the source root. Matching is case-insensitive.
    pub copy_exclude: Vec<String>,
    /// Retention policy for retained workspaces.
    pub retention: Retention,
    /// Patch artifact cap in bytes. Default 16 MiB (RFC-0012 0012-041).
    pub patch_cap_bytes: u64,
}

impl WorkspaceConfig {
    /// Builds a configuration with RFC-0012 defaults: base `HEAD`, non-git refusal, 2 GiB and
    /// 100000-file copy limits, 14 days and 20 GiB retention, 16 MiB patch cap.
    pub fn new(
        root: impl Into<PathBuf>,
        artifacts_root: impl Into<PathBuf>,
        tenant: impl Into<String>,
        mode: WorkspaceMode,
    ) -> Self {
        Self {
            root: root.into(),
            artifacts_root: artifacts_root.into(),
            tenant: tenant.into(),
            mode,
            base_ref: "HEAD".to_string(),
            non_git: NonGitPolicy::Refuse,
            copy_limits: CopyLimits::default(),
            copy_exclude: Vec::new(),
            retention: Retention::default(),
            patch_cap_bytes: PATCH_CAP_BYTES,
        }
    }
}

/// Rejects base references that could be read as options or contain unsafe characters.
pub(crate) fn validate_base_ref(value: &str) -> Result<()> {
    let bad = value.is_empty()
        || value.len() > 255
        || value.starts_with('-')
        || value.contains("..")
        || value.contains("@{")
        || value.chars().any(|c| c.is_control() || c.is_whitespace());
    if bad {
        Err(config_err(format!(
            "base_ref {value:?} is not a safe revision name"
        )))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    // [0012-034] copy-mode defaults are 2 GiB and 100000 files.
    #[test]
    fn req_0012_034_default_copy_limits_are_two_gib_and_100000_files() {
        let limits = CopyLimits::default();
        assert_eq!(limits.max_bytes, 2 * GIB);
        assert_eq!(limits.max_files, 100_000);
    }

    // [0012-036] retention defaults are 14 days and 20 GiB.
    #[test]
    fn req_0012_036_default_retention_is_fourteen_days_and_twenty_gib() {
        let retention = Retention::default();
        assert_eq!(retention.max_age, Duration::from_secs(14 * 24 * 60 * 60));
        assert_eq!(retention.max_retained_bytes, 20 * GIB);
    }

    // [0012-041] default patch cap is 16 MiB.
    #[test]
    fn req_0012_041_default_patch_cap_is_sixteen_mib() {
        let config = WorkspaceConfig::new("/r", "/a", "t", WorkspaceMode::PerSession);
        assert_eq!(config.patch_cap_bytes, 16 * 1024 * 1024);
        assert_eq!(config.base_ref, "HEAD");
        assert_eq!(config.non_git, NonGitPolicy::Refuse);
    }

    #[test]
    fn base_ref_rejects_option_like_and_unsafe_names() {
        assert!(validate_base_ref("HEAD").is_ok());
        assert!(validate_base_ref("HEAD~1").is_ok());
        assert!(validate_base_ref("main").is_ok());
        assert!(validate_base_ref("--upload-pack=x").is_err());
        assert!(validate_base_ref("a b").is_err());
        assert!(validate_base_ref("a..b").is_err());
        assert!(validate_base_ref("").is_err());
    }
}
