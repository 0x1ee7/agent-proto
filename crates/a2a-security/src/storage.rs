//! Per-user configuration, key files and permission checks ([0007-010..014], [0007-012]).

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use a2a_types::ids::AgentId;
use ed25519_dalek::pkcs8::{DecodePrivateKey, EncodePrivateKey, KeypairBytes};

use crate::error::SecurityError;
use crate::identity::{Ed25519Signer, Kid, Signer, encode_public_key};
use crate::trust::TrustStore;

const APP_DIR: &str = "a2abus";
const KEY_DIR: &str = "keys";
const TRUST_FILE: &str = "trust.toml";
const PEM_LABEL: &str = "-----BEGIN PRIVATE KEY-----";
const MAX_KEY_FILE_BYTES: u64 = 4096;
/// Principals other than the current user may hold only these ACE tokens ([0007-012]).
const SAFE_NON_USER_TOKENS: &[&str] = &[
    "R", "RX", "RC", "RD", "REA", "RA", "X", "GR", "GE", "N", "I", "OI", "CI", "IO", "NP",
];

/// Which Windows ACL rule a file must satisfy.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AclRule {
    /// Only the current user has an ACE, and no ACE is inherited ([0007-012] key files).
    PrivateToUser,
    /// The current user may write; other principals may read and traverse only ([0007-012] trust file).
    NoOtherWriters,
}

/// Resolves the per-user configuration directory from environment values (pure).
#[must_use]
pub fn config_dir_from_env(
    local_app_data: Option<&OsStr>,
    xdg_config_home: Option<&OsStr>,
    home: Option<&OsStr>,
    windows: bool,
) -> Option<PathBuf> {
    if windows {
        let base = local_app_data.filter(|v| !v.is_empty())?;
        return Some(Path::new(base).join(APP_DIR));
    }
    // XDG_CONFIG_HOME counts only as an absolute POSIX path ([0007-010]); the textual check keeps
    // the Linux rule identical on every host.
    if let Some(xdg) = xdg_config_home.filter(|v| is_posix_absolute(v)) {
        return Some(Path::new(xdg).join(APP_DIR));
    }
    let home = home.filter(|v| !v.is_empty())?;
    Some(Path::new(home).join(".config").join(APP_DIR))
}

/// Whether `value` is an absolute POSIX path, judged textually so the result is the same on every host.
fn is_posix_absolute(value: &OsStr) -> bool {
    value.to_str().is_some_and(|text| text.starts_with('/'))
}

/// The configuration directory of the current user.
pub fn default_config_dir() -> Result<PathBuf, SecurityError> {
    let windows = cfg!(windows);
    let local = std::env::var_os("LOCALAPPDATA");
    let xdg = std::env::var_os("XDG_CONFIG_HOME");
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    config_dir_from_env(local.as_deref(), xdg.as_deref(), home.as_deref(), windows)
        .ok_or_else(|| SecurityError::Storage("no per-user configuration directory".to_owned()))
}

#[must_use]
pub fn key_dir(config: &Path) -> PathBuf {
    config.join(KEY_DIR)
}

#[must_use]
pub fn trust_file(config: &Path) -> PathBuf {
    config.join(TRUST_FILE)
}

/// `keys/<agent>.<K>.key.pem` ([0007-010]).
#[must_use]
pub fn key_path(config: &Path, kid: &Kid) -> PathBuf {
    key_dir(config).join(format!(
        "{}.{}.key.pem",
        kid.agent().as_str(),
        kid.fingerprint()
    ))
}

/// Unix rule for key files and the key directory: no group or other bits ([0007-011]).
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn check_unix_mode(mode: u32) -> Result<(), &'static str> {
    if mode & 0o077 != 0 {
        return Err("group or other permission bits set");
    }
    Ok(())
}

/// Unix rule for the trust file: no group or other write bit ([0007-012]).
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn check_unix_trust_mode(mode: u32) -> Result<(), &'static str> {
    if mode & 0o022 != 0 {
        return Err("group or other write bit set");
    }
    Ok(())
}

