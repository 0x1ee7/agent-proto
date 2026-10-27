//! State layout, NKey identity files and delegation files (RFC-0012 sec 4.4 and 4.8,
//! RFC-0015 [0015-080..082]).
//!
//! Identity seeds live in `<state>/creds/<identity>.nk`, owner-only (0012-023..025). The
//! supervisor generates a seed when it is missing and verifies the file before every start.
//! Seeds are never printed, logged, or placed on a command line.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use a2a_security::identity::Kid;
use a2a_security::storage;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Serialize;

use crate::platform::{self, PlatformError};

/// Directory of identity seed files under the state directory.
pub const CREDS_DIR: &str = "creds";
/// Directory of per-agent run records and delegation files.
pub const RUN_DIR: &str = "run";
/// Directory of per-agent log files.
pub const LOGS_DIR: &str = "logs";
/// JetStream store directory of the broker, relative to the state directory.
pub const NATS_STORE_DIR: &str = "nats/jetstream";
/// Delegation file name inside `run/<agent>/` (RFC-0015 [0015-082]).
pub const DELEGATION_FILE: &str = "delegation.json";
/// Grant id length in bytes (RFC-0015 [0015-081]).
pub const GRANT_ID_BYTES: usize = 16;
/// Grant credential length in bytes (RFC-0015 [0015-081]).
pub const GRANT_CREDENTIAL_BYTES: usize = 32;

