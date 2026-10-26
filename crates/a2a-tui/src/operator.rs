//! Operator identity for signed approval decisions (RFC-0012 sec 4.10, RFC-0007 0007-006).
//!
//! Two identities meet here. The console connects to the bus and sends as `tui`, the only
//! operator principal `bus-nats` admits (0010-063). The signing key is loaded by the security
//! crate for the `operator` agent, the decision origin of RFC-0006. Without a usable key the
//! Approvals screen is read-only (0012-043); the reason is kept so the operator can see why.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use a2a_security::identity::Signer;
use a2a_security::storage::load_key_file;
use a2a_security::trust::{OPERATOR_AGENT, TrustStore};
use a2a_types::ids::AgentId;

/// What the console may do with the operator identity.
pub enum Identity {
    /// Decisions are signed with this key.
    Signed(Arc<dyn Signer>),
    /// No usable key: decisions are disabled. The reason is shown to the operator.
    ReadOnly(String),
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Signed(_) => f.write_str("Identity::Signed"),
            Self::ReadOnly(reason) => f.debug_tuple("Identity::ReadOnly").field(reason).finish(),
        }
    }
}

/// Loads the operator key at `key`, checked against the trust store at `trust`.
///
/// `roots` lists the workspace roots that must not contain the key file (see
/// [`load_key_file`]). A missing key path or any rejection yields [`Identity::ReadOnly`].
#[must_use]
pub fn load(key: Option<&Path>, trust: &Path, roots: &[PathBuf]) -> Identity {
    let Some(key) = key else {
        return Identity::ReadOnly("no operator key given (--key)".to_owned());
    };
    match try_load(key, trust, roots) {
        Ok(signer) => Identity::Signed(signer),
        Err(reason) => Identity::ReadOnly(reason),
    }
}

fn try_load(key: &Path, trust: &Path, roots: &[PathBuf]) -> Result<Arc<dyn Signer>, String> {
    let store = TrustStore::load(trust).map_err(|e| format!("trust store: {e}"))?;
    let agent = signing_agent()?;
    let signer =
        load_key_file(key, &store, &agent, roots).map_err(|e| format!("operator key: {e}"))?;
    Ok(Arc::new(signer))
}

/// The agent the operator key belongs to (`operator`, RFC-0006 decision origin).
fn signing_agent() -> Result<AgentId, String> {
    AgentId::parse(OPERATOR_AGENT).map_err(|e| format!("operator id: {e}"))
}

/// The bus principal the console connects and sends as. `bus-nats` admits operator
/// connections only as `tui` or `cli` (0010-063), so this is the console identity.
#[must_use]
pub fn console_agent() -> AgentId {
    AgentId::tui()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn req_0012_043_missing_key_path_is_read_only() {
        let identity = load(None, Path::new("trust.toml"), &[]);
        assert!(matches!(&identity, Identity::ReadOnly(reason) if reason.contains("--key")));
    }

    #[test]
    fn req_0012_043_unreadable_trust_store_is_read_only() {
        let dir = std::env::temp_dir().join("a2a-tui-operator-missing-trust");
        let identity = load(
            Some(&dir.join("operator.key")),
            &dir.join("no-such-trust.toml"),
            &[],
        );
        assert!(
            matches!(&identity, Identity::ReadOnly(reason) if reason.starts_with("trust store:"))
        );
    }

    #[test]
    fn req_0012_043_signing_agent_is_operator() {
        let agent = signing_agent().expect("operator constant is valid");
        assert_eq!(agent.as_str(), OPERATOR_AGENT);
    }

    #[test]
    fn req_0010_063_console_connects_as_tui() {
        assert_eq!(console_agent().as_str(), "tui");
    }
}
