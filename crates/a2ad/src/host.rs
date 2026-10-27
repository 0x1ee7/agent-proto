//! Control surface shared by the `worker` and `gateway` child subcommands
//! (RFC-0012 sec 4.5 and 4.6).
//!
//! A child process learns about shutdown through [`HostControl::stop_flag`], which
//! the child's signal and lifeline handlers set. It reports readiness through
//! [`HostControl::report_ready`], which creates `<state>/run/<id>.ready`; the
//! supervisor's `bus-presence` probe reads that file, so no probe ever sends a
//! credential (RFC-0012 0012-009, 0012-010).

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Name of the run directory under the state directory (RFC-0012 4.2).
pub const RUN_DIR: &str = "run";

/// Suffix of the readiness file in the run directory.
pub const READY_SUFFIX: &str = "ready";

/// Shutdown and readiness control of one child process.
#[derive(Debug, Clone)]
pub struct HostControl {
    state_dir: PathBuf,
    ready_file: PathBuf,
    stop: Arc<AtomicBool>,
}

impl HostControl {
    /// Builds the control for the child whose identity is `id`.
    #[must_use]
    pub fn new(state_dir: PathBuf, id: &str, stop: Arc<AtomicBool>) -> Self {
        let ready_file = ready_path(&state_dir, id);
        Self {
            state_dir,
            ready_file,
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
}

/// Path of the readiness file of `id` under `state_dir`.
#[must_use]
pub fn ready_path(state_dir: &Path, id: &str) -> PathBuf {
    state_dir.join(RUN_DIR).join(format!("{id}.{READY_SUFFIX}"))
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