/// Checks `icacls` output for one file against `rule` (pure, all platforms).
///
/// Each ACE line reads `<principal>:(<flag or right>)...`. The first line may begin with the
/// path as passed to `icacls`. Unknown tokens, DENY ACEs and an empty ACE list fail closed.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn parse_icacls_output(
    output: &str,
    path: &str,
    user: &str,
    rule: AclRule,
) -> Result<(), &'static str> {
    let mut aces: Vec<(String, Vec<String>)> = Vec::new();
    for (line_no, raw_line) in output.lines().enumerate() {
        let mut line = raw_line.trim_end();
        if line_no == 0 {
            if let Some(rest) = line.strip_prefix(path) {
                line = rest;
            }
        }
        let line = line.trim_start();
        let Some(split) = line.find(":(") else {
            continue;
        };
        let principal = &line[..split];
        let tokens = parse_tokens(&line[split + 1..])?;
        if principal.is_empty() {
            return Err("ACE without principal");
        }
        aces.push((principal.to_owned(), tokens));
    }
    if aces.is_empty() {
        return Err("no ACEs reported");
    }
    let mut user_seen = false;
    for (principal, tokens) in &aces {
        let is_user = principal_is_user(principal, user);
        if tokens.iter().any(|t| t == "DENY") {
            return Err("deny ACE present");
        }
        match rule {
            AclRule::PrivateToUser => {
                if !is_user {
                    return Err("ACE for a principal other than the current user");
                }
                if tokens.iter().any(|t| t == "I") {
                    return Err("inherited ACE present");
                }
                user_seen = true;
            }
            AclRule::NoOtherWriters => {
                if is_user {
                    user_seen = true;
                } else if !tokens
                    .iter()
                    .all(|t| SAFE_NON_USER_TOKENS.contains(&t.as_str()))
                {
                    return Err("non-user principal can write");
                }
            }
        }
    }
    if !user_seen {
        return Err("current user has no ACE");
    }
    Ok(())
}

fn parse_tokens(text: &str) -> Result<Vec<String>, &'static str> {
    let mut tokens = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let inner = rest.strip_prefix('(').ok_or("malformed ACE")?;
        let end = inner.find(')').ok_or("malformed ACE")?;
        let token = &inner[..end];
        if token.is_empty() || !token.chars().all(|c| c.is_ascii_uppercase()) {
            return Err("unknown ACE token");
        }
        tokens.push(token.to_owned());
        rest = &inner[end + 1..];
    }
    if tokens.is_empty() {
        return Err("ACE without rights");
    }
    Ok(tokens)
}

fn principal_is_user(principal: &str, user: &str) -> bool {
    if principal.eq_ignore_ascii_case(user) {
        return true;
    }
    // A bare principal names the account; a domain-qualified one must match in full.
    let user_account = user.rsplit_once('\\').map_or(user, |(_, account)| account);
    !principal.contains('\\') && principal.eq_ignore_ascii_case(user_account)
}

/// Reads at most `max` bytes from a regular file. Symlinks are refused.
pub(crate) fn read_limited(path: &Path, max: u64) -> Result<String, SecurityError> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() {
        return Err(SecurityError::Storage("not a regular file".to_owned()));
    }
    if meta.len() > max {
        return Err(SecurityError::Storage("file too large".to_owned()));
    }
    let mut buf = String::new();
    fs::File::open(path)?
        .take(max + 1)
        .read_to_string(&mut buf)?;
    if buf.len() as u64 > max {
        return Err(SecurityError::Storage("file too large".to_owned()));
    }
    Ok(buf)
}

/// Checks the trust file's permissions ([0007-012]).
pub fn check_trust_file(path: &Path) -> Result<(), SecurityError> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() {
        return Err(SecurityError::Trust(
            "trust file is not a regular file".to_owned(),
        ));
    }
    check_path_acl(path, AclRule::NoOtherWriters).map_err(SecurityError::Trust)
}

/// Loads the active signing key of `agent` from `config/keys` ([0007-010..014]).
pub fn load_signer(
    config: &Path,
    trust: &TrustStore,
    agent: &AgentId,
    roots: &[PathBuf],
) -> Result<Ed25519Signer, SecurityError> {
    let entry = trust
        .active_for_agent(agent)
        .ok_or_else(|| SecurityError::Storage("no active key for agent".to_owned()))?;
    let path = key_path(config, entry.kid());
    refuse_under_roots(&path, roots)?;
    check_key_dir(&key_dir(config), roots)?;
    check_private_file(&path)?;
    let text = read_limited(&path, MAX_KEY_FILE_BYTES)?;
    check_pem_label(&text)?;
    let pair = KeypairBytes::from_pkcs8_pem(&text)
        .map_err(|_| SecurityError::Key("key file does not decode".to_owned()))?;
    let signer = Ed25519Signer::from_seed(agent, &pair.secret_key);
    if signer.kid() != entry.kid() {
        return Err(SecurityError::Key(
            "key does not match its trust entry kid".to_owned(),
        ));
    }
    if encode_public_key(&signer.public_key()) != encode_public_key(&entry.public_key_bytes()) {
        return Err(SecurityError::Key(
            "key does not match its trust entry".to_owned(),
        ));
    }
    Ok(signer)
}

