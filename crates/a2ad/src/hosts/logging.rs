//! Host log output (RFC-0013 0013-038 and 0013-039).
//!
//! A host process writes its `tracing` events to `<state_dir>/logs/<component>.log`. The
//! supervisor redirects the child's stdout and stderr to the same file, so only tracing lines
//! reach this writer and nothing is duplicated. The active file rotates at 10 MiB and keeps five
//! rotated files. Each event is redacted through `a2a_audit::redact::Redactor` before it is
//! written, so a secret that reaches a log call still never persists. Files contain no ANSI
//! sequences on either platform.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use a2a_audit::redact::{Redactor, RuleTally};
use tracing::Dispatch;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

use crate::creds::LOGS_DIR;
use crate::host::HostError;

/// Size at which the active log file rotates (0013-039).
pub(crate) const LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;
/// Rotated files kept beside the active file (0013-039, 0013-049).
pub(crate) const LOG_KEEP: usize = 5;
/// Operator override for the filter (0013-038). Takes precedence over `RUST_LOG`.
pub(crate) const LOG_ENV: &str = "A2AB_LOG";
/// Conventional `tracing` override, honoured after `A2AB_LOG`.
pub(crate) const RUST_LOG_ENV: &str = "RUST_LOG";

/// Output format of log lines (`log.format`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogFormat {
    /// Human-readable lines.
    #[default]
    Text,
    /// One JSON object per line.
    Json,
}

/// Validated `[log]` settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogSettings {
    level: String,
    format: LogFormat,
}

impl Default for LogSettings {
    fn default() -> Self {
        Self {
            level: "info".to_owned(),
            format: LogFormat::Text,
        }
    }
}

impl LogSettings {
    /// Validates `log.level` (a filter directive such as `info` or `warn,a2ad=debug`) and
    /// `log.format` (`text` or `json`). Errors name the key, never a value.
    pub fn new(level: &str, format: &str) -> Result<Self, String> {
        if level.trim().is_empty() {
            return Err("log.level must not be empty".to_owned());
        }
        EnvFilter::try_new(level).map_err(|err| format!("log.level is invalid: {err}"))?;
        let format = match format {
            "text" => LogFormat::Text,
            "json" => LogFormat::Json,
            _ => return Err("log.format must be \"text\" or \"json\"".to_owned()),
        };
        Ok(Self {
            level: level.to_owned(),
            format,
        })
    }

    /// The configured filter directive.
    pub fn level(&self) -> &str {
        &self.level
    }

    /// The configured output format.
    pub fn format(&self) -> LogFormat {
        self.format
    }
}

/// Path of a component's log file: `<state_dir>/logs/<component>.log`.
pub(crate) fn log_path(state_dir: &Path, component: &str) -> PathBuf {
    state_dir.join(LOGS_DIR).join(format!("{component}.log"))
}

/// Installs the process-wide subscriber for one host component. Fails if a subscriber is
/// already installed, so a host never silently replaces another's output.
pub(crate) fn init(
    state_dir: &Path,
    component: &str,
    settings: &LogSettings,
) -> Result<(), HostError> {
    let path = log_path(state_dir, component);
    let filter = resolve_filter(
        std::env::var(LOG_ENV).ok(),
        std::env::var(RUST_LOG_ENV).ok(),
        settings.level(),
    )?;
    let dispatch = build_dispatch(&path, settings.format(), filter, LOG_MAX_BYTES, LOG_KEEP)?;
    tracing::dispatcher::set_global_default(dispatch)
        .map_err(|_| HostError::Config("logging is already initialised".to_owned()))
}

/// Chooses the filter: `A2AB_LOG`, then `RUST_LOG`, then `log.level`. Blank overrides are
/// ignored. Values are directives, not secrets, so errors may show them.
fn resolve_filter(
    a2ab_log: Option<String>,
    rust_log: Option<String>,
    configured: &str,
) -> Result<EnvFilter, HostError> {
    let directive = [a2ab_log, rust_log]
        .into_iter()
        .flatten()
        .find(|value| !value.trim().is_empty())
        .unwrap_or_else(|| configured.to_owned());
    EnvFilter::try_new(&directive)
        .map_err(|err| HostError::Config(format!("log filter is invalid: {err}")))
}

