//! Host-wide JSONL sink (RFC-0013 0013-016..0013-021). One canonical JSON line per event,
//! synced before the caller may acknowledge it. Files rotate at 64 MiB or at the UTC day
//! boundary. Retention removes files older than 90 days and then the oldest while the total
//! exceeds 1 GiB, never the current file.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use a2a_types::proto::a2abus::AuditEvent;
use chrono::{DateTime, TimeDelta, Utc};
use prost::Message;
use serde::Deserialize;

use crate::canon::{CanonError, canonical_string};
use crate::clock::{Clock, date_stamp};
use crate::fsutil;
use crate::gaplog::GapLog;

/// Rotation size (0013-018).
pub const SINK_FILE_MAX_BYTES: u64 = 64 * 1024 * 1024;
/// Age after which a sink file is removed (0013-021).
pub const SINK_RETAIN_DAYS: i64 = 90;
/// Total size above which the oldest sink files are removed (0013-021).
pub const SINK_RETAIN_BYTES: u64 = 1024 * 1024 * 1024;

const DEDUPE_WINDOW: usize = 65_536;

/// Sink failures. A failed write is never acknowledged to the caller.
#[derive(Debug, thiserror::Error)]
pub enum SinkError {
    /// File system failure, including a failed sync.
    #[error("sink i/o: {0}")]
    Io(#[from] io::Error),
    /// The payload is not a protobuf audit event.
    #[error("payload is not an audit event: {0}")]
    Decode(#[from] prost::DecodeError),
    /// The event could not be put in canonical form.
    #[error("event is not canonical JSON: {0}")]
    Canon(#[from] CanonError),
    /// The event could not be encoded as JSON.
    #[error("event encoding: {0}")]
    Encode(#[from] serde_json::Error),
}

/// A sequence range that was never received for one emitter (0013-013).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeqGap {
    /// Emitter whose sequence jumped.
    pub emitter: String,
    /// First missing sequence number.
    pub first: u64,
    /// Last missing sequence number.
    pub last: u64,
}

/// Result of writing one event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SinkOutcome {
    /// The line was written and synced. `gap` is set when the sequence jumped.
    Written {
        /// Missing range before this event, if any.
        gap: Option<SeqGap>,
    },
    /// The event was already written (same `event_id`, or `seq` not beyond the last written).
    Duplicate,
}

/// Consumer-side persistence of audit events.
pub trait AuditSink: Send {
    /// Writes one event. Returns only after the line is durable.
    fn write_event(&mut self, event: &AuditEvent) -> Result<SinkOutcome, SinkError>;

    /// Decodes a protobuf payload and writes it.
    fn write_payload(&mut self, payload: &[u8]) -> Result<SinkOutcome, SinkError> {
        let event = AuditEvent::decode(payload)?;
        self.write_event(&event)
    }
}

/// Sink size and retention settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SinkLimits {
    /// Rotate once a file would exceed this many bytes.
    pub max_file_bytes: u64,
    /// Remove files whose day is older than this many days.
    pub retain_days: i64,
    /// Remove the oldest files while the total exceeds this many bytes.
    pub retain_bytes: u64,
}

impl Default for SinkLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: SINK_FILE_MAX_BYTES,
            retain_days: SINK_RETAIN_DAYS,
            retain_bytes: SINK_RETAIN_BYTES,
        }
    }
}

#[derive(Debug)]
struct DayFile {
    day: String,
    index: u32,
    path: PathBuf,
    len: u64,
}

#[derive(Deserialize)]
struct Head {
    emitter: String,
    seq: String,
}

/// JSONL files under one directory, one line per event.
#[derive(Debug)]
pub struct JsonlSink {
    dir: PathBuf,
    clock: Arc<dyn Clock>,
    limits: SinkLimits,
    day: String,
    index: u32,
    path: PathBuf,
    len: u64,
    dirty: bool,
    current: Option<File>,
    last: BTreeMap<String, u64>,
    recent: VecDeque<String>,
    recent_set: HashSet<String>,
    gaps: Vec<SeqGap>,
    duplicates: u64,
}

impl JsonlSink {
    /// Opens the sink with the default limits.
    pub fn open(dir: &Path, clock: Arc<dyn Clock>) -> Result<Self, SinkError> {
        Self::open_with_limits(dir, clock, SinkLimits::default())
    }

