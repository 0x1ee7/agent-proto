//! TLS listener setup for non-loopback binds (RFC-0008 4.9, 0008-066 and 0008-068).
//!
//! Only TLS 1.3 is offered. The certificate and key are read from PEM files. On Unix the key file
//! must not be accessible to group or other users. Windows ACLs are not inspected (see AGENTS.md).

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
    /// A PEM file could not be read.
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
    /// The key file is readable by group or other users.
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
/// Returns an error when a file cannot be read, holds no certificate or key, the key is readable
/// by other users (Unix), or rustls rejects the pair.
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

#[cfg(not(unix))]
fn check_key_permissions(_path: &Path) -> Result<(), TlsError> {
    Ok(())
}
