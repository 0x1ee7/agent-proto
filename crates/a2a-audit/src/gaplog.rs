//! Durable `warn` record of consumer-observed sequence gaps (RFC-0013 0013-051).
//!
//! The record goes to `<state_dir>/logs/` (0013-039). It never enters the audit files or the
//! stream: only the emitter publishes `AUDIT_GAP` under its own subject (0013-015), so the
//! emitter's chain is not altered by what a consumer observed.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::clock::format_timestamp;
use crate::fsutil;
use crate::sink::SeqGap;

/// File under the logs directory that receives gap warnings.
pub const GAP_LOG_FILE: &str = "audit-gaps.log";

/// Append-only, synced log of consumer-observed sequence gaps.
#[derive(Debug)]
pub struct GapLog {
    path: PathBuf,
    file: File,
}

impl GapLog {
    /// Opens `<logs_dir>/audit-gaps.log` for appending, creating it owner-only when absent.
    pub fn open(logs_dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(logs_dir)?;
        let path = logs_dir.join(GAP_LOG_FILE);
        if !path.exists() {
            drop(fsutil::create_owner_only(&path)?);
            fsutil::sync_parent(&path)?;
        }
        let file = fsutil::open_shared_append(&path)?;
        Ok(Self { path, file })
    }

    /// The log file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends one `warn` line for `gap` and syncs it before returning.
    pub fn warn_gap(&mut self, gap: &SeqGap, now: DateTime<Utc>) -> io::Result<()> {
        let line = gap_line(gap, now);
        self.file.write_all(line.as_bytes())?;
        self.file.sync_data()
    }
}

/// One JSON line: time, level, message, emitter, and the missing range as decimal strings.
fn gap_line(gap: &SeqGap, now: DateTime<Utc>) -> String {
    let record = serde_json::json!({
        "time": format_timestamp(now),
        "level": "warn",
        "msg": "consumer-observed audit sequence gap",
        "emitter": gap.emitter,
        "first": gap.first.to_string(),
        "last": gap.last.to_string(),
    });
    format!("{record}\n")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn at() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 1, 12, 0, 0).single().unwrap()
    }

    /// 0013-051: the gap line is a `warn` record naming the emitter, `first` and `last` as
    /// decimal strings, and it is one complete JSON line.
    #[test]
    fn req_0013_051_gap_line_is_warn_with_decimal_strings() {
        let gap = SeqGap {
            emitter: "agent-a".to_owned(),
            first: 2,
            last: 13,
        };
        let line = gap_line(&gap, at());
        assert!(line.ends_with('\n'));
        let value: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(value["level"], "warn");
        assert_eq!(value["emitter"], "agent-a");
        assert_eq!(value["first"], "2");
        assert_eq!(value["last"], "13");
        assert_eq!(value["time"], "2026-05-01T12:00:00.000Z");
    }

    /// 0013-051: appends accumulate in the log file; the file is created under the given dir.
    #[test]
    fn req_0013_051_warnings_append_to_the_logs_dir() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        let mut log = GapLog::open(&logs).unwrap();
        let gap = SeqGap {
            emitter: "agent-a".to_owned(),
            first: 2,
            last: 3,
        };
        log.warn_gap(&gap, at()).unwrap();
        log.warn_gap(&gap, at()).unwrap();
        let text = fs::read_to_string(logs.join(GAP_LOG_FILE)).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert_eq!(log.path(), logs.join(GAP_LOG_FILE));
    }

    /// 0013-051: a logs path that is occupied by a regular file cannot be opened, and the error
    /// is returned rather than a gap being dropped silently.
    #[test]
    fn req_0013_051_unusable_logs_dir_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("logs");
        fs::write(&blocker, b"not a directory").unwrap();
        assert!(GapLog::open(&blocker).is_err());
    }
}
