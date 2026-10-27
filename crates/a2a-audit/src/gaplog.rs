//! Durable `warn` record of consumer-observed sequence gaps (RFC-0013 0013-051).
//!
//! The record goes to `<state_dir>/logs/` (0013-039). It never enters the audit files or the
//! stream: only the emitter publishes `AUDIT_GAP` under its own subject (0013-015), so the
//! emitter's chain is not altered by what a consumer observed.
//!
//! The active file rotates at 10 MiB and keeps 5 rotated copies (0013-039):
//! `audit-gaps.log.1` is the newest rotated file and `audit-gaps.log.5` the oldest. The handle is
//! closed before any rename, because Windows refuses to rename a file that is still open.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::clock::format_timestamp;
use crate::fsutil;
use crate::sink::SeqGap;

/// File under the logs directory that receives gap warnings.
pub const GAP_LOG_FILE: &str = "audit-gaps.log";

/// Rotation size of the gap log (0013-039). A record that would take the active file past this
/// many bytes starts a new file, so the active file never exceeds it.
pub const GAP_LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// Rotated copies kept beside the active file (0013-039).
pub const GAP_LOG_KEPT: u32 = 5;

/// Append-only, synced log of consumer-observed sequence gaps.
#[derive(Debug)]
pub struct GapLog {
    path: PathBuf,
    /// Open handle. `None` after a failed write, during rotation, or before the first write.
    file: Option<File>,
    /// Bytes of complete records in the active file.
    len: u64,
    /// Set when a failed write may have left a partial record past `len`.
    dirty: bool,
    max_bytes: u64,
    keep: u32,
}

impl GapLog {
    /// Opens `<logs_dir>/audit-gaps.log` for appending, creating it owner-only when absent, with
    /// the rotation limits of 0013-039.
    pub fn open(logs_dir: &Path) -> io::Result<Self> {
        Self::open_with_limits(logs_dir, GAP_LOG_MAX_BYTES, GAP_LOG_KEPT)
    }

    /// Opens the gap log with explicit rotation limits: rotate past `max_bytes`, keep `keep`
    /// rotated copies. `keep` must be at least 1.
    pub fn open_with_limits(logs_dir: &Path, max_bytes: u64, keep: u32) -> io::Result<Self> {
        if keep == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the gap log must keep at least one rotated file",
            ));
        }
        fs::create_dir_all(logs_dir)?;
        let path = logs_dir.join(GAP_LOG_FILE);
        if !path.exists() {
            drop(fsutil::create_owner_only(&path)?);
            fsutil::sync_parent(&path)?;
        }
        let len = fs::metadata(&path)?.len();
        Ok(Self {
            path,
            file: None,
            len,
            dirty: false,
            max_bytes,
            keep,
        })
    }

    /// The active log file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends one `warn` line for `gap` and syncs it before returning. Rotates first when the
    /// line would take the active file past the limit (0013-039).
    pub fn warn_gap(&mut self, gap: &SeqGap, now: DateTime<Utc>) -> io::Result<()> {
        let line = gap_line(gap, now);
        let bytes = line.as_bytes();
        if self.len > 0 && self.len + bytes.len() as u64 > self.max_bytes {
            self.rotate()?;
        }
        let written = self.writer().and_then(|file| {
            file.write_all(bytes)?;
            file.sync_data()
        });
        match written {
            Ok(()) => {
                self.len += bytes.len() as u64;
                Ok(())
            }
            Err(err) => {
                self.file = None;
                self.dirty = true;
                Err(err)
            }
        }
    }

    /// Returns the append handle, reopening it when needed. After a failed write the partial
    /// record is cut back to the last complete one first.
    fn writer(&mut self) -> io::Result<&mut File> {
        if self.file.is_none() {
            if self.dirty {
                let file = fsutil::open_rw(&self.path)?;
                file.set_len(self.len)?;
                file.sync_data()?;
                self.dirty = false;
            }
            self.file = Some(fsutil::open_shared_append(&self.path)?);
        }
        self.file
            .as_mut()
            .ok_or_else(|| io::Error::other("gap log handle is unavailable"))
    }

    /// Shifts the rotated copies up by one, drops the oldest, moves the active file to `.1`, and
    /// creates an empty active file. Missing files are skipped, so a rotation that failed part way
    /// can be retried without losing a copy.
    fn rotate(&mut self) -> io::Result<()> {
        // Windows cannot rename a file this process still has open: release the handle first.
        self.file = None;
        remove_if_exists(&rotated(&self.path, self.keep))?;
        for index in (1..self.keep).rev() {
            rename_if_exists(&rotated(&self.path, index), &rotated(&self.path, index + 1))?;
        }
        rename_if_exists(&self.path, &rotated(&self.path, 1))?;
        drop(fsutil::create_owner_only(&self.path)?);
        fsutil::sync_parent(&self.path)?;
        self.len = 0;
        self.dirty = false;
        Ok(())
    }
}