/// Credential and layout errors. Messages name the path and rule, never a secret value.
#[derive(Debug, thiserror::Error)]
pub enum CredsError {
    /// Permission check or permission change failed.
    #[error(transparent)]
    Platform(#[from] PlatformError),
    /// File system error.
    #[error("{0}")]
    Io(#[from] io::Error),
    /// The seed file is not an NKey user seed.
    #[error("credential {identity} is not a valid NKey user seed (credential_invalid)")]
    BadSeed {
        /// Identity whose seed file is malformed.
        identity: String,
    },
    /// The operating system random source failed.
    #[error("random source failed: {0}")]
    Random(String),
    /// A delegation file could not be encoded.
    #[error("delegation file encoding failed: {0}")]
    Encode(String),
    /// Operator key or trust entry is missing or unusable (RFC-0007 [0007-006], RFC-0012 [0012-043]).
    #[error("operator key: {0}")]
    Operator(String),
}

/// Path of the NKey seed file of `identity` (0012-023): `<state>/creds/<identity>.nk`.
#[must_use]
pub fn seed_path(state_dir: &Path, id: &str) -> PathBuf {
    state_dir.join(CREDS_DIR).join(format!("{id}.nk"))
}

/// Creates the operator key `<state>/creds/operator-<K>.key` and its allowlist entry on first
/// start, and changes nothing once both exist ([0007-006], [0012-043]). `roots` are the
/// workspace roots the key must stay out of ([0007-014]).
///
/// # Errors
/// Returns [`CredsError::Operator`] when the trust file is invalid, the key file is missing for an
/// existing operator entry, or the key cannot be written with owner-only permissions.
pub fn ensure_operator(
    state_dir: &Path,
    trust_file: &Path,
    roots: &[PathBuf],
) -> Result<Kid, CredsError> {
    storage::ensure_operator_key(trust_file, &state_dir.join(CREDS_DIR), roots)
        .map_err(|err| CredsError::Operator(err.to_string()))
}

/// Creates the state directory tree with owner-only directories (0012-006). Existing
/// directories are restricted again, so a loosened tree is repaired on start.
///
/// # Errors
/// Returns [`CredsError`] when a directory cannot be created or restricted.
pub fn ensure_layout(state_dir: &Path) -> Result<(), CredsError> {
    let dirs = [
        state_dir.to_path_buf(),
        state_dir.join(CREDS_DIR),
        state_dir.join(RUN_DIR),
        state_dir.join(LOGS_DIR),
        state_dir.join(NATS_STORE_DIR),
    ];
    for dir in &dirs {
        fs::create_dir_all(dir)?;
        platform::restrict_to_owner(dir, true)?;
    }
    Ok(())
}

/// Returns the public key of `identity`, creating its seed file when missing (0012-023). An
/// existing file is verified first: owner-only permissions and a valid user seed.
///
/// # Errors
/// Returns [`CredsError`] when verification or creation fails.
pub fn ensure_identity(state_dir: &Path, identity: &str) -> Result<String, CredsError> {
    let path = seed_path(state_dir, identity);
    if path.exists() {
        return public_key_of(&path, identity);
    }
    let seed = generate_seed()?;
    let kp = nkeys::KeyPair::from_seed(&seed).map_err(|_| CredsError::BadSeed {
        identity: identity.to_owned(),
    })?;
    let public = kp.public_key();
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    platform::restrict_to_owner(&path, false)?;
    file.write_all(seed.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(public)
}

/// Reads and verifies the seed of `identity`, returning the seed text for connection use.
/// The caller owns its secrecy and must not log it.
///
/// # Errors
/// Returns [`CredsError`] when the file is missing, broader than owner-only, or malformed.
pub fn load_seed(state_dir: &Path, identity: &str) -> Result<String, CredsError> {
    let path = seed_path(state_dir, identity);
    verify_seed_file(&path, identity)
}

fn public_key_of(path: &Path, identity: &str) -> Result<String, CredsError> {
    let seed = verify_seed_file(path, identity)?;
    let kp = nkeys::KeyPair::from_seed(&seed).map_err(|_| CredsError::BadSeed {
        identity: identity.to_owned(),
    })?;
    Ok(kp.public_key())
}

fn verify_seed_file(path: &Path, identity: &str) -> Result<String, CredsError> {
    platform::verify_owner_only(path, false)?;
    let text = fs::read_to_string(path)?;
    let seed = text.trim();
    if !seed.starts_with("SU") {
        return Err(CredsError::BadSeed {
            identity: identity.to_owned(),
        });
    }
    nkeys::KeyPair::from_seed(seed).map_err(|_| CredsError::BadSeed {
        identity: identity.to_owned(),
    })?;
    Ok(seed.to_owned())
}

fn generate_seed() -> Result<String, CredsError> {
    let kp = nkeys::KeyPair::new_user();
    kp.seed().map_err(|err| CredsError::Random(err.to_string()))
}

/// Fills a fixed-size array from the operating system random source (RFC-0007 [0007-116]).
///
/// # Errors
/// Returns [`CredsError::Random`] when the source fails.
pub fn random_bytes<const N: usize>() -> Result<[u8; N], CredsError> {
    let mut buf = [0u8; N];
    getrandom::fill(&mut buf).map_err(|err| CredsError::Random(err.to_string()))?;
    Ok(buf)
}

/// A delegation grant for one worker run (RFC-0015 [0015-081]). Both values are secret
/// except the id, which is still never placed on a command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// 16-byte grant id.
    pub id: [u8; GRANT_ID_BYTES],
    /// 32-byte grant credential.
    pub credential: [u8; GRANT_CREDENTIAL_BYTES],
}

impl Grant {
    /// Generates a new grant from the operating system random source.
    ///
    /// # Errors
    /// Returns [`CredsError::Random`] when the source fails.
    pub fn generate() -> Result<Self, CredsError> {
        Ok(Self {
            id: random_bytes::<GRANT_ID_BYTES>()?,
            credential: random_bytes::<GRANT_CREDENTIAL_BYTES>()?,
        })
    }
}

/// Contents of `run/<agent>/delegation.json` (RFC-0015 [0015-082]). The grant credential is
/// base64url text; the file is owner-only and deleted when the worker stops.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DelegationFile {
    /// Base64url grant id.
    pub grant_id: String,
    /// Base64url grant credential.
    pub grant_credential: String,
    /// Delegation endpoint path (socket or pipe name).
    pub endpoint: String,
    /// Absolute path of the MCP shim.
    pub shim_path: String,
    /// Peer allowlist (agent ids).
    pub peers: Vec<String>,
    /// Concurrent calls allowed.
    pub max_inflight: u8,
    /// Sends allowed per minute.
    pub max_sends_per_min: u16,
    /// Calls allowed per minute.
    pub max_calls_per_min: u16,
}

impl DelegationFile {
    /// Builds the file contents for `grant`.
    #[must_use]
    pub fn new(
        grant: &Grant,
        endpoint: String,
        shim_path: &Path,
        peers: Vec<String>,
        limits: (u8, u16, u16),
    ) -> Self {
        Self {
            grant_id: URL_SAFE_NO_PAD.encode(grant.id),
            grant_credential: URL_SAFE_NO_PAD.encode(grant.credential),
            endpoint,
            shim_path: shim_path.to_string_lossy().into_owned(),
            peers,
            max_inflight: limits.0,
            max_sends_per_min: limits.1,
            max_calls_per_min: limits.2,
        }
    }
}

/// Writes the delegation file owner-only (RFC-0015 [0015-082]). Permissions are set before
/// any content is written.
///
/// # Errors
/// Returns [`CredsError`] when the file cannot be created, restricted, or written.
pub fn write_delegation(path: &Path, file: &DelegationFile) -> Result<(), CredsError> {
    let body = serde_json::to_string(file).map_err(|err| CredsError::Encode(err.to_string()))?;
    if path.exists() {
        fs::remove_file(path)?;
    }
    let mut handle = OpenOptions::new().write(true).create_new(true).open(path)?;
    platform::restrict_to_owner(path, false)?;
    handle.write_all(body.as_bytes())?;
    handle.sync_all()?;
    Ok(())
}

/// Removes the delegation file; a file already gone is not an error (RFC-0015 [0015-088]).
///
/// # Errors
/// Returns [`io::Error`] for failures other than a missing file.
pub fn remove_delegation(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use a2a_security::identity::Signer;

    #[test]
    fn stable_0012_023_seed_path_is_under_creds() {
        let state = Path::new("state");
        let path = seed_path(state, "alpha");
        assert_eq!(path, state.join("creds").join("alpha.nk"));
    }

    #[test]
    fn stable_0012_023_identity_created_once_and_reused() {
        let dir = tempfile::tempdir().expect("tempdir");
        ensure_layout(dir.path()).expect("layout");
        let first = ensure_identity(dir.path(), "alpha").expect("create");
        let second = ensure_identity(dir.path(), "alpha").expect("reuse");
        assert_eq!(first, second);
        assert!(first.starts_with('U'));
        let seed = load_seed(dir.path(), "alpha").expect("load");
        assert!(seed.starts_with("SU"));
    }

    #[test]
    fn stable_0012_024_malformed_seed_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        ensure_layout(dir.path()).expect("layout");
        let path = seed_path(dir.path(), "alpha");
        fs::write(&path, b"not-a-seed\n").expect("write");
        platform::restrict_to_owner(&path, false).expect("restrict");
        let err = load_seed(dir.path(), "alpha").unwrap_err();
        assert!(err.to_string().contains("credential_invalid"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn stable_0012_024_broad_seed_mode_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        ensure_layout(dir.path()).expect("layout");
        ensure_identity(dir.path(), "alpha").expect("create");
        let path = seed_path(dir.path(), "alpha");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).expect("chmod");
        let err = load_seed(dir.path(), "alpha").unwrap_err();
        assert!(err.to_string().contains("credential_acl"), "{err}");
    }

    #[test]
    fn stable_0015_081_grant_lengths() {
        let grant = Grant::generate().expect("grant");
        assert_eq!(grant.id.len(), 16);
        assert_eq!(grant.credential.len(), 32);
        assert_ne!(grant.credential, [0u8; 32]);
    }

    #[test]
    fn stable_0015_082_delegation_file_written_owner_only_and_removed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(DELEGATION_FILE);
        let grant = Grant::generate().expect("grant");
        let file = DelegationFile::new(
            &grant,
            "endpoint".to_owned(),
            Path::new("/abs/shim"),
            vec!["beta".to_owned()],
            (4, 30, 120),
        );
        write_delegation(&path, &file).expect("write");
        platform::verify_owner_only(&path, false).expect("owner-only");
        let text = fs::read_to_string(&path).expect("read");
        assert!(text.contains("grant_credential"));
        remove_delegation(&path).expect("remove");
        assert!(!path.exists());
        remove_delegation(&path).expect("idempotent");
    }

