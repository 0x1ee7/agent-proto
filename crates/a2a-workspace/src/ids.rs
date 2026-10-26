//! Agent, session/task and reference identifiers with strict validation.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{Result, config_err};

const SLUG_MAX: usize = 48;
const TASK_ID_MAX: usize = 64;
const UUID_LEN: usize = 36;
const UUID_HYPHENS: [usize; 4] = [8, 13, 18, 23];

/// Validates a lowercase slug (`[a-z0-9][a-z0-9-]{0,47}`) used as a path component.
pub(crate) fn validate_slug(value: &str, what: &str) -> Result<()> {
    let bytes = value.as_bytes();
    let first_ok = matches!(bytes.first(), Some(b) if b.is_ascii_lowercase() || b.is_ascii_digit());
    let rest_ok = bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-');
    if first_ok && rest_ok && bytes.len() <= SLUG_MAX {
        Ok(())
    } else {
        Err(config_err(format!(
            "{what} must match [a-z0-9][a-z0-9-]{{0,47}}, got {value:?}"
        )))
    }
}

/// Validates a task identifier used as an artifact directory name.
pub(crate) fn validate_task_id(value: &str) -> Result<()> {
    let ok = !value.is_empty()
        && value.len() <= TASK_ID_MAX
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(config_err(format!(
            "task id must match [A-Za-z0-9_-]{{1,64}}, got {value:?}"
        )))
    }
}

/// Agent identifier (RFC-0001 slug rules).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AgentId(String);

impl AgentId {
    /// Parses and validates an agent identifier.
    pub fn parse(value: &str) -> Result<Self> {
        validate_slug(value, "agent id")?;
        Ok(Self(value.to_string()))
    }

    /// Identifier text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Session or task UUID. The workspace short id is its first 12 lowercase hex digits.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkKey {
    canonical: String,
    short: String,
}

impl WorkKey {
    /// Parses a canonical 8-4-4-4-12 UUID (any version). Input is lowercased.
    pub fn parse(value: &str) -> Result<Self> {
        let lower = value.to_ascii_lowercase();
        let bytes = lower.as_bytes();
        let shape_ok = bytes.len() == UUID_LEN
            && bytes.iter().enumerate().all(|(i, b)| {
                if UUID_HYPHENS.contains(&i) {
                    *b == b'-'
                } else {
                    b.is_ascii_hexdigit()
                }
            });
        if !shape_ok {
            return Err(config_err(format!(
                "session or task id must be a canonical UUID, got {value:?}"
            )));
        }
        let hex: String = lower.chars().filter(|c| *c != '-').collect();
        let short = hex[..12].to_string();
        Ok(Self {
            canonical: lower,
            short,
        })
    }

    /// Canonical lowercase UUID text.
    pub fn canonical(&self) -> &str {
        &self.canonical
    }

    /// First 12 hex digits of the UUID; names the workspace directory and branch.
    pub fn short(&self) -> &str {
        &self.short
    }
}

/// Lifetime scope of a workspace lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScopeKind {
    /// Per-session workspace (RFC-0012 0012-028 default).
    Session,
    /// Per-task workspace (RFC-0012 0012-029 opt-in).
    Task,
}

/// Scope plus the session or task key that names the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkScope {
    /// Session key.
    Session(WorkKey),
    /// Task key.
    Task(WorkKey),
}

impl WorkScope {
    /// Scope kind.
    pub fn kind(&self) -> ScopeKind {
        match self {
            Self::Session(_) => ScopeKind::Session,
            Self::Task(_) => ScopeKind::Task,
        }
    }

    /// Underlying key.
    pub fn key(&self) -> &WorkKey {
        match self {
            Self::Session(key) | Self::Task(key) => key,
        }
    }
}

/// Identifies a workspace by agent and key, for example a non-terminal task that must keep its
/// workspace during retention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRef {
    /// Owning agent.
    pub agent: AgentId,
    /// Session or task key.
    pub key: WorkKey,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    // [0012-038] the workspace directory component is the first 12 hex digits of the UUID.
    #[test]
    fn short_id_is_first_twelve_hex_digits_of_uuid() {
        let key = WorkKey::parse("0190F3A2-7C1D-7E4A-9B2F-5D6E7F8A9B0C").unwrap();
        assert_eq!(key.short(), "0190f3a27c1d");
        assert_eq!(key.canonical(), "0190f3a2-7c1d-7e4a-9b2f-5d6e7f8a9b0c");
    }

    #[test]
    fn work_key_rejects_non_uuid_shapes() {
        assert!(WorkKey::parse("0190f3a27c1d7e4a9b2f5d6e7f8a9b0c").is_err());
        assert!(WorkKey::parse("0190f3a2-7c1d-7e4a-9b2f-5d6e7f8a9b0g").is_err());
        assert!(WorkKey::parse("../../../../etc/passwd-xxxxxxxxx").is_err());
    }

    #[test]
    fn agent_id_follows_slug_rules() {
        assert!(AgentId::parse("omp-1").is_ok());
        assert!(AgentId::parse("-omp").is_err());
        assert!(AgentId::parse("Omp").is_err());
        assert!(AgentId::parse("a/b").is_err());
        assert!(AgentId::parse(&"a".repeat(49)).is_err());
        assert!(AgentId::parse(&"a".repeat(48)).is_ok());
    }

    #[test]
    fn task_id_rejects_path_separators() {
        assert!(validate_task_id("task_01-A").is_ok());
        assert!(validate_task_id("../x").is_err());
        assert!(validate_task_id("").is_err());
    }
}