    /// Opens the sink: scans existing files for the last sequence per emitter, repairs a torn
    /// tail, applies retention, and selects the file to append to.
    pub fn open_with_limits(
        dir: &Path,
        clock: Arc<dyn Clock>,
        limits: SinkLimits,
    ) -> Result<Self, SinkError> {
        fs::create_dir_all(dir)?;
        let mut files = list_files(dir)?;
        let mut last = BTreeMap::new();
        for file in &mut files {
            file.len = scan_file(&file.path, &mut last)?;
        }
        let now = clock.now();
        let day = date_stamp(now);
        let max_today = files
            .iter()
            .filter(|file| file.day == day)
            .map(|file| file.index)
            .max()
            .unwrap_or(0);
        let reuse = files
            .iter()
            .find(|file| file.day == day && file.index == max_today)
            .filter(|file| file.len < limits.max_file_bytes)
            .map(|file| (file.index, file.path.clone(), file.len));
        let (index, path, len) = match reuse {
            Some(found) => found,
            None => {
                let index = max_today + 1;
                (index, dir.join(file_name(&day, index)), 0)
            }
        };
        if !path.exists() {
            drop(fsutil::create_owner_only(&path)?);
            fsutil::sync_parent(&path)?;
        }
        apply_retention(dir, now, &path, limits)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            clock,
            limits,
            day,
            index,
            path,
            len,
            dirty: false,
            current: None,
            last,
            recent: VecDeque::new(),
            recent_set: HashSet::new(),
            gaps: Vec::new(),
            duplicates: 0,
        })
    }

    /// The file currently being appended to.
    pub fn current_path(&self) -> &Path {
        &self.path
    }

    /// Events skipped as duplicates since the sink was opened (0013-017).
    pub fn duplicates(&self) -> u64 {
        self.duplicates
    }

    /// Returns and clears the sequence gaps observed since the last call.
    pub fn take_gaps(&mut self) -> Vec<SeqGap> {
        std::mem::take(&mut self.gaps)
    }

    /// Writes a `warn` record to `log` for every gap observed since the last call (0013-051).
    /// Consumers call this after each batch. The gaps not yet written, including the one that
    /// failed, are kept for the next call, so a failed write loses nothing. Returns the number of
    /// records written.
    pub fn report_gaps(&mut self, log: &mut GapLog) -> io::Result<usize> {
        let mut gaps = self.take_gaps();
        let now = self.clock.now();
        let mut written = 0;
        while written < gaps.len() {
            if let Err(err) = log.warn_gap(&gaps[written], now) {
                let mut retained = gaps.split_off(written);
                retained.append(&mut self.gaps);
                self.gaps = retained;
                return Err(err);
            }
            written += 1;
        }
        Ok(written)
    }

    fn remember(&mut self, event_id: &str) {
        if self.recent_set.insert(event_id.to_owned()) {
            self.recent.push_back(event_id.to_owned());
        }
        while self.recent.len() > DEDUPE_WINDOW {
            if let Some(old) = self.recent.pop_front() {
                self.recent_set.remove(&old);
            }
        }
    }

    fn ensure_room(&mut self, line_len: u64) -> Result<(), SinkError> {
        let day = date_stamp(self.clock.now());
        if day != self.day || (self.len > 0 && self.len + line_len > self.limits.max_file_bytes) {
            self.rotate(day)?;
        }
        Ok(())
    }

    fn rotate(&mut self, day: String) -> Result<(), SinkError> {
        self.current = None;
        let index = list_files(&self.dir)?
            .iter()
            .filter(|file| file.day == day)
            .map(|file| file.index)
            .max()
            .unwrap_or(0)
            + 1;
        let path = self.dir.join(file_name(&day, index));
        drop(fsutil::create_owner_only(&path)?);
        fsutil::sync_parent(&path)?;
        self.day = day;
        self.index = index;
        self.path = path;
        self.len = 0;
        self.dirty = false;
        apply_retention(&self.dir, self.clock.now(), &self.path, self.limits)
    }

    fn writer(&mut self) -> Result<&mut File, SinkError> {
        if self.current.is_none() {
            if self.dirty {
                // A failed write may have left a partial line; cut back to the last good length.
                let file = fsutil::open_rw(&self.path)?;
                file.set_len(self.len)?;
                file.sync_data()?;
                self.dirty = false;
            }
            self.current = Some(fsutil::open_shared_append(&self.path)?);
        }
        self.current
            .as_mut()
            .ok_or_else(|| SinkError::Io(io::Error::other("sink file handle is unavailable")))
    }

    fn write_line(&mut self, bytes: &[u8]) -> Result<(), SinkError> {
        let result = {
            let file = self.writer()?;
            file.write_all(bytes).and_then(|()| file.sync_data())
        };
        match result {
            Ok(()) => {
                self.len += bytes.len() as u64;
                Ok(())
            }
            Err(err) => {
                self.current = None;
                self.dirty = true;
                Err(err.into())
            }
        }
    }
}

