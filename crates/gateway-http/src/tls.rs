//! TLS listener setup for non-loopback binds (RFC-0008 4.9, 0008-066 and 0008-068).
//!
//! Only TLS 1.3 is offered. The certificate and key are read from PEM files. The key file must be
//! owner-only: on Unix it must grant no group or other permission bits; on Windows every access
//! entry must name, by SID, the current user, SYSTEM (`S-1-5-18`) or the local Administrators
//! group (`S-1-5-32-544`), the principals of the default Windows ACL, inherited or not. The check
//! never compares principal names, so it does not depend on the display language: it reads the
//! SDDL that `icacls /save` writes and the current user's SID from `whoami /user`. If the access
//! list or the user SID cannot be read, the key is refused.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::ServerConfig;
use rustls::crypto::ring::default_provider;
use rustls::version::TLS13;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::TlsAcceptor;

use crate::config::TlsConfig;

/// Failure to load or accept the TLS material.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    /// A PEM file or its access list could not be read.
    #[error("reading {path} failed: {source}")]
    Read {
        /// File that could not be read.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },
    /// A PEM file did not yield the expected object.
    #[error("{path} holds no usable PEM {what}")]
    Pem {
        /// File that was parsed.
        path: PathBuf,
        /// Kind of object expected.
        what: &'static str,
    },
    /// The key file is accessible to users other than the service account.
    #[error("private key {path} is accessible to other users; restrict it to the service account")]
    KeyPermissions {
        /// Key file with excessive permissions.
        path: PathBuf,
    },
    /// rustls rejected the certificate and key pair.
    #[error("TLS configuration rejected: {0}")]
    Rustls(#[from] rustls::Error),
}

/// Builds the TLS 1.3-only server configuration from the configured PEM files.
///
/// # Errors
/// Returns an error when a file cannot be read, holds no certificate or key, the key is not
/// owner-only (Unix mode bits or Windows ACL), or rustls rejects the pair.
pub fn server_config(tls: &TlsConfig) -> Result<ServerConfig, TlsError> {
    let certificates = read_certificates(&tls.cert_path)?;
    let key = read_key(&tls.key_path)?;
    let config = ServerConfig::builder_with_provider(Arc::new(default_provider()))
        .with_protocol_versions(&[&TLS13])?
        .with_no_client_auth()
        .with_single_cert(certificates, key)?;
    Ok(config)
}

/// Builds the acceptor used for each inbound connection.
///
/// # Errors
/// Same conditions as [`server_config`].
pub fn acceptor(tls: &TlsConfig) -> Result<TlsAcceptor, TlsError> {
    Ok(TlsAcceptor::from(Arc::new(server_config(tls)?)))
}

fn read_certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let certificates = CertificateDer::pem_file_iter(path)
        .map_err(|error| pem_error(path, "certificate", error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| pem_error(path, "certificate", error))?;
    if certificates.is_empty() {
        return Err(TlsError::Pem {
            path: path.to_path_buf(),
            what: "certificate",
        });
    }
    Ok(certificates)
}

fn read_key(path: &Path) -> Result<PrivateKeyDer<'static>, TlsError> {
    check_key_permissions(path)?;
    PrivateKeyDer::from_pem_file(path).map_err(|error| pem_error(path, "private key", error))
}

fn pem_error(path: &Path, what: &'static str, error: rustls_pki_types::pem::Error) -> TlsError {
    match error {
        rustls_pki_types::pem::Error::Io(source) => TlsError::Read {
            path: path.to_path_buf(),
            source,
        },
        _ => TlsError::Pem {
            path: path.to_path_buf(),
            what,
        },
    }
}

