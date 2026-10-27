//! Operator signing key: where it lives and how it is loaded ([0007-038], [0012-043]).

use std::path::{Path, PathBuf};

use a2a_security::identity::Ed25519Signer;
use a2a_security::storage::{load_key_file, operator_key_path};
use a2a_security::trust::{OPERATOR_AGENT, TrustStore};
use a2a_types::ids::AgentId;

use crate::config::{CREDS_DIR, Settings};
use crate::error::CliError;

/// Directory of the operator key files: `creds/` under the state directory, the same directory as
/// `cli.nk` (a2ad's `creds::seed_path`). The operator key is `creds/operator-<K>.key` ([0012-043]).
#[must_use]
pub fn operator_creds_dir(settings: &Settings) -> PathBuf {
    settings.state_dir.join(CREDS_DIR)
}

/// Loads the operator signing key from `creds/operator-<K>.key` for the trust entry of agent
/// `operator` ([0007-038], [0012-043]). The CLI relays as `cli`; the key never belongs to `cli`.
///
/// # Errors
///
/// Returns [`CliError::Failed`] when the trust store has no `operator` entry or no entry's key
/// file loads.
pub fn load_operator_signer(creds: &Path, trust: &TrustStore) -> Result<Ed25519Signer, CliError> {
    let agent = AgentId::parse(OPERATOR_AGENT)
        .map_err(|error| CliError::Failed(format!("operator id: {error}")))?;
    let mut first_error = None;
    for entry in trust
        .entries()
        .filter(|entry| entry.agent().as_str() == OPERATOR_AGENT)
    {
        let outcome = operator_key_path(creds, entry.kid())
            .and_then(|path| load_key_file(&path, trust, &agent, &[]));
        match outcome {
            Ok(signer) => return Ok(signer),
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
    }
    Err(CliError::Failed(match first_error {
        Some(error) => format!("cannot load the operator signing key: {error}"),
        None => "cannot load the operator signing key: trust file has no operator entry".to_owned(),
    }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::config::{Overrides, parse_file, resolve};
    use a2a_security::identity::{Signer, encode_public_key};
    use a2a_security::storage::{generate_signer, key_path};

    /// Req [0012-043]: the approval signer is the `operator` key under `creds/`, even when the CLI
    /// relays as `cli` and no `cli` trust entry exists.
    #[test]
    fn req_0012_043_operator_signer_loads_from_creds_for_operator_agent() {
        let root = std::env::temp_dir().join(format!("a2a-cli-keys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let config = root.join("config");
        let creds = root.join("creds");
        std::fs::create_dir_all(&creds).expect("creds dir");
        let operator = AgentId::parse(OPERATOR_AGENT).expect("id");
        let signer = generate_signer(&config, &operator, &[]).expect("generated");
        let trust_text = format!(
            "schema = \"a2abus.trust/1\"\n[[key]]\nkid = \"{}\"\nagent = \"operator\"\nrole = \"operator\"\npublic_key = \"{}\"\n",
            signer.kid(),
            encode_public_key(&signer.public_key()),
        );
        let trust = TrustStore::parse(&trust_text).expect("trust loads");
        std::fs::rename(
            key_path(&config, signer.kid()),
            operator_key_path(&creds, signer.kid()).expect("operator kid"),
        )
        .expect("key moved into creds");
        let loaded = load_operator_signer(&creds, &trust).expect("loads");
        assert_eq!(loaded.kid(), signer.kid());

        let empty = TrustStore::parse("schema = \"a2abus.trust/1\"\n").expect("empty trust");
        let err = load_operator_signer(&creds, &empty).expect_err("no operator entry");
        assert!(err.to_string().contains("no operator entry"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Req [0012-043]: the operator key directory is `creds/` under the state directory, the same
    /// directory as `cli.nk`.
    #[test]
    fn req_0012_043_operator_creds_dir_is_state_creds() {
        let file = parse_file("[paths]\nstate_dir = \"/state\"\n").expect("valid config");
        let no_env = |_: &str| -> Option<String> { None };
        let settings = resolve(
            &Overrides::default(),
            &no_env,
            &file,
            Path::new("/defaults"),
        )
        .expect("resolves");
        assert_eq!(
            operator_creds_dir(&settings),
            PathBuf::from("/state").join("creds")
        );
        assert_eq!(
            settings.seed_file.parent(),
            Some(operator_creds_dir(&settings).as_path())
        );
    }
}