impl AuditSink for JsonlSink {
    fn write_event(&mut self, event: &AuditEvent) -> Result<SinkOutcome, SinkError> {
        if self.recent_set.contains(&event.event_id) {
            self.duplicates += 1;
            return Ok(SinkOutcome::Duplicate);
        }
        let last = self.last.get(&event.emitter).copied().unwrap_or(0);
        if event.seq <= last {
            self.duplicates += 1;
            return Ok(SinkOutcome::Duplicate);
        }
        let line = format!("{}\n", canonical_string(&serde_json::to_value(event)?)?);
        self.ensure_room(line.len() as u64)?;
        self.write_line(line.as_bytes())?;
        self.last.insert(event.emitter.clone(), event.seq);
        self.remember(&event.event_id);
        let gap = (event.seq > last + 1).then(|| SeqGap {
            emitter: event.emitter.clone(),
            first: last + 1,
            last: event.seq - 1,
        });
        if let Some(gap) = &gap {
            self.gaps.push(gap.clone());
        }
        Ok(SinkOutcome::Written { gap })
    }
}

fn file_name(day: &str, index: u32) -> String {
    format!("events-{day}-{index}.jsonl")
}

fn parse_name(name: &str) -> Option<(String, u32)> {
    let stem = name.strip_prefix("events-")?.strip_suffix(".jsonl")?;
    let (day, index) = stem.split_once('-')?;
    if day.len() != 8 || !day.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some((day.to_owned(), index.parse().ok()?))
}

fn list_files(dir: &Path) -> Result<Vec<DayFile>, SinkError> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some((day, index)) = name.to_str().and_then(parse_name) else {
            continue;
        };
        files.push(DayFile {
            day,
            index,
            path: entry.path(),
            len: entry.metadata()?.len(),
        });
    }
    files.sort_by(|a, b| a.day.cmp(&b.day).then(a.index.cmp(&b.index)));
    Ok(files)
}

/// Reads a sink file, records the highest sequence per emitter, and truncates a torn tail.
/// Returns the valid length.
fn scan_file(path: &Path, last: &mut BTreeMap<String, u64>) -> Result<u64, SinkError> {
    let bytes = fs::read(path)?;
    let valid = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    let (complete, _) = bytes.split_at(valid);
    for line in complete.split(|byte| *byte == b'\n') {
        let Ok(head) = serde_json::from_slice::<Head>(line) else {
            continue;
        };
        let Ok(seq) = head.seq.parse::<u64>() else {
            continue;
        };
        let slot = last.entry(head.emitter).or_insert(0);
        *slot = (*slot).max(seq);
    }
    if (valid as u64) < bytes.len() as u64 {
        let file = fsutil::open_rw(path)?;
        file.set_len(valid as u64)?;
        file.sync_data()?;
    }
    Ok(valid as u64)
}