#[cfg(unix)]
fn check_key_permissions(path: &Path) -> Result<(), TlsError> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = std::fs::metadata(path).map_err(|source| TlsError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(TlsError::KeyPermissions {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

#[cfg(windows)]
fn check_key_permissions(path: &Path) -> Result<(), TlsError> {
    let read_error = |source: io::Error| TlsError::Read {
        path: path.to_path_buf(),
        source: io::Error::other(format!("access list could not be read: {source}")),
    };
    let user_sid = current_user_sid().map_err(read_error)?;
    let saved = saved_acl(path).map_err(read_error)?;
    if owner_only_saved_acl(&saved, &user_sid) {
        Ok(())
    } else {
        Err(TlsError::KeyPermissions {
            path: path.to_path_buf(),
        })
    }
}

/// `name` from the system directory, so a key check never runs a binary found on PATH.
#[cfg(windows)]
fn system_tool(name: &str) -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    PathBuf::from(root).join("System32").join(name)
}

/// SID of the user running this process, from `whoami /user`.
#[cfg(windows)]
fn current_user_sid() -> io::Result<String> {
    let output = std::process::Command::new(system_tool("whoami.exe"))
        .args(["/user", "/fo", "csv", "/nh"])
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other("whoami failed"));
    }
    parse_whoami_user_sid(&String::from_utf8_lossy(&output.stdout))
        .ok_or_else(|| io::Error::other("whoami printed no user SID"))
}

/// Private scratch directory for the `icacls /save` output; removed on drop.
#[cfg(windows)]
struct ScratchDir(PathBuf);

#[cfg(windows)]
impl ScratchDir {
    fn create() -> io::Result<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};

        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let dir = std::env::temp_dir().join(format!(
            "a2ab-acl-{}-{nanos}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir)?;
        Ok(Self(dir))
    }
}

#[cfg(windows)]
impl Drop for ScratchDir {
    fn drop(&mut self) {
        // Best effort: the directory only holds a copy of a public access list.
        drop(std::fs::remove_dir_all(&self.0));
    }
}

/// The file `icacls <path> /save` writes: the path, then the SDDL of the file.
#[cfg(windows)]
fn saved_acl(path: &Path) -> io::Result<String> {
    let scratch = ScratchDir::create()?;
    let saved = scratch.0.join("acl.txt");
    let output = std::process::Command::new(system_tool("icacls.exe"))
        .arg(path)
        .arg("/save")
        .arg(&saved)
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other("icacls failed"));
    }
    decode_utf16le(&std::fs::read(&saved)?)
        .ok_or_else(|| io::Error::other("icacls wrote an unreadable access list"))
}

/// SID of the SYSTEM account.
#[cfg(any(windows, test))]
const SYSTEM_SID: &str = "S-1-5-18";
/// SID of the local Administrators group.
#[cfg(any(windows, test))]
const ADMINISTRATORS_SID: &str = "S-1-5-32-544";

/// Decodes the UTF-16LE text `icacls /save` writes (with or without a byte-order mark).
#[cfg(any(windows, test))]
fn decode_utf16le(bytes: &[u8]) -> Option<String> {
    let bytes = bytes.strip_prefix(&[0xFF, 0xFE]).unwrap_or(bytes);
    if bytes.len() % 2 != 0 {
        return None;
    }
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    String::from_utf16(&units).ok()
}

/// Extracts the user SID from `whoami /user /fo csv /nh` output: one line `"<name>","<SID>"`.
/// Only the SID is read; the (localized or domain-qualified) name is ignored.
#[cfg(any(windows, test))]
fn parse_whoami_user_sid(output: &str) -> Option<String> {
    let line = output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let (_name, sid) = line.rsplit_once(',')?;
    let sid = sid.trim().strip_prefix('"')?.strip_suffix('"')?;
    is_sid(sid).then(|| sid.to_ascii_uppercase())
}