/// Accepts only an unencrypted PKCS#8 PEM key, whose first line is the PRIVATE KEY label ([0007-013]).
fn check_pem_label(text: &str) -> Result<(), SecurityError> {
    if text.lines().next() == Some(PEM_LABEL) {
        return Ok(());
    }
    Err(SecurityError::Key(
        "key file is not an unencrypted PKCS#8 PEM".to_owned(),
    ))
}

/// Generates a new key for `agent`, stores it in `config/keys` and returns the signer.
/// The caller adds the public key to `trust.toml`.
pub fn generate_signer(
    config: &Path,
    agent: &AgentId,
    roots: &[PathBuf],
) -> Result<Ed25519Signer, SecurityError> {
    let signer = Ed25519Signer::generate(agent)?;
    let path = key_path(config, signer.kid());
    refuse_under_roots(&path, roots)?;
    create_key_dir(&key_dir(config))?;
    check_key_dir(&key_dir(config), roots)?;
    let pem = signer
        .signing_key()
        .to_pkcs8_pem(Default::default())
        .map_err(|_| SecurityError::Key("PKCS#8 encoding failed".to_owned()))?;
    let mut file = create_private_file(&path)?;
    let written = file
        .write_all(pem.as_bytes())
        .and_then(|()| file.sync_all());
    if let Err(error) = written {
        let _ = fs::remove_file(&path);
        return Err(SecurityError::Io(error));
    }
    Ok(signer)
}

/// Canonicalizes the nearest existing ancestor and refuses paths under a workspace root ([0007-014]).
fn refuse_under_roots(path: &Path, roots: &[PathBuf]) -> Result<(), SecurityError> {
    let resolved = resolve_existing(path)?;
    for root in roots {
        let canonical = fs::canonicalize(root)
            .map_err(|_| SecurityError::Storage("workspace root cannot be resolved".to_owned()))?;
        if resolved.starts_with(&canonical) {
            return Err(SecurityError::Invalid(
                "key path is inside a workspace root",
            ));
        }
    }
    Ok(())
}

fn resolve_existing(path: &Path) -> Result<PathBuf, SecurityError> {
    let mut existing = path.to_path_buf();
    let mut tail: Vec<OsString> = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name().map(OsStr::to_os_string) else {
            break;
        };
        tail.push(name);
        if !existing.pop() {
            break;
        }
    }
    let mut resolved = fs::canonicalize(&existing)
        .map_err(|_| SecurityError::Storage("path cannot be resolved".to_owned()))?;
    for name in tail.into_iter().rev() {
        resolved.push(name);
    }
    Ok(resolved)
}

fn check_key_dir(dir: &Path, roots: &[PathBuf]) -> Result<(), SecurityError> {
    refuse_under_roots(dir, roots)?;
    check_path_acl(dir, AclRule::PrivateToUser).map_err(SecurityError::Storage)
}

fn check_private_file(path: &Path) -> Result<(), SecurityError> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() {
        return Err(SecurityError::Storage(
            "key is not a regular file".to_owned(),
        ));
    }
    check_path_acl(path, AclRule::PrivateToUser).map_err(SecurityError::Storage)
}

#[cfg(unix)]
fn check_path_acl(path: &Path, rule: AclRule) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path)
        .map_err(|_| "cannot read permissions".to_owned())?
        .permissions()
        .mode();
    match rule {
        AclRule::PrivateToUser => check_unix_mode(mode),
        AclRule::NoOtherWriters => check_unix_trust_mode(mode),
    }
    .map_err(str::to_owned)
}

#[cfg(windows)]
fn check_path_acl(path: &Path, rule: AclRule) -> Result<(), String> {
    let user = current_user()?;
    let output = run_icacls(&[path.as_os_str().to_owned()])?;
    let shown = path.to_string_lossy();
    parse_icacls_output(&output, &shown, &user, rule).map_err(str::to_owned)
}

