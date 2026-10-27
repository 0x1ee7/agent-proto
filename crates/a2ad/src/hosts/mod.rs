//! Helpers shared by the in-process worker and gateway hosts.
//!
//! Secrets are never stored in configuration. A reference is either `env:<NAME>` (the value
//! is read from the environment at start) or `file:<path>` (the file must be owner-only,
//! RFC-0008 0008-066). Error messages name the reference kind, never its value.

pub mod logging;

use std::fs;
use std::path::Path;
use std::time::Duration;

use a2a_types::ids::TenantId;
use bus_nats::NatsBus;
use bus_nats::config::{NatsConfig, Seed};

use crate::host::HostError;

/// Client-side deadline for connection establishment and bus API calls.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Builds the single-threaded runtime that drives a host's async work.
pub(crate) fn runtime() -> Result<tokio::runtime::Runtime, HostError> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}

/// Refuses a secret file that is not owner-only (RFC-0008 0008-066, RFC-0012 0012-023).
pub(crate) fn check_owner_only(path: &Path) -> Result<(), HostError> {
    crate::platform::verify_owner_only(path, false)
        .map_err(|err| HostError::Config(err.to_string()))
}

/// Resolves a secret reference. `lookup` supplies environment values (injected for tests).
pub(crate) fn resolve_secret(
    reference: &str,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<String, HostError> {
    if let Some(name) = reference.strip_prefix("env:") {
        let valid = !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !name.starts_with(|c: char| c.is_ascii_digit());
        if !valid {
            return Err(HostError::Config(
                "env secret reference names an invalid variable".to_owned(),
            ));
        }
        return match lookup(name) {
            Some(value) if !value.trim().is_empty() => Ok(value.trim().to_owned()),
            _ => Err(HostError::Config(format!(
                "environment variable {name} named by a secret reference is not set"
            ))),
        };
    }
    if let Some(path) = reference.strip_prefix("file:") {
        let path = Path::new(path);
        check_owner_only(path)?;
        let text = fs::read_to_string(path)
            .map_err(|err| HostError::Config(format!("cannot read secret file: {}", err.kind())))?;
        let value = text.trim();
        if value.is_empty() {
            return Err(HostError::Config("secret file is empty".to_owned()));
        }
        return Ok(value.to_owned());
    }
    Err(HostError::Config(
        "secret must be an env:<NAME> or file:<path> reference".to_owned(),
    ))
}

/// Reads an NKey user seed from its creds file. The file must be owner-only (RFC-0012 0012-023).
pub(crate) fn read_seed(path: &Path) -> Result<Seed, HostError> {
    check_owner_only(path)?;
    let text = fs::read_to_string(path)
        .map_err(|err| HostError::Config(format!("cannot read creds file: {}", err.kind())))?;
    let value = text.trim();
    if value.is_empty() {
        return Err(HostError::Config("creds file is empty".to_owned()));
    }
    Ok(Seed::new(value))
}

/// Connects to the broker under `identity` (RFC-0010 0010-011).
pub(crate) async fn connect_bus(
    url: String,
    identity: &str,
    seed: Seed,
    tenant: TenantId,
) -> Result<NatsBus, HostError> {
    let config = NatsConfig {
        url,
        identity: identity.to_owned(),
        seed,
        tenant,
        request_timeout: REQUEST_TIMEOUT,
        ack_wait: bus_nats::layout::ACK_WAIT,
    };
    bus_nats::connect(config)
        .await
        .map_err(|err| HostError::Bus(err.to_string()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn none(_: &str) -> Option<String> {
        None
    }

    // 0008-066: `env:` references resolve through the injected lookup.
    #[test]
    fn req_0008_066_env_reference_resolves() {
        let value = resolve_secret("env:A2AB_TEST_TOKEN", |name| {
            (name == "A2AB_TEST_TOKEN").then(|| " abc123 ".to_owned())
        })
        .unwrap();
        assert_eq!(value, "abc123");
    }

    #[test]
    fn req_0008_066_missing_env_reference_is_refused() {
        let err = resolve_secret("env:A2AB_TEST_TOKEN", none).unwrap_err();
        assert!(err.to_string().contains("A2AB_TEST_TOKEN"));
    }

    #[test]
    fn req_0008_066_empty_env_value_is_refused() {
        let err = resolve_secret("env:A2AB_TEST_TOKEN", |_| Some("  ".to_owned())).unwrap_err();
        assert!(matches!(err, HostError::Config(_)));
    }

    #[test]
    fn req_0008_066_literal_secret_is_refused() {
        let err = resolve_secret("literal-token-value", none).unwrap_err();
        assert!(matches!(err, HostError::Config(_)));
        assert!(!err.to_string().contains("literal-token-value"));
    }

    #[test]
    fn req_0008_066_invalid_env_name_is_refused() {
        assert!(resolve_secret("env:1BAD", none).is_err());
        assert!(resolve_secret("env:", none).is_err());
        assert!(resolve_secret("env:BAD-NAME", none).is_err());
    }

    #[test]
    fn req_0008_066_file_reference_reads_trimmed_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        fs::write(&path, "  secret-value \n").unwrap();
        restrict_to_owner(&path);
        let reference = format!("file:{}", path.display());
        assert_eq!(resolve_secret(&reference, none).unwrap(), "secret-value");
    }

    #[cfg(unix)]
    #[test]
    fn req_0008_066_wide_secret_file_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        fs::write(&path, "secret-value").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let reference = format!("file:{}", path.display());
        let err = resolve_secret(&reference, none).unwrap_err();
        assert!(err.to_string().contains("owner-only"));
        assert!(!err.to_string().contains("secret-value"));
    }

    #[cfg(unix)]
    #[test]
    fn req_0012_023_owner_only_creds_are_accepted() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("omp.nk");
        fs::write(&path, "SUAFAKE").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(read_seed(&path).is_ok());
    }

    /// Makes a test secret owner-only on every platform the tests run on.
    fn restrict_to_owner(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        #[cfg(not(unix))]
        {
            // Drop inherited ACEs (the temp dir grants SYSTEM and Administrators) and grant
            // only the current user, mirroring what an operator sets for a credential file.
            let user = std::env::var("USERNAME").unwrap();
            let status = std::process::Command::new("icacls")
                .arg(path)
                .args(["/inheritance:r", "/grant:r"])
                .arg(format!("{user}:F"))
                .status()
                .unwrap();
            assert!(
                status.success(),
                "icacls could not restrict the test secret"
            );
        }
    }
}