/// `S-<revision>-<authority>(-<subauthority>)*`, decimal parts only.
#[cfg(any(windows, test))]
fn is_sid(text: &str) -> bool {
    let mut parts = text.split('-');
    let head = parts.next().is_some_and(|s| s.eq_ignore_ascii_case("S"));
    let rest: Vec<&str> = parts.collect();
    head && rest.len() >= 2
        && rest
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

/// Decides whether the file written by `icacls /save` is owner-only (0008-066). The text is the
/// path on the first line and the SDDL on the next. The DACL must hold at least one ACE, and
/// every ACE must name `user_sid`, SYSTEM or Administrators, by SID or by the SDDL aliases `SY`
/// and `BA`. Inherited ACEs are allowed (the default ACL of a profile directory is inherited);
/// the principal decides. A missing DACL (full access for everyone), an empty DACL, an ACE type
/// other than allow or deny, and malformed text all fail closed.
#[cfg(any(windows, test))]
fn owner_only_saved_acl(saved: &str, user_sid: &str) -> bool {
    let Some(sddl) = saved
        .lines()
        .skip(1)
        .map(str::trim)
        .find(|line| matches!(line.as_bytes(), [b'O' | b'G' | b'D' | b'S', b':', ..]))
    else {
        return false;
    };
    let Some(aces) = dacl_section(sddl).and_then(split_aces) else {
        return false;
    };
    !aces.is_empty() && aces.iter().all(|ace| ace_principal_trusted(ace, user_sid))
}

/// The text of the `D:` section of an SDDL string (flags and ACEs), if it has one.
#[cfg(any(windows, test))]
fn dacl_section(sddl: &str) -> Option<&str> {
    let mut depth = 0usize;
    let mut start = None;
    let bytes = sddl.as_bytes();
    for (i, &byte) in bytes.iter().enumerate() {
        match byte {
            b'(' => depth += 1,
            b')' => depth = depth.checked_sub(1)?,
            b'O' | b'G' | b'D' | b'S' if depth == 0 && bytes.get(i + 1) == Some(&b':') => {
                if let Some(from) = start {
                    return Some(&sddl[from..i]);
                }
                if byte == b'D' {
                    start = Some(i + 2);
                }
            }
            _ => {}
        }
    }
    start.map(|from| &sddl[from..])
}

/// Splits the ACEs `(...)` out of a DACL section. Flags may precede the first ACE; anything else
/// outside parentheses, or unbalanced parentheses, is malformed.
#[cfg(any(windows, test))]
fn split_aces(dacl: &str) -> Option<Vec<&str>> {
    let mut aces = Vec::new();
    let mut depth = 0usize;
    let mut from = 0usize;
    for (i, byte) in dacl.bytes().enumerate() {
        match byte {
            b'(' => {
                if depth == 0 {
                    from = i + 1;
                }
                depth += 1;
            }
            b')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    aces.push(&dacl[from..i]);
                }
            }
            _ if depth == 0 && !aces.is_empty() => return None,
            _ => {}
        }
    }
    (depth == 0).then_some(aces)
}

/// One ACE `type;flags;rights;object;inherited object;account SID`: allow or deny, naming a
/// trusted principal.
#[cfg(any(windows, test))]
fn ace_principal_trusted(ace: &str, user_sid: &str) -> bool {
    let mut fields = ace.split(';');
    if !matches!(fields.next(), Some("A" | "D")) {
        return false;
    }
    let Some(principal) = fields.nth(4).map(str::trim) else {
        return false;
    };
    ["SY", "BA", SYSTEM_SID, ADMINISTRATORS_SID, user_sid]
        .iter()
        .any(|trusted| principal.eq_ignore_ascii_case(trusted))
}