    fn operator_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = dir.path().join("state");
        ensure_layout(&state).expect("layout");
        let trust = dir.path().join("user").join("trust.toml");
        (dir, state, trust)
    }

    #[test]
    fn req_0007_006_first_start_creates_operator_key_and_trust_entry() {
        let (dir, state, trust) = operator_fixture();
        let kid = ensure_operator(&state, &trust, &[]).expect("bootstrap");
        assert_eq!(kid.agent().as_str(), "operator");
        let key = storage::operator_key_path(&state.join(CREDS_DIR), &kid).expect("key path");
        assert!(key.is_file(), "operator key must be in creds/");
        assert!(!dir.path().join("user").join("keys").exists());
        let store = a2a_security::trust::TrustStore::load(&trust).expect("trust loads");
        let agent = a2a_types::ids::AgentId::parse("operator").expect("agent");
        let entry = store.active_for_agent(&agent).expect("operator entry");
        assert_eq!(entry.kid().to_string(), kid.to_string());
        let signer = storage::load_key_file(&key, &store, &agent, &[]).expect("key loads");
        assert_eq!(signer.kid().to_string(), kid.to_string());
    }

    #[test]
    fn req_0007_006_second_start_is_idempotent() {
        let (_dir, state, trust) = operator_fixture();
        let first = ensure_operator(&state, &trust, &[]).expect("first");
        let key = storage::operator_key_path(&state.join(CREDS_DIR), &first).expect("key path");
        let trust_before = fs::read(&trust).expect("read trust");
        let key_before = fs::read(&key).expect("read key");
        let second = ensure_operator(&state, &trust, &[]).expect("second");
        assert_eq!(first.to_string(), second.to_string());
        assert_eq!(fs::read(&trust).expect("read trust"), trust_before);
        assert_eq!(fs::read(&key).expect("read key"), key_before);
    }

    #[test]
    fn req_0007_010_operator_key_is_owner_only() {
        let (_dir, state, trust) = operator_fixture();
        let kid = ensure_operator(&state, &trust, &[]).expect("bootstrap");
        let key = storage::operator_key_path(&state.join(CREDS_DIR), &kid).expect("key path");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&key).expect("metadata").permissions().mode();
            assert_eq!(mode & 0o077, 0, "group and other bits must be clear");
        }
        let store = a2a_security::trust::TrustStore::load(&trust).expect("trust loads");
        let agent = a2a_types::ids::AgentId::parse("operator").expect("agent");
        storage::load_key_file(&key, &store, &agent, &[]).expect("permission check passes");
    }

    #[test]
    fn req_0012_023_operator_bootstrap_keeps_bus_seeds() {
        let (_dir, state, trust) = operator_fixture();
        ensure_identity(&state, "cli").expect("seed");
        let seed = fs::read(seed_path(&state, "cli")).expect("read seed");
        ensure_operator(&state, &trust, &[]).expect("bootstrap");
        assert_eq!(fs::read(seed_path(&state, "cli")).expect("read seed"), seed);
    }

    #[test]
    fn req_0012_043_existing_entry_without_key_is_refused() {
        let (_dir, state, trust) = operator_fixture();
        let kid = ensure_operator(&state, &trust, &[]).expect("bootstrap");
        let key = storage::operator_key_path(&state.join(CREDS_DIR), &kid).expect("key path");
        fs::remove_file(&key).expect("remove key");
        assert!(ensure_operator(&state, &trust, &[]).is_err());
        assert!(
            !key.exists(),
            "a missing key must not be silently regenerated"
        );
    }

    #[test]
    fn req_0007_014_key_under_workspace_root_is_refused() {
        let (_dir, state, trust) = operator_fixture();
        let roots = vec![state.clone()];
        assert!(ensure_operator(&state, &trust, &roots).is_err());
    }
}