fn apply_retention(
    dir: &Path,
    now: DateTime<Utc>,
    keep: &Path,
    limits: SinkLimits,
) -> Result<(), SinkError> {
    let cutoff = date_stamp(now - TimeDelta::days(limits.retain_days));
    let mut kept = Vec::new();
    for file in list_files(dir)? {
        if file.path != keep && file.day < cutoff {
            fs::remove_file(&file.path)?;
        } else {
            kept.push(file);
        }
    }
    let mut total: u64 = kept.iter().map(|file| file.len).sum();
    for file in &kept {
        if total <= limits.retain_bytes {
            break;
        }
        if file.path == keep {
            continue;
        }
        fs::remove_file(&file.path)?;
        total = total.saturating_sub(file.len);
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::time::Duration;

    use a2a_types::proto::a2abus::{AuditEventType, EmitterKind};
    use chrono::TimeZone;
    use uuid::Uuid;

    use super::*;
    use crate::clock::ManualClock;

    fn at(day: u32, hour: u32, minute: u32, second: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, day, hour, minute, second)
            .single()
            .unwrap()
    }

    fn event(emitter: &str, seq: u64) -> AuditEvent {
        AuditEvent {
            event_id: Uuid::now_v7().to_string(),
            time: "2026-05-01T12:00:00.000Z".to_owned(),
            seq,
            tenant: "tenant-a".to_owned(),
            emitter: emitter.to_owned(),
            emitter_kind: EmitterKind::Worker as i32,
            r#type: AuditEventType::ApprovalRequested as i32,
            prev_hash: "0".repeat(64),
            chain_hash: "a".repeat(64),
            ..Default::default()
        }
    }

    fn lines(path: &Path) -> Vec<String> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn sink_files(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn open(dir: &Path, clock: &ManualClock) -> JsonlSink {
        JsonlSink::open(dir, Arc::new(clock.clone())).unwrap()
    }

    #[test]
    fn req_0013_013_duplicate_delivery_writes_one_line() {
        let dir = tempfile::tempdir().unwrap();
        let clock = ManualClock::new(at(1, 12, 0, 0));
        let mut sink = open(dir.path(), &clock);
        let first = event("agent-a", 1);
        assert!(matches!(
            sink.write_event(&first).unwrap(),
            SinkOutcome::Written { gap: None }
        ));
        assert_eq!(sink.write_event(&first).unwrap(), SinkOutcome::Duplicate);
        assert_eq!(lines(sink.current_path()).len(), 1);
    }

    #[test]
    fn req_0013_013_missing_sequence_is_recorded_as_gap() {
        let dir = tempfile::tempdir().unwrap();
        let clock = ManualClock::new(at(1, 12, 0, 0));
        let mut sink = open(dir.path(), &clock);
        sink.write_event(&event("agent-a", 1)).unwrap();
        let outcome = sink.write_event(&event("agent-a", 4)).unwrap();
        let expected = SeqGap {
            emitter: "agent-a".to_owned(),
            first: 2,
            last: 3,
        };
        assert_eq!(
            outcome,
            SinkOutcome::Written {
                gap: Some(expected.clone())
            }
        );
        assert_eq!(sink.take_gaps(), vec![expected]);
        assert!(sink.take_gaps().is_empty());
    }

    /// 0013-051: a consumer-observed gap becomes one `warn` record under the logs dir. The sink
    /// writes no `AUDIT_GAP` line and the gap is not left pending.
    #[test]
    fn req_0013_051_consumer_gap_is_a_warn_record_not_an_audit_event() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let clock = ManualClock::new(at(1, 12, 0, 0));
        let mut sink = open(dir.path(), &clock);
        sink.write_event(&event("agent-a", 1)).unwrap();
        sink.write_event(&event("agent-a", 4)).unwrap();
        let before = lines(sink.current_path()).len();
        let mut log = GapLog::open(&state.path().join("logs")).unwrap();
        assert_eq!(sink.report_gaps(&mut log).unwrap(), 1);
        assert!(sink.take_gaps().is_empty());
        assert_eq!(lines(sink.current_path()).len(), before);
        let text = fs::read_to_string(log.path()).unwrap();
        assert!(!text.contains("AUDIT_GAP"));
        let record: serde_json::Value = serde_json::from_str(text.trim_end()).unwrap();
        assert_eq!(record["level"], "warn");
        assert_eq!(record["emitter"], "agent-a");
        assert_eq!(record["first"], "2");
        assert_eq!(record["last"], "3");
    }

    /// 0013-051: with no gap observed, `report_gaps` writes nothing to the log.
    #[test]
    fn req_0013_051_no_gap_writes_no_warning() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let clock = ManualClock::new(at(1, 12, 0, 0));
        let mut sink = open(dir.path(), &clock);
        sink.write_event(&event("agent-a", 1)).unwrap();
        sink.write_event(&event("agent-a", 2)).unwrap();
        let mut log = GapLog::open(&state.path().join("logs")).unwrap();
        assert_eq!(sink.report_gaps(&mut log).unwrap(), 0);
        assert_eq!(fs::read_to_string(log.path()).unwrap(), "");
    }

    #[test]
    fn req_0013_017_sequence_not_beyond_last_is_skipped_and_counted() {
        let dir = tempfile::tempdir().unwrap();
        let clock = ManualClock::new(at(1, 12, 0, 0));
        let mut sink = open(dir.path(), &clock);
        sink.write_event(&event("agent-a", 1)).unwrap();
        sink.write_event(&event("agent-a", 2)).unwrap();
        assert_eq!(
            sink.write_event(&event("agent-a", 2)).unwrap(),
            SinkOutcome::Duplicate
        );
        assert_eq!(
            sink.write_event(&event("agent-a", 1)).unwrap(),
            SinkOutcome::Duplicate
        );
        assert_eq!(sink.duplicates(), 2);
        assert_eq!(lines(sink.current_path()).len(), 2);
    }

    #[test]
    fn req_0013_017_restart_keeps_last_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let clock = ManualClock::new(at(1, 12, 0, 0));
        let mut sink = open(dir.path(), &clock);
        sink.write_event(&event("agent-a", 1)).unwrap();
        sink.write_event(&event("agent-a", 2)).unwrap();
        drop(sink);
        let mut reopened = open(dir.path(), &clock);
        assert_eq!(
            reopened.write_event(&event("agent-a", 2)).unwrap(),
            SinkOutcome::Duplicate
        );
        assert!(matches!(
            reopened.write_event(&event("agent-a", 3)).unwrap(),
            SinkOutcome::Written { gap: None }
        ));
    }

    #[test]
    fn req_0013_018_lines_are_canonical_json_in_dated_files() {
        let dir = tempfile::tempdir().unwrap();
        let clock = ManualClock::new(at(1, 12, 0, 0));
        let mut sink = open(dir.path(), &clock);
        let original = event("agent-a", 1);
        sink.write_event(&original).unwrap();
        assert_eq!(
            sink_files(dir.path()),
            vec!["events-20260501-1.jsonl".to_owned()]
        );
        let line = lines(sink.current_path()).remove(0);
        let expected = canonical_string(&serde_json::to_value(&original).unwrap()).unwrap();
        assert_eq!(line, expected);
        assert!(!line.contains(' '));
    }

    #[test]
    fn req_0013_018_rotates_when_file_would_exceed_limit() {
        let dir = tempfile::tempdir().unwrap();
        let clock = ManualClock::new(at(1, 12, 0, 0));
        let limits = SinkLimits {
            max_file_bytes: 700,
            ..SinkLimits::default()
        };
        let mut sink =
            JsonlSink::open_with_limits(dir.path(), Arc::new(clock.clone()), limits).unwrap();
        sink.write_event(&event("agent-a", 1)).unwrap();
        sink.write_event(&event("agent-a", 2)).unwrap();
        assert_eq!(
            sink_files(dir.path()),
            vec![
                "events-20260501-1.jsonl".to_owned(),
                "events-20260501-2.jsonl".to_owned()
            ]
        );
        assert_eq!(lines(&dir.path().join("events-20260501-1.jsonl")).len(), 1);
    }

    #[test]
    fn req_0013_018_rotates_at_utc_midnight() {
        let dir = tempfile::tempdir().unwrap();
        let clock = ManualClock::new(at(1, 23, 59, 59));
        let mut sink = open(dir.path(), &clock);
        sink.write_event(&event("agent-a", 1)).unwrap();
        clock.advance(Duration::from_secs(2));
        sink.write_event(&event("agent-a", 2)).unwrap();
        assert_eq!(
            sink_files(dir.path()),
            vec![
                "events-20260501-1.jsonl".to_owned(),
                "events-20260502-1.jsonl".to_owned()
            ]
        );
    }

    #[test]
    fn req_0013_019_acknowledged_line_survives_restart_without_close() {
        let dir = tempfile::tempdir().unwrap();
        let clock = ManualClock::new(at(1, 12, 0, 0));
        let mut sink = open(dir.path(), &clock);
        sink.write_event(&event("agent-a", 1)).unwrap();
        std::mem::forget(sink);
        let reopened = open(dir.path(), &clock);
        assert_eq!(lines(reopened.current_path()).len(), 1);
    }

    #[test]
    fn req_0013_019_torn_tail_is_repaired_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let clock = ManualClock::new(at(1, 12, 0, 0));
        let mut sink = open(dir.path(), &clock);
        sink.write_event(&event("agent-a", 1)).unwrap();
        let path = sink.current_path().to_path_buf();
        drop(sink);
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        io::Write::write_all(&mut file, b"{\"emitter\":\"agent-a\",\"seq\":\"2\"").unwrap();
        drop(file);
        let mut reopened = open(dir.path(), &clock);
        assert_eq!(lines(&path).len(), 1);
        assert!(matches!(
            reopened.write_event(&event("agent-a", 2)).unwrap(),
            SinkOutcome::Written { gap: None }
        ));
        assert_eq!(lines(&path).len(), 2);
    }

    #[test]
    fn req_0013_016_failed_write_is_not_acknowledged() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("audit");
        let clock = ManualClock::new(at(1, 12, 0, 0));
        let mut sink = open(&root, &clock);
        sink.write_event(&event("agent-a", 1)).unwrap();
        fs::remove_dir_all(&root).unwrap();
        clock.advance(Duration::from_secs(86_400));
        assert!(sink.write_event(&event("agent-a", 2)).is_err());
        fs::create_dir_all(&root).unwrap();
        let retried = event("agent-a", 2);
        assert!(matches!(
            sink.write_event(&retried).unwrap(),
            SinkOutcome::Written { gap: None }
        ));
    }

    #[test]
    fn req_0013_020_file_is_readable_while_sink_holds_it() {
        let dir = tempfile::tempdir().unwrap();
        let clock = ManualClock::new(at(1, 12, 0, 0));
        let mut sink = open(dir.path(), &clock);
        sink.write_event(&event("agent-a", 1)).unwrap();
        assert_eq!(lines(sink.current_path()).len(), 1);
        sink.write_event(&event("agent-a", 2)).unwrap();
        assert_eq!(lines(sink.current_path()).len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn req_0013_020_sink_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let clock = ManualClock::new(at(1, 12, 0, 0));
        let mut sink = open(dir.path(), &clock);
        sink.write_event(&event("agent-a", 1)).unwrap();
        let mode = fs::metadata(sink.current_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn req_0013_021_retention_removes_old_files_and_keeps_current() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("events-20260101-1.jsonl"), b"x").unwrap();
        fs::write(dir.path().join("events-20260420-1.jsonl"), b"x").unwrap();
        let clock = ManualClock::new(at(1, 12, 0, 0));
        let sink = open(dir.path(), &clock);
        assert!(!dir.path().join("events-20260101-1.jsonl").exists());
        assert!(dir.path().join("events-20260420-1.jsonl").exists());
        assert!(sink.current_path().exists());
    }

    #[test]
    fn req_0013_021_retention_removes_oldest_while_over_size() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("events-20260420-1.jsonl");
        let newer = dir.path().join("events-20260425-1.jsonl");
        let mut body = vec![b'x'; 399];
        body.push(b'\n');
        fs::write(&old, &body).unwrap();
        fs::write(&newer, &body).unwrap();
        let clock = ManualClock::new(at(1, 12, 0, 0));
        let limits = SinkLimits {
            retain_bytes: 500,
            ..SinkLimits::default()
        };
        let _sink = JsonlSink::open_with_limits(dir.path(), Arc::new(clock), limits).unwrap();
        assert!(!old.exists());
        assert!(newer.exists());
    }

    #[test]
    fn req_0013_021_retention_never_removes_current_file() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("events-20260501-1.jsonl");
        fs::write(&current, vec![b'x'; 400]).unwrap();
        let clock = ManualClock::new(at(1, 12, 0, 0));
        let limits = SinkLimits {
            retain_bytes: 10,
            ..SinkLimits::default()
        };
        let sink = JsonlSink::open_with_limits(dir.path(), Arc::new(clock), limits).unwrap();
        assert_eq!(sink.current_path(), current.as_path());
        assert!(current.exists());
    }

    #[test]
    fn req_0013_016_payload_round_trip_through_trait() {
        let dir = tempfile::tempdir().unwrap();
        let clock = ManualClock::new(at(1, 12, 0, 0));
        let mut sink = open(dir.path(), &clock);
        let payload = event("agent-a", 1).encode_to_vec();
        assert!(matches!(
            sink.write_payload(&payload).unwrap(),
            SinkOutcome::Written { gap: None }
        ));
        assert!(sink.write_payload(b"\xff\xff").is_err());
    }
}