#[cfg(all(not(unix), not(windows)))]
fn check_key_permissions(_path: &Path) -> Result<(), TlsError> {
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const USER_SID: &str = "S-1-5-21-1111111111-2222222222-3333333333-1001";
    /// First line of the `icacls /save` file: the path as given, here with a drive prefix that
    /// looks like a DACL marker and localized characters.
    const SAVED_PATH: &str = "D:\\Benutzer\\Zoe\\Schl\u{fc}ssel.pem";

    fn saved(sddl: &str) -> String {
        format!("{SAVED_PATH}\r\n{sddl}\r\n\r\n")
    }

    #[test]
    fn req_0008_066_default_windows_acl_is_owner_only() {
        let sddl = format!("D:AI(A;ID;FA;;;SY)(A;ID;FA;;;BA)(A;ID;FA;;;{USER_SID})");
        assert!(owner_only_saved_acl(&saved(&sddl), USER_SID));
    }

    #[test]
    fn req_0008_066_localized_account_names_do_not_matter() {
        // A German host: icacls would list `NT-AUTORIT<A-umlaut>T\SYSTEM` and
        // `VORDEFINIERT\Administratoren`; the saved SDDL holds only SIDs and aliases.
        let sddl = format!(
            "O:BAG:SYD:PAI(A;;FA;;;S-1-5-18)(A;;FA;;;S-1-5-32-544)(A;;FA;;;{})",
            USER_SID.to_ascii_lowercase()
        );
        assert!(owner_only_saved_acl(&saved(&sddl), USER_SID));
    }

    #[test]
    fn req_0008_066_key_granted_only_to_the_user_is_owner_only() {
        let sddl = format!("D:PAI(A;;FA;;;{USER_SID})");
        assert!(owner_only_saved_acl(&saved(&sddl), USER_SID));
    }

    #[test]
    fn req_0008_066_sections_after_the_dacl_are_ignored() {
        let sddl = format!("D:(A;;FA;;;{USER_SID})S:(AU;SA;FA;;;WD)");
        assert!(owner_only_saved_acl(&saved(&sddl), USER_SID));
    }

    #[test]
    fn req_0008_066_everyone_and_users_make_the_key_wide() {
        for principal in [
            "WD",
            "S-1-1-0",
            "BU",
            "S-1-5-32-545",
            "AU",
            "S-1-5-11",
            "IU",
            "S-1-5-21-1111111111-2222222222-3333333333-501",
        ] {
            let sddl = format!(
                "D:AI(A;ID;FA;;;SY)(A;ID;FA;;;BA)(A;ID;FA;;;{USER_SID})(A;ID;FR;;;{principal})"
            );
            assert!(
                !owner_only_saved_acl(&saved(&sddl), USER_SID),
                "{principal} accepted"
            );
        }
    }

    #[test]
    fn req_0008_066_look_alike_names_and_sids_are_refused() {
        for principal in [
            r"DESKTOP-1\SYSTEM",
            "NT-AUTORIT\u{c4}T\\SYSTEM",
            "Administrators",
            "S-1-5-18-1",
            "S-1-5-32-5440",
            "S-1-5-1800",
            "SYS",
            "",
        ] {
            let sddl = format!("D:(A;;FA;;;{USER_SID})(A;;FA;;;{principal})");
            assert!(
                !owner_only_saved_acl(&saved(&sddl), USER_SID),
                "{principal:?} accepted"
            );
        }
    }

    #[test]
    fn req_0008_066_user_name_in_place_of_a_sid_is_refused() {
        assert!(!owner_only_saved_acl(
            &saved("D:(A;;FA;;;svc-gateway)"),
            USER_SID
        ));
    }

    #[test]
    fn req_0008_066_another_users_sid_is_refused() {
        let sddl = "D:(A;;FA;;;S-1-5-21-1111111111-2222222222-3333333333-1002)";
        assert!(!owner_only_saved_acl(&saved(sddl), USER_SID));
    }

    #[test]
    fn req_0008_066_untrusted_deny_and_unknown_ace_types_fail_closed() {
        for ace in [
            "(D;;FR;;;WD)",
            "(OA;;CR;5f202010-79a5-11d0-9020-00c04fc2d4cf;;WD)",
            "(XA;;FA;;;WD;(@User.x==1))",
            "(A;;FA)",
            "()",
        ] {
            let sddl = format!("D:(A;;FA;;;{USER_SID}){ace}");
            assert!(
                !owner_only_saved_acl(&saved(&sddl), USER_SID),
                "{ace} accepted"
            );
        }
    }

    #[test]
    fn req_0008_066_missing_empty_or_malformed_dacl_is_refused() {
        for sddl in [
            "O:BAG:SY",
            "D:",
            "D:PAI",
            "S:(AU;SA;FA;;;WD)",
            "D:(A;;FA;;;SY",
            "D:(A;;FA;;;SY))",
            "D:(A;;FA;;;SY)junk",
            "",
        ] {
            assert!(
                !owner_only_saved_acl(&saved(sddl), USER_SID),
                "{sddl:?} accepted"
            );
        }
        // Only the path line, no SDDL: a path starting `D:` must not be taken for a DACL.
        assert!(!owner_only_saved_acl(SAVED_PATH, USER_SID));
    }

    #[test]
    fn req_0008_066_saved_acl_decodes_from_utf16le_with_or_without_bom() {
        let text = "k.pem\r\nD:(A;;FA;;;SY)\r\n";
        let plain: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let mut with_bom = vec![0xFF, 0xFE];
        with_bom.extend_from_slice(&plain);
        assert_eq!(decode_utf16le(&plain).as_deref(), Some(text));
        assert_eq!(decode_utf16le(&with_bom).as_deref(), Some(text));
        assert_eq!(decode_utf16le(&plain[..plain.len() - 1]), None);
        assert_eq!(decode_utf16le(&[0x00, 0xD8]), None);
    }

    #[test]
    fn req_0008_066_whoami_csv_yields_the_user_sid() {
        for output in [
            "\"dx\\0x1de\",\"S-1-5-21-1-2-3-1001\"\r\n",
            "\r\n\"NT-AUTORIT\u{c4}T\\SYSTEM\",\"S-1-5-18\"\r\n",
            "\"a,b\\c\",\"s-1-5-21-1-2-3-1001\"\n",
        ] {
            let sid = parse_whoami_user_sid(output).unwrap();
            assert!(sid.starts_with("S-1-5-"), "{sid}");
        }
        assert_eq!(
            parse_whoami_user_sid("\"dx\\u\",\"S-1-5-21-1-2-3-1001\"").as_deref(),
            Some("S-1-5-21-1-2-3-1001")
        );
        for output in [
            "",
            "ERROR: unexpected argument",
            "\"dx\\u\",\"not-a-sid\"",
            "\"dx\\u\",\"S-1\"",
            "\"dx\\u\",\"S-1-5-x\"",
            "\"dx\\u\",S-1-5-21-1-2-3-1001",
        ] {
            assert_eq!(parse_whoami_user_sid(output), None, "{output:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn req_0008_066_key_readable_by_group_or_other_is_refused() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("key.pem");
        std::fs::write(&key, b"not a real key").unwrap();
        for mode in [0o640, 0o604] {
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(mode)).unwrap();
            assert!(matches!(
                check_key_permissions(&key),
                Err(TlsError::KeyPermissions { .. })
            ));
        }
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(check_key_permissions(&key).is_ok());
    }

    #[cfg(windows)]
    fn windows_key(dir: &tempfile::TempDir) -> PathBuf {
        let key = dir.path().join("key.pem");
        std::fs::write(&key, b"not a real key").unwrap();
        key
    }

    #[cfg(windows)]
    fn run_icacls(key: &Path, args: &[&str]) {
        let output = std::process::Command::new(system_tool("icacls.exe"))
            .arg(key)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "icacls {args:?} failed");
    }

    #[cfg(windows)]
    #[test]
    fn req_0008_066_windows_key_granted_only_to_the_user_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let key = windows_key(&dir);
        let user = std::env::var("USERNAME").unwrap();
        run_icacls(&key, &["/inheritance:r", "/grant:r", &format!("{user}:F")]);
        assert!(check_key_permissions(&key).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn req_0008_066_windows_default_key_acl_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let key = windows_key(&dir);
        assert!(check_key_permissions(&key).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn req_0008_066_windows_key_granted_to_everyone_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let key = windows_key(&dir);
        let user = std::env::var("USERNAME").unwrap();
        run_icacls(&key, &["/inheritance:r", "/grant:r", &format!("{user}:F")]);
        run_icacls(&key, &["/grant", "*S-1-1-0:(R)"]);
        assert!(matches!(
            check_key_permissions(&key),
            Err(TlsError::KeyPermissions { .. })
        ));
    }

    #[cfg(windows)]
    #[test]
    fn req_0008_066_windows_key_granted_to_users_group_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let key = windows_key(&dir);
        let sid = current_user_sid().unwrap();
        run_icacls(&key, &["/inheritance:r", "/grant:r", &format!("*{sid}:F")]);
        assert!(check_key_permissions(&key).is_ok());
        run_icacls(&key, &["/grant", "*S-1-5-32-545:(R)"]);
        assert!(matches!(
            check_key_permissions(&key),
            Err(TlsError::KeyPermissions { .. })
        ));
    }

    #[cfg(windows)]
    #[test]
    fn req_0008_066_windows_unreadable_access_list_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("absent.pem");
        assert!(matches!(
            check_key_permissions(&missing),
            Err(TlsError::Read { .. })
        ));
    }
}
