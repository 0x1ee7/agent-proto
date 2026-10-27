//! TLS listener setup for non-loopback binds (RFC-0008 4.9, 0008-066 and 0008-068).
//!
//! Only TLS 1.3 is offered. The certificate and key are read from PEM files. The key file must be
//! owner-only: on Unix it must grant no group or other permission bits; on Windows every access
//! entry must name the current user, `NT AUTHORITY\SYSTEM` or `BUILTIN\Administrators` (the
//! principals of the default Windows ACL, inherited or not), checked with `icacls`. If the access
//! list cannot be read, the key is refused.

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
    let read_error = |reason: &str| TlsError::Read {
        path: path.to_path_buf(),
        source: io::Error::other(format!("access list could not be read: {reason}")),
    };
    let user = std::env::var("USERNAME").map_err(|_| read_error("USERNAME is not set"))?;
    let output = std::process::Command::new(icacls_path())
        .arg(path)
        .output()
        .map_err(|source| TlsError::Read {
            path: path.to_path_buf(),
            source,
        })?;
    if !output.status.success() {
        return Err(read_error("icacls failed"));
    }
    let listing = String::from_utf8_lossy(&output.stdout);
    let path_text = path.to_string_lossy();
    if owner_only_listing(&listing, &path_text, &user) {
        Ok(())
    } else {
        Err(TlsError::KeyPermissions {
            path: path.to_path_buf(),
        })
    }
}

/// `icacls.exe` from the system directory, so a key check never runs a binary found on PATH.
#[cfg(windows)]
fn icacls_path() -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    PathBuf::from(root).join("System32").join("icacls.exe")
}

/// Access-list principals of the default Windows ACL that may hold a key besides its owner: the
/// SYSTEM account and the local Administrators group, by name and by SID (0008-066).
#[cfg(any(windows, test))]
const TRUSTED_PRINCIPALS: [&str; 4] = [
    r"NT AUTHORITY\SYSTEM",
    "*S-1-5-18",
    r"BUILTIN\Administrators",
    "*S-1-5-32-544",
];

/// Decides whether an `icacls <path>` listing is owner-only (0008-066). The listing
/// must hold at least one access entry, and every entry must name `user` (bare, or after a
/// `DOMAIN\` prefix, compared case-insensitively) or a principal in [`TRUSTED_PRINCIPALS`].
/// Inherited entries are allowed: the default ACL of a profile directory is inherited, and the
/// principal decides.
#[cfg(any(windows, test))]
fn owner_only_listing(listing: &str, path: &str, user: &str) -> bool {
    let mut entries = 0usize;
    for line in listing.lines() {
        let rest = line.strip_prefix(path).unwrap_or(line).trim();
        let Some((principal, _rights)) = rest.split_once(":(") else {
            continue;
        };
        entries += 1;
        if !trusted_principal(principal, user) {
            return false;
        }
    }
    entries > 0
}

#[cfg(any(windows, test))]
fn trusted_principal(principal: &str, user: &str) -> bool {
    let names_user = principal.eq_ignore_ascii_case(user)
        || principal
            .rsplit_once('\\')
            .is_some_and(|(_, name)| name.eq_ignore_ascii_case(user));
    names_user
        || TRUSTED_PRINCIPALS
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

    const PATH: &str = r"C:\state\tls\key.pem";
    const USER: &str = "svc-gateway";

    #[test]
    fn req_0008_066_listing_naming_only_the_user_is_owner_only() {
        let listing = format!(
            "{PATH} DESKTOP-1\\{USER}:(F)\r\n\r\nSuccessfully processed 1 files; Failed processing 0 files\r\n"
        );
        assert!(owner_only_listing(&listing, PATH, USER));
    }

    #[test]
    fn req_0008_066_bare_user_name_is_owner_only_and_case_insensitive() {
        let listing = format!("{PATH} SVC-GATEWAY:(F)\n");
        assert!(owner_only_listing(&listing, PATH, USER));
    }

    #[test]
    fn req_0008_066_second_principal_makes_the_key_wide() {
        let listing = format!(
            "{PATH} DESKTOP-1\\{USER}:(F)\n                BUILTIN\\Users:(R)\n\nSuccessfully processed 1 files; Failed processing 0 files\n"
        );
        assert!(!owner_only_listing(&listing, PATH, USER));
    }

    #[test]
    fn req_0008_066_inherited_entry_for_the_user_is_owner_only() {
        let listing = format!("{PATH} DESKTOP-1\\{USER}:(I)(F)\n");
        assert!(owner_only_listing(&listing, PATH, USER));
    }

    #[test]
    fn req_0008_066_default_windows_acl_is_owner_only() {
        let listing = format!(
            "{PATH} NT AUTHORITY\\SYSTEM:(I)(F)\n                BUILTIN\\Administrators:(I)(F)\n                DESKTOP-1\\{USER}:(I)(F)\n\nSuccessfully processed 1 files; Failed processing 0 files\n"
        );
        assert!(owner_only_listing(&listing, PATH, USER));
    }

    #[test]
    fn req_0008_066_system_and_administrators_by_sid_are_owner_only() {
        let listing = format!(
            "{PATH} *S-1-5-18:(F)\n                *S-1-5-32-544:(F)\n                {USER}:(F)\n"
        );
        assert!(owner_only_listing(&listing, PATH, USER));
    }

    #[test]
    fn req_0008_066_other_principals_make_the_key_wide() {
        for principal in [
            "Everyone",
            r"NT AUTHORITY\Authenticated Users",
            r"BUILTIN\Users",
            "*S-1-1-0",
            "*S-1-5-11",
            r"DESKTOP-1\Guest",
        ] {
            let listing = format!("{PATH} {USER}:(F)\n                {principal}:(I)(R)\n");
            assert!(
                !owner_only_listing(&listing, PATH, USER),
                "{principal} accepted"
            );
        }
    }

    #[test]
    fn req_0008_066_look_alike_system_and_administrators_are_refused() {
        for principal in [
            r"DESKTOP-1\SYSTEM",
            r"EVIL\Administrators",
            r"NT AUTHORITY\Administrators",
        ] {
            let listing = format!("{PATH} {principal}:(F)\n");
            assert!(
                !owner_only_listing(&listing, PATH, USER),
                "{principal} accepted"
            );
        }
    }

    #[test]
    fn req_0008_066_principal_that_only_ends_in_the_user_name_is_refused() {
        let listing = format!("{PATH} DESKTOP-1\\x{USER}:(R)\n");
        assert!(!owner_only_listing(&listing, PATH, USER));
    }

    #[test]
    fn req_0008_066_listing_without_access_entries_is_not_owner_only() {
        let listing = "Successfully processed 1 files; Failed processing 0 files\n";
        assert!(!owner_only_listing(listing, PATH, USER));
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
        let output = std::process::Command::new(icacls_path())
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
}
