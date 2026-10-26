//! In-process fake worker for tests (feature `testkit`).
//!
//! Wraps the real [`crate::ipc_server`] with one static grant, so tests
//! exercise the same handshake and framing code the worker uses.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

use crate::ipc::{self, IpcError, Secret, reason};
use crate::ipc_server::{self, GrantTable, ServerHandle, ServerRequest};

/// One grant the fake worker accepts. Fields hold wire encodings: ids are
/// base64url strings exactly as the shim receives them.
#[derive(Debug, Clone)]
pub struct TestGrant {
    pub grant_id: String,
    pub credential: Secret,
    pub session_slot: String,
}

impl TestGrant {
    /// Builds a grant from raw bytes (RFC-0015 [0015-043] lengths: 16, 32, 16).
    pub fn new(grant_id: &[u8; 16], credential: &[u8; 32], session_slot: &[u8; 16]) -> Self {
        Self {
            grant_id: b64url(grant_id),
            credential: Secret::new(b64url(credential)),
            session_slot: b64url(session_slot),
        }
    }
}

/// Encodes bytes as unpadded base64url, the wire form of ids and credentials.
pub fn b64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Returns a per-test endpoint: a named pipe on Windows, a socket path in the
/// temp directory elsewhere.
pub fn endpoint_name(tag: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let unique = format!("{}-{tag}-{nanos}", std::process::id());
    if cfg!(windows) {
        format!(r"\\.\pipe\a2abus-test-{unique}")
    } else {
        std::env::temp_dir()
            .join(format!("a2abus-test-{unique}.sock"))
            .to_string_lossy()
            .into_owned()
    }
}

struct StaticGrant {
    grant: TestGrant,
}

impl GrantTable for StaticGrant {
    fn authenticate(
        &self,
        grant_id: &str,
        credential: &Secret,
        session_slot: &str,
    ) -> Result<(), IpcError> {
        let grant_ok = ipc::constant_time_eq(grant_id.as_bytes(), self.grant.grant_id.as_bytes());
        let credential_ok = ipc::constant_time_eq(
            credential.expose().as_bytes(),
            self.grant.credential.expose().as_bytes(),
        );
        if !(grant_ok && credential_ok) {
            return Err(IpcError::new(reason::UNAUTHENTICATED));
        }
        if session_slot != self.grant.session_slot {
            return Err(IpcError::new(reason::SLOT_UNBOUND));
        }
        Ok(())
    }
}

/// A running fake worker. Requests arrive through [`FakeWorker::next_request`];
/// the test answers each with [`ServerRequest::respond`].
#[derive(Debug)]
pub struct FakeWorker {
    _server: ServerHandle,
    requests: std::sync::mpsc::Receiver<ServerRequest>,
}

impl FakeWorker {
    /// Binds `endpoint` and accepts connections that present `grant`.
    pub fn start(endpoint: &str, grant: TestGrant) -> io::Result<Self> {
        let (tx, requests) = std::sync::mpsc::channel();
        let server = ipc_server::spawn(endpoint, Arc::new(StaticGrant { grant }), tx)?;
        Ok(Self {
            _server: server,
            requests,
        })
    }

    /// Waits up to `timeout` for the next authenticated request.
    pub fn next_request(&self, timeout: Duration) -> Option<ServerRequest> {
        self.requests.recv_timeout(timeout).ok()
    }
}
