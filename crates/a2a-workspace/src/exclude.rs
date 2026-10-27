//! Copy-mode exclusion rules (RFC-0012 0012-074): the built-in secret denylist plus the
//! operator's `copy_exclude` globs. Matching is case-insensitive on every platform so Windows and
//! Linux copy the same set.

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use crate::error::{Result, config_err};

/// Built-in denylist of file base names (RFC-0012 0012-074).
const DENY_FILES: [&str; 13] = [
    ".env",
    ".env.*",
    "*.pem",
    "*.key",
    "*.p12",
    "*.pfx",
    "id_rsa*",
    "id_ed25519*",
    "credentials*",
    "*.kdbx",
    ".npmrc",
    ".netrc",
    ".git-credentials",
];

/// Built-in denylist of directory names, excluded at any depth with all their content.
const DENY_DIRS: [&str; 3] = [".aws", ".ssh", ".gnupg"];

/// Compiled exclusion rules for one workspace configuration.
#[derive(Debug)]
pub(crate) struct Excludes {
    /// Patterns without `/`, matched against base names.
    names: GlobSet,
    /// Patterns with `/`, matched against the path relative to the source root.
    paths: GlobSet,
}

impl Excludes {
    /// Compiles the denylist and the operator's patterns. A pattern must be non-empty and
    /// relative; an invalid pattern is a configuration error that names no file.
    pub(crate) fn new(copy_exclude: &[String]) -> Result<Self> {
        let mut names = GlobSetBuilder::new();
        let mut paths = GlobSetBuilder::new();
        for pattern in DENY_FILES {
            names.add(glob(pattern)?);
        }
        for pattern in copy_exclude {
            if pattern.is_empty() || pattern.starts_with('/') {
                return Err(config_err(
                    "copy_exclude patterns must be non-empty relative globs",
                ));
            }
            let compiled = glob(pattern)
                .map_err(|e| config_err(format!("invalid copy_exclude pattern: {e}")))?;
            if pattern.contains('/') {
                paths.add(compiled);
            } else {
                names.add(compiled);
            }
        }
        Ok(Self {
            names: build(names)?,
            paths: build(paths)?,
        })
    }

    /// Whether the file at `rel` (a `/`-separated path relative to the source root) is excluded.
    pub(crate) fn file_excluded(&self, rel: &str) -> bool {
        self.names.is_match(base_name(rel)) || self.paths.is_match(rel)
    }

    /// Whether the directory at `rel` is excluded together with everything below it.
    pub(crate) fn dir_excluded(&self, rel: &str) -> bool {
        let base = base_name(rel);
        DENY_DIRS.iter().any(|dir| dir.eq_ignore_ascii_case(base)) || self.file_excluded(rel)
    }
}

fn glob(pattern: &str) -> Result<globset::Glob> {
    GlobBuilder::new(pattern)
        .case_insensitive(true)
        .literal_separator(true)
        .build()
        .map_err(|e| config_err(format!("invalid glob {pattern:?}: {e}")))
}

fn build(set: GlobSetBuilder) -> Result<GlobSet> {
    set.build()
        .map_err(|e| config_err(format!("cannot compile copy_exclude: {e}")))
}

/// Last component of a `/`-separated relative path.
pub(crate) fn base_name(rel: &str) -> &str {
    rel.rsplit('/').next().unwrap_or(rel)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn req_0012_074_denylist_matches_base_names_case_insensitively() {
        let rules = Excludes::new(&[]).unwrap();
        assert!(rules.file_excluded(".env"));
        assert!(rules.file_excluded("deep/dir/.env.production"));
        assert!(rules.file_excluded("keys/SERVER.PEM"));
        assert!(rules.file_excluded("id_ed25519_work"));
        assert!(!rules.file_excluded("src/env.rs"));
        assert!(!rules.file_excluded("environment.txt"));
    }

    #[test]
    fn req_0012_074_secret_directories_match_at_any_depth() {
        let rules = Excludes::new(&[]).unwrap();
        assert!(rules.dir_excluded(".aws"));
        assert!(rules.dir_excluded("home/.ssh"));
        assert!(rules.dir_excluded("x/.GnuPG"));
        assert!(!rules.dir_excluded("src"));
        assert!(!rules.file_excluded("home/.ssh"));
    }

    #[test]
    fn req_0012_074_copy_exclude_name_and_path_patterns() {
        let rules = Excludes::new(&["notes.txt".into(), "docs/private/*.md".into()]).unwrap();
        assert!(rules.file_excluded("a/b/notes.txt"));
        assert!(rules.file_excluded("docs/private/plan.md"));
        assert!(!rules.file_excluded("other/docs/private/plan.md"));
        assert!(!rules.file_excluded("docs/private/nested/plan.md"));
    }

    #[test]
    fn req_0012_074_invalid_copy_exclude_is_config_error() {
        assert!(Excludes::new(&["/abs".into()]).is_err());
        assert!(Excludes::new(&[String::new()]).is_err());
        assert!(Excludes::new(&["[unclosed".into()]).is_err());
    }
}