#[cfg(unix)]
fn create_key_dir(dir: &Path) -> Result<(), SecurityError> {
    use std::os::unix::fs::DirBuilderExt;
    if dir.exists() {
        return Ok(());
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    Ok(())
}

#[cfg(windows)]
fn create_key_dir(dir: &Path) -> Result<(), SecurityError> {
    if dir.exists() {
        return Ok(());
    }
    fs::create_dir_all(dir)?;
    let user = current_user().map_err(SecurityError::Storage)?;
    let grant = format!("{user}:(OI)(CI)F");
    run_icacls(&[
        dir.as_os_str().to_owned(),
        "/inheritance:r".into(),
        "/grant:r".into(),
        grant.into(),
    ])
    .map(|_| ())
    .map_err(SecurityError::Storage)
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> Result<fs::File, SecurityError> {
    use std::os::unix::fs::OpenOptionsExt;
    Ok(fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?)
}

#[cfg(windows)]
fn create_private_file(path: &Path) -> Result<fs::File, SecurityError> {
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    let user = current_user().map_err(SecurityError::Storage)?;
    let grant = format!("{user}:F");
    run_icacls(&[
        path.as_os_str().to_owned(),
        "/inheritance:r".into(),
        "/grant:r".into(),
        grant.into(),
    ])
    .map_err(SecurityError::Storage)?;
    Ok(file)
}

#[cfg(windows)]
fn current_user() -> Result<String, String> {
    let name = std::env::var("USERNAME").map_err(|_| "USERNAME is not set".to_owned())?;
    match std::env::var("USERDOMAIN") {
        Ok(domain) if !domain.is_empty() => Ok(format!("{domain}\\{name}")),
        _ => Ok(name),
    }
}

#[cfg(windows)]
fn run_icacls(args: &[std::ffi::OsString]) -> Result<String, String> {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    let exe = Path::new(&root).join("System32").join("icacls.exe");
    let output = std::process::Command::new(exe)
        .args(args)
        .output()
        .map_err(|_| "icacls could not run".to_owned())?;
    if !output.status.success() {
        return Err("icacls failed".to_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn req_0007_010_windows_config_dir_is_under_local_app_data() {
        let dir = config_dir_from_env(
            Some(OsStr::new("C:\\Users\\x\\AppData\\Local")),
            None,
            None,
            true,
        )
        .expect("resolved");
        assert_eq!(
            dir,
            Path::new("C:\\Users\\x\\AppData\\Local").join("a2abus")
        );
        assert!(config_dir_from_env(None, None, None, true).is_none());
    }

    #[test]
    fn req_0007_010_linux_config_dir_prefers_absolute_xdg_then_home() {
        let xdg = config_dir_from_env(
            None,
            Some(OsStr::new("/cfg")),
            Some(OsStr::new("/home/u")),
            false,
        );
        assert_eq!(xdg, Some(PathBuf::from("/cfg/a2abus")));
        let relative = config_dir_from_env(
            None,
            Some(OsStr::new("cfg")),
            Some(OsStr::new("/home/u")),
            false,
        );
        assert_eq!(relative, Some(PathBuf::from("/home/u/.config/a2abus")));
        assert!(config_dir_from_env(None, None, None, false).is_none());
    }

    #[test]
    fn req_0007_010_key_path_has_no_colon_and_matches_kid() {
        let kid = Kid::derive(&AgentId::parse("alpha").expect("id"), &[1u8; 32]);
        let path = key_path(Path::new("/cfg/a2abus"), &kid);
        let name = path.file_name().and_then(|n| n.to_str()).expect("name");
        assert!(name.starts_with("alpha.") && name.ends_with(".key.pem"));
        assert!(!name.contains(':'));
        assert_eq!(path.parent(), Some(Path::new("/cfg/a2abus/keys")));
    }

    #[test]
    fn req_0007_011_unix_mode_refuses_group_and_other_bits() {
        assert!(check_unix_mode(0o600).is_ok());
        assert!(check_unix_mode(0o700).is_ok());
        assert!(check_unix_mode(0o640).is_err());
        assert!(check_unix_mode(0o604).is_err());
        assert!(check_unix_mode(0o601).is_err());
    }

    #[test]
    fn req_0007_012_trust_file_mode_refuses_group_and_other_writes() {
        assert!(check_unix_trust_mode(0o644).is_ok());
        assert!(check_unix_trust_mode(0o600).is_ok());
        assert!(check_unix_trust_mode(0o664).is_err());
        assert!(check_unix_trust_mode(0o646).is_err());
    }

    const USER: &str = "DESKTOP\\alice";

    #[test]
    fn req_0007_012_private_to_user_accepts_only_the_user_ace() {
        let output = "C:\\k\\a.key.pem DESKTOP\\alice:(F)\n\nSuccessfully processed 1 files; Failed processing 0 files\n";
        assert!(
            parse_icacls_output(output, "C:\\k\\a.key.pem", USER, AclRule::PrivateToUser).is_ok()
        );
        let system =
            "C:\\k\\a.key.pem DESKTOP\\alice:(F)\n                 NT AUTHORITY\\SYSTEM:(F)\n";
        assert!(
            parse_icacls_output(system, "C:\\k\\a.key.pem", USER, AclRule::PrivateToUser).is_err()
        );
    }

    #[test]
    fn req_0007_012_private_to_user_refuses_inherited_ace_for_the_user() {
        let output = "C:\\k\\a.key.pem DESKTOP\\alice:(I)(F)\n";
        assert!(
            parse_icacls_output(output, "C:\\k\\a.key.pem", USER, AclRule::PrivateToUser).is_err()
        );
    }

    #[test]
    fn req_0007_012_no_other_writers_allows_read_only_others() {
        let output =
            "C:\\t\\trust.toml DESKTOP\\alice:(F)\n                 BUILTIN\\Users:(I)(RX)\n";
        assert!(
            parse_icacls_output(output, "C:\\t\\trust.toml", USER, AclRule::NoOtherWriters).is_ok()
        );
        let writable =
            "C:\\t\\trust.toml DESKTOP\\alice:(F)\n                 BUILTIN\\Users:(I)(M)\n";
        assert!(
            parse_icacls_output(writable, "C:\\t\\trust.toml", USER, AclRule::NoOtherWriters)
                .is_err()
        );
    }

    #[test]
    fn req_0007_012_unknown_tokens_deny_and_empty_output_fail_closed() {
        let deny = "C:\\k\\a DESKTOP\\alice:(F)\n                 Everyone:(DENY)(W)\n";
        assert!(parse_icacls_output(deny, "C:\\k\\a", USER, AclRule::NoOtherWriters).is_err());
        assert!(parse_icacls_output("", "C:\\k\\a", USER, AclRule::PrivateToUser).is_err());
        let lowercase = "C:\\k\\a DESKTOP\\alice:(f)\n";
        assert!(parse_icacls_output(lowercase, "C:\\k\\a", USER, AclRule::PrivateToUser).is_err());
    }

    #[test]
    fn req_0007_012_user_match_accepts_bare_or_domain_qualified_names() {
        let bare = "C:\\k\\a alice:(F)\n";
        assert!(parse_icacls_output(bare, "C:\\k\\a", USER, AclRule::PrivateToUser).is_ok());
    }

    #[test]
    fn req_0007_013_key_file_not_pkcs8_pem_is_refused() {
        assert!(check_pem_label(&format!("{PEM_LABEL}\nMII=\n")).is_ok());
        for text in [
            "-----BEGIN ENCRYPTED PRIVATE KEY-----\nMII=\n",
            "-----BEGIN EC PRIVATE KEY-----\nMII=\n",
            "",
        ] {
            assert!(
                matches!(check_pem_label(text), Err(SecurityError::Key(_))),
                "{text:?}"
            );
        }
    }

    #[test]
    fn req_0007_014_key_path_under_workspace_root_is_refused() {
        let root = std::env::temp_dir().join(format!("a2a-ws-{}", std::process::id()));
        fs::create_dir_all(root.join("keys")).expect("dir");
        let inside = root.join("keys").join("a.key.pem");
        let err = refuse_under_roots(&inside, std::slice::from_ref(&root)).err();
        assert!(matches!(err, Some(SecurityError::Invalid(_))));
        let elsewhere = std::env::temp_dir().join("a2a-outside-root.key.pem");
        assert!(refuse_under_roots(&elsewhere, std::slice::from_ref(&root)).is_ok());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn req_0007_010_generated_key_is_stored_in_keys_dir_and_reloads() {
        let config = std::env::temp_dir().join(format!("a2a-cfg-{}", std::process::id()));
        let _ = fs::remove_dir_all(&config);
        let agent = AgentId::parse("alpha").expect("id");
        let signer = generate_signer(&config, &agent, &[]).expect("generated");
        let path = key_path(&config, signer.kid());
        assert!(path.is_file());
        let pem = read_limited(&path, MAX_KEY_FILE_BYTES).expect("read");
        assert_eq!(pem.lines().next(), Some(PEM_LABEL));
        let trust_text = format!(
            "schema = \"a2abus.trust/1\"\n[[key]]\nkid = \"{}\"\nagent = \"alpha\"\nrole = \"agent\"\npublic_key = \"{}\"\n",
            signer.kid(),
            encode_public_key(&signer.public_key()),
        );
        let trust = TrustStore::parse(&trust_text).expect("trust loads");
        let reloaded = load_signer(&config, &trust, &agent, &[]).expect("reloads");
        assert_eq!(reloaded.kid(), signer.kid());
        assert_eq!(reloaded.sign_bytes(b"m"), signer.sign_bytes(b"m"));
        let _ = fs::remove_dir_all(&config);
    }
}
