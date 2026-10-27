//! Control surface shared by the `worker` and `gateway` child subcommands
//! (RFC-0012 sec 4.5 and 4.6).
//!
//! A child process learns about shutdown through [`HostControl::stop_flag`], which
//! the child's signal and lifeline handlers set. It reports readiness through
//! [`HostControl::report_ready`], which creates `<state>/run/<id>.ready`; the
//! supervisor's `bus-presence` probe reads that file, so no probe ever sends a
//! credential (RFC-0012 0012-009, 0012-010).
//! Worker hosts also report the driver's liveness through [`HostControl::report_health`]
//! (`<state>/run/<id>.health`), so the supervisor never shows an agent `ready` while its
//! backend is not live (RFC-0004 0004-015).

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

/// Name of the run directory under the state directory (RFC-0012 4.2).
pub const RUN_DIR: &str = "run";

/// Suffix of the readiness file in the run directory.
pub const READY_SUFFIX: &str = "ready";

/// Suffix of the driver health file in the run directory.
pub const HEALTH_SUFFIX: &str = "health";

/// Shutdown and readiness control of one child process.
#[derive(Debug, Clone)]
pub struct HostControl {
    state_dir: PathBuf,
    ready_file: PathBuf,
    health_file: PathBuf,
    stop: Arc<AtomicBool>,
}

impl HostControl {
    /// Builds the control for the child whose identity is `id`.
    #[must_use]
    pub fn new(state_dir: PathBuf, id: &str, stop: Arc<AtomicBool>) -> Self {
        let ready_file = ready_path(&state_dir, id);
        let health_file = health_path(&state_dir, id);
        Self {
            state_dir,
            ready_file,
            health_file,
            stop,
        }
    }

    /// The state directory the child was started with.
    #[must_use]
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// True once a stop signal or lifeline EOF has been observed.
    #[must_use]
    pub fn stop_requested(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// The shared stop flag; handlers set it, hosts poll it.
    #[must_use]
    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop)
    }

    /// Publishes readiness by creating the ready file atomically (write then rename).
    ///
    /// Call only after the agent's presence key holds `PRESENCE_STATE_READY`
    /// (RFC-0012 0012-009).
    ///
    /// # Errors
    /// Returns the I/O error when the run directory cannot be written.
    pub fn report_ready(&self) -> io::Result<()> {
        let tmp = self.ready_file.with_extension("tmp");
        fs::write(&tmp, b"ready\n")?;
        fs::rename(&tmp, &self.ready_file)
    }

    /// Publishes the driver health the supervisor reads (RFC-0004 0004-015). The file is
    /// replaced atomically, like the ready file.
    ///
    /// # Errors
    /// Returns the I/O error when the run directory cannot be written.
    pub fn report_health(&self, health: &HostHealth) -> io::Result<()> {
        let tmp = self.health_file.with_extension("health.tmp");
        let body = serde_json::to_vec(health).map_err(io::Error::other)?;
        fs::write(&tmp, body)?;
        fs::rename(&tmp, &self.health_file)
    }
}

/// Path of the readiness file of `id` under `state_dir`.
#[must_use]
pub fn ready_path(state_dir: &Path, id: &str) -> PathBuf {
    state_dir.join(RUN_DIR).join(format!("{id}.{READY_SUFFIX}"))
}

/// Driver health a worker host publishes (RFC-0004 0004-015). The supervisor keeps an agent
/// `ready` only while its backend is live.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostHealth {
    /// True while the backend can take a turn.
    pub live: bool,
    /// Why the backend is not live, when it is not.
    pub reason: Option<String>,
}

/// Path of the driver health file of `id` under `state_dir`.
#[must_use]
pub fn health_path(state_dir: &Path, id: &str) -> PathBuf {
    state_dir
        .join(RUN_DIR)
        .join(format!("{id}.{HEALTH_SUFFIX}"))
}

/// Reads the driver health `id` last published. `Ok(None)` when no worker host has published it.
///
/// # Errors
/// Returns the I/O error when the file cannot be read or does not parse.
pub fn read_health(state_dir: &Path, id: &str) -> io::Result<Option<HostHealth>> {
    match fs::read(health_path(state_dir, id)) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Failure of a child host. Messages never contain credential bytes.
#[derive(Debug)]
pub enum HostError {
    /// The configuration or the requested identity is not usable.
    Config(String),
    /// A filesystem operation failed.
    Io(io::Error),
    /// The bus could not be reached or refused an operation.
    Bus(String),
    /// The backend could not be started or failed.
    Backend(String),
}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(detail) => write!(f, "configuration: {detail}"),
            Self::Io(err) => write!(f, "io: {err}"),
            Self::Bus(detail) => write!(f, "bus: {detail}"),
            Self::Backend(detail) => write!(f, "backend: {detail}"),
        }
    }
}

impl std::error::Error for HostError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for HostError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// 0004-015: the driver health a worker host publishes is the health the supervisor reads.
    #[test]
    fn req_0004_015_health_file_round_trips() {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::create_dir_all(dir.path().join(RUN_DIR)).expect("run dir");
        let control = HostControl::new(
            dir.path().to_path_buf(),
            "omp",
            Arc::new(AtomicBool::new(false)),
        );
        assert_eq!(read_health(dir.path(), "omp").expect("read"), None);

        let down = HostHealth {
            live: false,
            reason: Some("agent process is not running".to_owned()),
        };
        control.report_health(&down).expect("publish");
        assert_eq!(read_health(dir.path(), "omp").expect("read"), Some(down));

        let live = HostHealth {
            live: true,
            reason: None,
        };
        control.report_health(&live).expect("publish");
        assert_eq!(read_health(dir.path(), "omp").expect("read"), Some(live));
    }
}