/// Builds a dispatch that writes redacted lines to a rotating file at `path`.
fn build_dispatch(
    path: &Path,
    format: LogFormat,
    filter: EnvFilter,
    max_bytes: u64,
    keep: usize,
) -> Result<Dispatch, HostError> {
    let sink = LogSink {
        file: Arc::new(Mutex::new(RotatingFile::open(path, max_bytes, keep)?)),
        redactor: Arc::new(Redactor::new()),
    };
    let dispatch = match format {
        LogFormat::Text => Dispatch::new(
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(sink)
                .finish(),
        ),
        LogFormat::Json => Dispatch::new(
            tracing_subscriber::fmt()
                .json()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(sink)
                .finish(),
        ),
    };
    Ok(dispatch)
}

/// Shared handle to the rotating file and the redaction rules.
#[derive(Debug, Clone)]
struct LogSink {
    file: Arc<Mutex<RotatingFile>>,
    redactor: Arc<Redactor>,
}

impl<'a> MakeWriter<'a> for LogSink {
    type Writer = LogRecord;

    fn make_writer(&'a self) -> LogRecord {
        LogRecord {
            sink: self.clone(),
            buf: Vec::new(),
        }
    }
}

/// The bytes of one event. They are buffered until the event ends, then redacted and written as
/// one unit, so a secret split across formatter writes is still caught.
#[derive(Debug)]
struct LogRecord {
    sink: LogSink,
    buf: Vec<u8>,
}

impl Write for LogRecord {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for LogRecord {
    fn drop(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        let text = String::from_utf8_lossy(&self.buf);
        let mut tally = RuleTally::default();
        let redacted = self.sink.redactor.redact_text(&text, &mut tally);
        // Logging must never take the host down: a poisoned lock or failed write drops the line.
        if let Ok(mut file) = self.sink.file.lock() {
            let _ = file.write_record(redacted.as_bytes());
        }
    }
}

/// The active log file with size-based rotation.
#[derive(Debug)]
struct RotatingFile {
    path: PathBuf,
    file: File,
    written: u64,
    max_bytes: u64,
    keep: usize,
    /// Set after a rotation fails (for example a Windows sharing conflict). Appending continues
    /// on the current file instead of losing rotated files on every retry.
    rotation_failed: bool,
}

impl RotatingFile {
    fn open(path: &Path, max_bytes: u64, keep: usize) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = open_append(path)?;
        let written = file.metadata()?.len();
        Ok(Self {
            path: path.to_path_buf(),
            file,
            written,
            max_bytes,
            keep: keep.max(1),
            rotation_failed: false,
        })
    }

    fn write_record(&mut self, record: &[u8]) -> io::Result<()> {
        let len = record.len() as u64;
        if !self.rotation_failed
            && self.written > 0
            && self.written.saturating_add(len) > self.max_bytes
            && self.rotate().is_err()
        {
            self.rotation_failed = true;
        }
        self.file.write_all(record)?;
        self.written = self.written.saturating_add(len);
        Ok(())
    }

    /// Shifts `.N` to `.N+1` (dropping the oldest), moves the active file to `.1`, and reopens.
    fn rotate(&mut self) -> io::Result<()> {
        remove_if_exists(&rotated_path(&self.path, self.keep))?;
        for index in (1..self.keep).rev() {
            let from = rotated_path(&self.path, index);
            if from.exists() {
                fs::rename(&from, rotated_path(&self.path, index + 1))?;
            }
        }
        fs::rename(&self.path, rotated_path(&self.path, 1))?;
        self.file = open_append(&self.path)?;
        self.written = 0;
        Ok(())
    }
}

