//! Operator identity for signed approval decisions (RFC-0012 sec 4.10, RFC-0007 0007-006).
//!
//! Two identities meet here. The console connects to the bus and sends as `tui`, the only
//! operator principal `bus-nats` admits (0010-063). The signing key is the operator key of
//! RFC-0006, stored at `<state>/creds/operator-<K>.key` (RFC-0012 0012-043) and checked by
//! the security crate against the operator-role trust entries. Without a usable key the Approvals
//! screen is read-only (0012-043); the reason is kept so the operator can see why.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use a2a_security::identity::Signer;
use a2a_security::storage::{load_key_file, operator_key_path};
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

/// Loads the operator key from the credentials directory `creds` (`<state>/creds`), checked
/// against the trust store at `trust`.
///
/// Every operator-role trust entry is tried in turn, so a rotation overlap with two kids keeps
/// working. `roots` lists the workspace roots that must not contain the key file (see
/// [`load_key_file`]). Any rejection yields [`Identity::ReadOnly`].
#[must_use]
pub fn load(creds: &Path, trust: &Path, roots: &[PathBuf]) -> Identity {
    match try_load(creds, trust, roots) {
        Ok(signer) => Identity::Signed(signer),
        Err(reason) => Identity::ReadOnly(reason),
    }
}

/// Resolves the state directory (RFC-0012 4.2): `explicit` (`--state-dir`) wins, then
/// `A2AB_STATE_DIR`, then the platform default: `%LOCALAPPDATA%\a2abus` on Windows, otherwise
/// `$XDG_STATE_HOME/a2abus` (absolute values only) or `~/.local/state/a2abus`.
///
/// # Errors
/// Returns the reason when no source yields a directory.
pub fn resolve_state_dir(
    explicit: Option<PathBuf>,
    env: impl Fn(&str) -> Option<String>,
    windows: bool,
) -> Result<PathBuf, String> {
    if let Some(dir) = explicit {
        return Ok(dir);
    }
    if let Some(dir) = env("A2AB_STATE_DIR").filter(|dir| !dir.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    if windows {
        return env("LOCALAPPDATA")
            .map(|base| PathBuf::from(base).join("a2abus"))
            .ok_or_else(|| "LOCALAPPDATA is not set; pass --state-dir".to_owned());
    }
    if let Some(base) = env("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|base| base.is_absolute())
    {
        return Ok(base.join("a2abus"));
    }
    env("HOME")
        .map(|home| {
            PathBuf::from(home)
                .join(".local")
                .join("state")
                .join("a2abus")
        })
        .ok_or_else(|| "HOME is not set; pass --state-dir".to_owned())
}

fn try_load(creds: &Path, trust: &Path, roots: &[PathBuf]) -> Result<Arc<dyn Signer>, String> {
    let store = TrustStore::load(trust).map_err(|e| format!("trust store: {e}"))?;
    signer_from_store(creds, &store, roots)
}

/// Tries every operator-role entry of `store` for a key file under `creds`.
fn signer_from_store(
    creds: &Path,
    store: &TrustStore,
    roots: &[PathBuf],
) -> Result<Arc<dyn Signer>, String> {
    let agent = signing_agent()?;
    let mut first_error = None;
    for entry in store
        .entries()
        .filter(|entry| entry.agent().as_str() == OPERATOR_AGENT)
    {
        let outcome = operator_key_path(creds, entry.kid())
            .and_then(|path| load_key_file(&path, store, &agent, roots));
        match outcome {
            Ok(signer) => return Ok(Arc::new(signer)),
            Err(e) => {
                first_error.get_or_insert(e);
            }
        }
    }
    match first_error {
        Some(e) => Err(format!("operator key: {e}")),
        None => Err("trust store: no operator entry".to_owned()),
    }
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
    fn req_0012_043_unreadable_trust_store_is_read_only() {
        let dir = std::env::temp_dir().join("a2a-tui-operator-missing-trust");
        let identity = load(&dir, &dir.join("no-such-trust.toml"), &[]);
        assert!(
            matches!(&identity, Identity::ReadOnly(reason) if reason.starts_with("trust store:"))
        );
    }

    #[test]
    fn req_0012_043_trust_without_operator_entry_is_read_only() {
        let store = TrustStore::parse("schema = \"a2abus.trust/1\"\n").expect("empty trust parses");
        assert!(matches!(
            signer_from_store(Path::new("creds"), &store, &[]),
            Err(reason) if reason.contains("no operator entry")
        ));
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

    #[test]
    fn req_0012_043_state_dir_flag_beats_environment() {
        let env = |key: &str| (key == "A2AB_STATE_DIR").then(|| "/env".to_owned());
        assert_eq!(
            resolve_state_dir(Some(PathBuf::from("/flag")), env, false),
            Ok(PathBuf::from("/flag"))
        );
    }

    #[test]
    fn req_0012_043_state_dir_environment_beats_platform_default() {
        let env = |key: &str| match key {
            "A2AB_STATE_DIR" => Some("/env".to_owned()),
            "HOME" => Some("/home/u".to_owned()),
            _ => None,
        };
        assert_eq!(
            resolve_state_dir(None, env, false),
            Ok(PathBuf::from("/env"))
        );
    }

    #[test]
    fn req_0012_043_state_dir_platform_defaults() {
        let windows =
            |key: &str| (key == "LOCALAPPDATA").then(|| "C:/Users/u/AppData/Local".to_owned());
        assert_eq!(
            resolve_state_dir(None, windows, true),
            Ok(PathBuf::from("C:/Users/u/AppData/Local").join("a2abus"))
        );
        let linux = |key: &str| match key {
            "XDG_STATE_HOME" => Some("relative".to_owned()),
            "HOME" => Some("/home/u".to_owned()),
            _ => None,
        };
        assert_eq!(
            resolve_state_dir(None, linux, false),
            Ok(PathBuf::from("/home/u/.local/state/a2abus"))
        );
    }

    #[test]
    fn req_0012_043_state_dir_unresolvable_is_an_error() {
        assert!(resolve_state_dir(None, |_| None, true).is_err());
        assert!(resolve_state_dir(None, |_| None, false).is_err());
    }
}