/// `audit-gaps.log` becomes `audit-gaps.log.<index>`.
fn rotated(path: &Path, index: u32) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{index}"));
    PathBuf::from(name)
}

fn rename_if_exists(from: &Path, to: &Path) -> io::Result<()> {
    match fs::rename(from, to) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
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

    /// The rotation size the spec sets for the gap log (0013-039).
    const SPEC_MAX_BYTES: u64 = 10 * 1024 * 1024;

    fn at() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 1, 12, 0, 0).single().unwrap()
    }

    /// Two-digit `first` values keep every test line the same length.
    fn gap(first: u64) -> SeqGap {
        SeqGap {
            emitter: "agent-a".to_owned(),
            first,
            last: first + 1,
        }
    }

    fn line_len() -> u64 {
        gap_line(&gap(10), at()).len() as u64
    }

    fn file_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// The `first` values of every record in the log directory, oldest file first.
    fn firsts_oldest_first(dir: &Path, keep: u32) -> Vec<u64> {
        let mut paths: Vec<PathBuf> = (1..=keep)
            .rev()
            .map(|index| rotated(&dir.join(GAP_LOG_FILE), index))
            .collect();
        paths.push(dir.join(GAP_LOG_FILE));
        let mut firsts = Vec::new();
        for path in paths {
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            for line in text.lines() {
                let value: serde_json::Value = serde_json::from_str(line).unwrap();
                firsts.push(value["first"].as_str().unwrap().parse().unwrap());
            }
        }
        firsts
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

    /// 0013-039: a file that reaches exactly 10 MiB is not rotated; the next record is what
    /// starts a new file.
    #[test]
    fn req_0013_039_file_that_reaches_10_mib_is_not_rotated() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        fs::create_dir_all(&logs).unwrap();
        let seed = vec![b'x'; (SPEC_MAX_BYTES - line_len()) as usize];
        fs::write(logs.join(GAP_LOG_FILE), &seed).unwrap();
        let mut log = GapLog::open(&logs).unwrap();
        log.warn_gap(&gap(10), at()).unwrap();
        assert_eq!(
            fs::metadata(log.path()).unwrap().len(),
            SPEC_MAX_BYTES,
            "the record that exactly fills the file stays in it"
        );
        assert_eq!(file_names(&logs), vec![GAP_LOG_FILE.to_owned()]);
    }

    /// 0013-039: the record that would pass 10 MiB goes to a new active file. The bytes already in
    /// the old file are kept whole in `audit-gaps.log.1`.
    #[test]
    fn req_0013_039_record_past_10_mib_rotates_and_keeps_prior_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        fs::create_dir_all(&logs).unwrap();
        let seed = vec![b'x'; (SPEC_MAX_BYTES - line_len() + 1) as usize];
        fs::write(logs.join(GAP_LOG_FILE), &seed).unwrap();
        let mut log = GapLog::open(&logs).unwrap();
        log.warn_gap(&gap(10), at()).unwrap();
        assert_eq!(
            fs::read(logs.join("audit-gaps.log.1")).unwrap(),
            seed,
            "the rotated file holds the old bytes unchanged"
        );
        assert_eq!(
            fs::metadata(log.path()).unwrap().len(),
            line_len(),
            "the active file holds only the new record"
        );
    }

    /// 0013-039: with two records per file, twelve records fill the active file and five rotated
    /// copies exactly. Every record is kept, in order. A thirteenth record drops the oldest copy.
    #[test]
    fn req_0013_039_rotation_keeps_five_copies_and_every_record_in_them() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        let limit = 2 * line_len();
        let mut log = GapLog::open_with_limits(&logs, limit, 5).unwrap();
        for first in 10..22 {
            log.warn_gap(&gap(first), at()).unwrap();
        }
        assert_eq!(
            file_names(&logs).len(),
            6,
            "active file plus five rotated copies"
        );
        assert_eq!(
            firsts_oldest_first(&logs, 5),
            (10..22).collect::<Vec<u64>>()
        );
        log.warn_gap(&gap(22), at()).unwrap();
        assert_eq!(file_names(&logs).len(), 6, "no sixth rotated copy is kept");
        assert_eq!(
            firsts_oldest_first(&logs, 5),
            (12..23).collect::<Vec<u64>>(),
            "the oldest record is dropped with the oldest copy, the rest stay in order"
        );
    }

    /// 0013-039: a second record that exactly fills a small limit stays in the active file; the
    /// third starts `audit-gaps.log.1`.
    #[test]
    fn req_0013_039_second_record_fits_exactly_and_third_rotates() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        let mut log = GapLog::open_with_limits(&logs, 2 * line_len(), 5).unwrap();
        log.warn_gap(&gap(10), at()).unwrap();
        log.warn_gap(&gap(11), at()).unwrap();
        assert_eq!(file_names(&logs), vec![GAP_LOG_FILE.to_owned()]);
        log.warn_gap(&gap(12), at()).unwrap();
        assert_eq!(
            file_names(&logs),
            vec![GAP_LOG_FILE.to_owned(), "audit-gaps.log.1".to_owned()]
        );
    }

    /// 0013-039: after a restart the bytes already in the active file count toward the limit, so
    /// rotation happens at the same record as it would without the restart.
    #[test]
    fn req_0013_039_reopen_counts_existing_bytes_toward_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        let limit = 2 * line_len();
        let mut first_run = GapLog::open_with_limits(&logs, limit, 5).unwrap();
        first_run.warn_gap(&gap(10), at()).unwrap();
        drop(first_run);
        let mut second_run = GapLog::open_with_limits(&logs, limit, 5).unwrap();
        second_run.warn_gap(&gap(11), at()).unwrap();
        assert_eq!(file_names(&logs), vec![GAP_LOG_FILE.to_owned()]);
        drop(second_run);
        let mut third_run = GapLog::open_with_limits(&logs, limit, 5).unwrap();
        third_run.warn_gap(&gap(12), at()).unwrap();
        assert_eq!(
            firsts_oldest_first(&logs, 5),
            vec![10, 11, 12],
            "no record is lost across the restarts"
        );
        assert!(logs.join("audit-gaps.log.1").exists());
    }

    /// 0013-039: a log that keeps no rotated copy is refused at open, because rotation would
    /// otherwise discard every record it moves aside.
    #[test]
    fn req_0013_039_zero_rotated_copies_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        assert!(GapLog::open_with_limits(dir.path(), SPEC_MAX_BYTES, 0).is_err());
    }

    /// 0013-039: rotated copies keep the owner-only mode the active file was created with.
    #[cfg(unix)]
    #[test]
    fn req_0013_039_rotated_copy_keeps_owner_only_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        let mut log = GapLog::open_with_limits(&logs, line_len(), 5).unwrap();
        log.warn_gap(&gap(10), at()).unwrap();
        log.warn_gap(&gap(11), at()).unwrap();
        let mode = fs::metadata(logs.join("audit-gaps.log.1"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}