/// `<name>.<index>` beside the active file.
fn rotated_path(path: &Path, index: usize) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!("{name}.{index}"))
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn open_append(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Share read, write and delete so rotation, retention and operators are not blocked
        // by an open log file (the same rule as RFC-0013 0013-020).
        const FILE_SHARE_READ: u32 = 0x1;
        const FILE_SHARE_WRITE: u32 = 0x2;
        const FILE_SHARE_DELETE: u32 = 0x4;
        options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
    }
    options.open(path)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tracing_subscriber::EnvFilter;

    fn info_filter() -> EnvFilter {
        EnvFilter::new("info")
    }

    /// RFC-0013 0013-039: the log file exists and holds the first event after start.
    #[test]
    fn req_0013_039_log_file_created_after_start() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = log_path(dir.path(), "omp");
        let dispatch = build_dispatch(
            &path,
            LogFormat::Text,
            info_filter(),
            LOG_MAX_BYTES,
            LOG_KEEP,
        )
        .expect("dispatch");
        tracing::dispatcher::with_default(&dispatch, || {
            tracing::info!("worker started");
        });
        let text = fs::read_to_string(&path).expect("log file");
        assert!(!text.is_empty());
        assert!(text.contains("worker started"));
        assert!(!text.contains('\u{1b}'), "no ANSI sequences in log files");
    }

    /// RFC-0013 0013-039: JSON format writes one JSON object per line.
    #[test]
    fn req_0013_039_json_format_writes_json_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = log_path(dir.path(), "claude");
        let dispatch = build_dispatch(
            &path,
            LogFormat::Json,
            info_filter(),
            LOG_MAX_BYTES,
            LOG_KEEP,
        )
        .expect("dispatch");
        tracing::dispatcher::with_default(&dispatch, || {
            tracing::info!(target: "driver_acp", "child started");
        });
        let text = fs::read_to_string(&path).expect("log file");
        let line = text.lines().next().expect("one line");
        let value: serde_json::Value = serde_json::from_str(line).expect("json line");
        assert_eq!(value["fields"]["message"], "child started");
    }

    /// RFC-0013 0013-039: secrets never reach the file, even if a caller logs them.
    #[test]
    fn req_0013_039_log_lines_are_redacted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = log_path(dir.path(), "omp");
        let dispatch = build_dispatch(
            &path,
            LogFormat::Text,
            info_filter(),
            LOG_MAX_BYTES,
            LOG_KEEP,
        )
        .expect("dispatch");
        tracing::dispatcher::with_default(&dispatch, || {
            tracing::info!(
                "child stderr: auth Bearer SECRETTOKEN0123456789abcdef and password=hunter2hunter2"
            );
        });
        let text = fs::read_to_string(&path).expect("log file");
        assert!(!text.contains("SECRETTOKEN0123456789abcdef"));
        assert!(!text.contains("hunter2hunter2"));
        assert!(text.contains("[redacted"));
    }

    /// RFC-0013 0013-039: the active file rotates at the limit and keeps the configured count.
    #[test]
    fn req_0013_039_rotates_at_limit_and_keeps_count() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = log_path(dir.path(), "omp");
        let dispatch =
            build_dispatch(&path, LogFormat::Text, info_filter(), 200, 2).expect("dispatch");
        tracing::dispatcher::with_default(&dispatch, || {
            for index in 0..30 {
                tracing::info!("line number {index} with padding to fill the file quickly");
            }
        });
        assert!(rotated_path(&path, 1).exists());
        assert!(rotated_path(&path, 2).exists());
        assert!(!rotated_path(&path, 3).exists());
    }

    /// RFC-0013 0013-038: `A2AB_LOG` beats `RUST_LOG`, which beats `log.level`.
    #[test]
    fn req_0013_038_filter_precedence() {
        let from_a2ab =
            resolve_filter(Some("warn".into()), Some("debug".into()), "info").expect("filter");
        assert_eq!(from_a2ab.to_string(), "warn");
        let from_rust = resolve_filter(None, Some("debug".into()), "info").expect("filter");
        assert_eq!(from_rust.to_string(), "debug");
        let blank = resolve_filter(Some("  ".into()), None, "error").expect("filter");
        assert_eq!(blank.to_string(), "error");
    }

    /// RFC-0013 0013-038: the default level is info; invalid settings are refused.
    #[test]
    fn req_0013_038_settings_default_and_validation() {
        let defaults = LogSettings::default();
        assert_eq!(defaults.level(), "info");
        assert_eq!(defaults.format(), LogFormat::Text);
        assert!(LogSettings::new("", "text").is_err());
        assert!(LogSettings::new("info", "xml").is_err());
        let json = LogSettings::new("warn,a2ad=debug", "json").expect("valid");
        assert_eq!(json.format(), LogFormat::Json);
    }
}
