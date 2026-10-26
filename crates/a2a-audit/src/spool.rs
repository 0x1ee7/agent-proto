//! Per-emitter spool (RFC-0013 0013-003, 0013-014, 0013-015, 0013-019). Each event is written
//! and synced here before it is published; the record is removed only after the publish is
//! acknowledged. The file is a sequence of frames `<crc8> <json>\n`, where `crc8` is the first
//! eight hex digits of sha256 over the JSON. A torn final frame is dropped at open. Any other
//! damaged frame is reported, never skipped.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::canon::sha256_hex;
use crate::chain::GENESIS_HASH;
use crate::fsutil;

/// Default spool size cap (0013-015).
pub const SPOOL_CAP_BYTES: u64 = 64 * 1024 * 1024;

/// Spool failures.
#[derive(Debug, thiserror::Error)]
pub enum SpoolError {
    /// File system failure.
    #[error("spool i/o: {0}")]
    Io(#[from] io::Error),
    /// A record could not be encoded.
    #[error("spool encoding: {0}")]
    Encoding(#[from] serde_json::Error),
    /// A complete line before the tail failed its checksum or did not parse (1-based line).
    #[error("spool line {line} is corrupt")]
    Corrupt {
        /// Line number, starting at 1.
        line: u64,
    },
    /// The record reached the file but `sync_data` failed, so it may not be durable.
    #[error("spool sync failed: {0}")]
    Sync(io::Error),
    /// One record is larger than the whole cap.
    #[error("spool record of {frame} bytes exceeds the cap of {cap} bytes")]
    Oversized {
        /// Record size in bytes.
        frame: u64,
        /// Configured cap in bytes.
        cap: u64,
    },
}

/// A spooled event awaiting publish.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// Per-emitter sequence number.
    pub seq: u64,
    /// Chain hash of the event.
    pub hash: String,
    /// Canonical event JSON.
    pub event: Value,
}

/// What to do when the spool is full (0013-015).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overflow {
    /// Drop the oldest pending events and record a gap.
    Discard,
    /// Leave the spool unchanged and report [`AppendOutcome::Full`].
    Refuse,
}

/// Result of [`Spool::append`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendOutcome {
    /// The record is spooled. `discarded` oldest events were dropped to make room.
    Written {
        /// Events dropped by this append.
        discarded: u64,
    },
    /// The spool is full and the overflow policy is [`Overflow::Refuse`].
    Full,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "k")]
enum Record {
    E {
        seq: u64,
        hash: String,
        event: Value,
    },
    A {
        seq: u64,
    },
    P {
        first: u64,
        last: u64,
    },
    G {
        first: u64,
        last: u64,
    },
    H {
        seq: u64,
        hash: String,
    },
}

#[derive(Debug)]
struct Queued {
    entry: Entry,
    frame_len: u64,
}

#[derive(Debug)]
struct Gap {
    first: u64,
    last: u64,
    frame_len: u64,
}

/// The spool of one emitter.
#[derive(Debug)]
pub struct Spool {
    path: PathBuf,
    cap: u64,
    file: Option<File>,
    file_len: u64,
    live_bytes: u64,
    queue: VecDeque<Queued>,
    gaps: VecDeque<Gap>,
    hw_seq: u64,
    hw_hash: String,
}

impl Spool {
    /// Opens or creates the spool at `path`, repairing a torn tail and replaying the records.
    /// The parent directory must exist.
    pub fn open(path: &Path, cap: u64) -> Result<Self, SpoolError> {
        let (bytes, existed) = match std::fs::read(path) {
            Ok(bytes) => (bytes, true),
            Err(err) if err.kind() == io::ErrorKind::NotFound => (Vec::new(), false),
            Err(err) => return Err(err.into()),
        };
        let mut spool = Self {
            path: path.to_path_buf(),
            cap: cap.max(1),
            file: None,
            file_len: 0,
            live_bytes: 0,
            queue: VecDeque::new(),
            gaps: VecDeque::new(),
            hw_seq: 0,
            hw_hash: GENESIS_HASH.to_owned(),
        };
        let valid = spool.load(&bytes)?;
        if !existed {
            drop(fsutil::create_owner_only(path)?);
        }
        let file = fsutil::open_rw(path)?;
        if valid < bytes.len() as u64 {
            file.set_len(valid)?;
            file.sync_data()?;
        }
        spool.file = Some(file);
        spool.file_len = valid;
        Ok(spool)
    }

    /// The oldest pending event, if any.
    pub fn front(&self) -> Option<Entry> {
        self.queue.front().map(|queued| queued.entry.clone())
    }

    /// Number of pending (not yet acknowledged) events.
    pub fn pending(&self) -> usize {
        self.queue.len()
    }

    /// Highest sequence number ever spooled and its chain hash (survives discards and restarts).
    pub fn high_water(&self) -> (u64, &str) {
        (self.hw_seq, &self.hw_hash)
    }

    /// The oldest gap that has not yet been reported as an AUDIT_GAP event.
    pub fn first_gap(&self) -> Option<(u64, u64)> {
        self.gaps.front().map(|gap| (gap.first, gap.last))
    }

    /// Marks the oldest gap as reported (0013-015).
    pub fn clear_gap(&mut self) -> Result<(), SpoolError> {
        let (first, last) = match self.gaps.front() {
            Some(gap) => (gap.first, gap.last),
            None => return Ok(()),
        };
        let frame = frame(&Record::G { first, last })?;
        self.write_frame(&frame)?;
        if let Some(gap) = self.gaps.pop_front() {
            self.live_bytes = self.live_bytes.saturating_sub(gap.frame_len);
        }
        Ok(())
    }

    /// Current file size divided by the cap.
    pub fn usage_ratio(&self) -> f64 {
        self.file_len as f64 / self.cap as f64
    }

    /// Spools `entry` and syncs it. Returns [`AppendOutcome::Full`] only under
    /// [`Overflow::Refuse`].
    pub fn append(
        &mut self,
        entry: Entry,
        overflow: Overflow,
    ) -> Result<AppendOutcome, SpoolError> {
        let record = Record::E {
            seq: entry.seq,
            hash: entry.hash.clone(),
            event: entry.event.clone(),
        };
        let bytes = frame(&record)?;
        let frame_len = bytes.len() as u64;
        if frame_len > self.cap {
            return Err(SpoolError::Oversized {
                frame: frame_len,
                cap: self.cap,
            });
        }
        let mut discarded = 0;
        if self.projected_len(frame_len)? > self.cap {
            match overflow {
                Overflow::Refuse => return Ok(AppendOutcome::Full),
                Overflow::Discard => {
                    discarded = self.discard_oldest(frame_len)?;
                    if self.projected_len(frame_len)? > self.cap {
                        return Ok(AppendOutcome::Full);
                    }
                }
            }
        }
        if self.file_len + frame_len > self.cap || self.should_compact() {
            self.compact()?;
        }
        self.write_frame(&bytes)?;
        if entry.seq >= self.hw_seq {
            self.hw_seq = entry.seq;
            self.hw_hash = entry.hash.clone();
        }
        self.live_bytes += frame_len;
        self.queue.push_back(Queued { entry, frame_len });
        self.file_mut()?.sync_data().map_err(SpoolError::Sync)?;
        Ok(AppendOutcome::Written { discarded })
    }

    /// Records that the event with `seq` was published and acknowledged. The record is removed
    /// from the file at the next compaction. Unknown sequence numbers are ignored.
    pub fn ack(&mut self, seq: u64) -> Result<(), SpoolError> {
        let Some(pos) = self.queue.iter().position(|queued| queued.entry.seq == seq) else {
            return Ok(());
        };
        let bytes = frame(&Record::A { seq })?;
        self.write_frame(&bytes)?;
        if let Some(done) = self.queue.remove(pos) {
            self.live_bytes = self.live_bytes.saturating_sub(done.frame_len);
        }
        Ok(())
    }

    fn load(&mut self, bytes: &[u8]) -> Result<u64, SpoolError> {
        let mut valid = 0usize;
        for (index, chunk) in bytes.split_inclusive(|byte| *byte == b'\n').enumerate() {
            // A final piece without its newline is the torn tail of an interrupted write.
            let Some(body) = chunk.strip_suffix(b"\n") else {
                break;
            };
            let line = index as u64 + 1;
            let record = decode(body).ok_or(SpoolError::Corrupt { line })?;
            let frame_len = chunk.len() as u64;
            match record {
                Record::E { seq, hash, event } => {
                    self.observe(seq, &hash);
                    self.queue.push_back(Queued {
                        entry: Entry { seq, hash, event },
                        frame_len,
                    });
                }
                Record::A { seq } => {
                    if let Some(pos) = self.queue.iter().position(|queued| queued.entry.seq == seq)
                    {
                        let _ = self.queue.remove(pos);
                    }
                }
                Record::P { first, last } => self.gaps.push_back(Gap {
                    first,
                    last,
                    frame_len,
                }),
                Record::G { first, last } => {
                    if let Some(pos) = self
                        .gaps
                        .iter()
                        .position(|gap| gap.first == first && gap.last == last)
                    {
                        let _ = self.gaps.remove(pos);
                    }
                }
                Record::H { seq, hash } => self.observe(seq, &hash),
            }
            valid += chunk.len();
        }
        self.live_bytes = self
            .queue
            .iter()
            .map(|queued| queued.frame_len)
            .chain(self.gaps.iter().map(|gap| gap.frame_len))
            .sum();
        Ok(valid as u64)
    }

    fn observe(&mut self, seq: u64, hash: &str) {
        if seq >= self.hw_seq {
            self.hw_seq = seq;
            self.hw_hash = hash.to_owned();
        }
    }

    fn should_compact(&self) -> bool {
        self.file_len > self.cap / 2
            && self.file_len.saturating_sub(self.live_bytes) > self.live_bytes
    }

    /// Bytes the spool occupies once compacted, with `incoming` more bytes appended: the live
    /// records plus the high-water header that compaction writes.
    fn projected_len(&self, incoming: u64) -> Result<u64, SpoolError> {
        let header = if self.hw_seq > 0 {
            frame(&Record::H {
                seq: self.hw_seq,
                hash: self.hw_hash.clone(),
            })?
            .len() as u64
        } else {
            0
        };
        Ok(self.live_bytes + header + incoming)
    }

    /// Drops the oldest pending events until `incoming` more bytes fit under 7/8 of the cap,
    /// records one gap over the dropped sequence numbers, and compacts.
    fn discard_oldest(&mut self, incoming: u64) -> Result<u64, SpoolError> {
        let target = self.cap / 8 * 7;
        let mut discarded = 0u64;
        let mut range: Option<(u64, u64)> = None;
        while self.projected_len(incoming)? > target {
            let Some(old) = self.queue.pop_front() else {
                break;
            };
            self.live_bytes = self.live_bytes.saturating_sub(old.frame_len);
            discarded += 1;
            range = Some(match range {
                Some((first, _)) => (first, old.entry.seq),
                None => (old.entry.seq, old.entry.seq),
            });
        }
        if let Some((first, last)) = range {
            match self.gaps.back_mut().filter(|gap| gap.last + 1 == first) {
                Some(gap) => {
                    self.live_bytes = self.live_bytes.saturating_sub(gap.frame_len);
                    gap.last = last;
                    gap.frame_len = frame(&Record::P {
                        first: gap.first,
                        last,
                    })?
                    .len() as u64;
                    self.live_bytes += gap.frame_len;
                }
                None => {
                    let frame_len = frame(&Record::P { first, last })?.len() as u64;
                    self.gaps.push_back(Gap {
                        first,
                        last,
                        frame_len,
                    });
                    self.live_bytes += frame_len;
                }
            }
        }
        self.compact()?;
        Ok(discarded)
    }

    /// Rewrites the file with only the live records (high water, gaps, pending events), through
    /// a temporary file and a rename.
    fn compact(&mut self) -> Result<(), SpoolError> {
        let mut bytes = Vec::new();
        if self.hw_seq > 0 {
            bytes.extend(frame(&Record::H {
                seq: self.hw_seq,
                hash: self.hw_hash.clone(),
            })?);
        }
        let mut gap_frames = Vec::with_capacity(self.gaps.len());
        for gap in &self.gaps {
            let encoded = frame(&Record::P {
                first: gap.first,
                last: gap.last,
            })?;
            gap_frames.push(encoded.len() as u64);
            bytes.extend(encoded);
        }
        let mut entry_frames = Vec::with_capacity(self.queue.len());
        for queued in &self.queue {
            let encoded = frame(&Record::E {
                seq: queued.entry.seq,
                hash: queued.entry.hash.clone(),
                event: queued.entry.event.clone(),
            })?;
            entry_frames.push(encoded.len() as u64);
            bytes.extend(encoded);
        }
        let tmp = tmp_path(&self.path);
        let _ = std::fs::remove_file(&tmp);
        {
            let mut out = fsutil::create_owner_only(&tmp)?;
            out.write_all(&bytes)?;
            out.sync_data()?;
        }
        // The old handle is closed before the rename so Windows can replace the file.
        self.file = None;
        let renamed = std::fs::rename(&tmp, &self.path);
        self.file = Some(fsutil::open_rw(&self.path)?);
        renamed?;
        fsutil::sync_parent(&self.path)?;
        for (gap, len) in self.gaps.iter_mut().zip(gap_frames) {
            gap.frame_len = len;
        }
        for (queued, len) in self.queue.iter_mut().zip(entry_frames) {
            queued.frame_len = len;
        }
        self.live_bytes = self
            .queue
            .iter()
            .map(|queued| queued.frame_len)
            .chain(self.gaps.iter().map(|gap| gap.frame_len))
            .sum();
        self.file_len = bytes.len() as u64;
        Ok(())
    }

    fn file_mut(&mut self) -> Result<&mut File, SpoolError> {
        self.file
            .as_mut()
            .ok_or_else(|| SpoolError::Io(io::Error::other("spool file handle is unavailable")))
    }

    fn write_frame(&mut self, bytes: &[u8]) -> Result<(), SpoolError> {
        let at = self.file_len;
        let file = self.file_mut()?;
        let result = file
            .seek(SeekFrom::Start(at))
            .and_then(|_| file.write_all(bytes));
        if let Err(err) = result {
            // Roll back a partial write so the next frame starts at a clean boundary.
            let _ = file.set_len(at);
            return Err(err.into());
        }
        self.file_len += bytes.len() as u64;
        Ok(())
    }
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".tmp");
    PathBuf::from(name)
}

fn frame(record: &Record) -> Result<Vec<u8>, SpoolError> {
    let json = serde_json::to_string(record)?;
    let crc: String = sha256_hex(json.as_bytes()).chars().take(8).collect();
    Ok(format!("{crc} {json}\n").into_bytes())
}

fn decode(body: &[u8]) -> Option<Record> {
    let text = std::str::from_utf8(body).ok()?;
    let (crc, json) = text.split_once(' ')?;
    let expected: String = sha256_hex(json.as_bytes()).chars().take(8).collect();
    if crc != expected {
        return None;
    }
    serde_json::from_str(json).ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use serde_json::json;

    use super::*;

    const BIG: usize = 200;

    fn entry(seq: u64) -> Entry {
        Entry {
            seq,
            hash: format!("{seq:064x}"),
            event: json!({"seq": seq.to_string(), "pad": "x".repeat(BIG)}),
        }
    }

    fn open(dir: &tempfile::TempDir, cap: u64) -> Spool {
        Spool::open(&dir.path().join("emitter.jsonl"), cap).unwrap()
    }

    fn written(outcome: AppendOutcome) -> u64 {
        match outcome {
            AppendOutcome::Written { discarded } => discarded,
            AppendOutcome::Full => u64::MAX,
        }
    }

    #[test]
    fn req_0013_003_reopen_continues_from_high_water() {
        let dir = tempfile::tempdir().unwrap();
        let mut spool = open(&dir, SPOOL_CAP_BYTES);
        for seq in 1..=3 {
            written(spool.append(entry(seq), Overflow::Discard).unwrap());
        }
        for seq in 1..=3 {
            spool.ack(seq).unwrap();
        }
        drop(spool);
        let reopened = open(&dir, SPOOL_CAP_BYTES);
        assert_eq!(reopened.pending(), 0);
        let (seq, hash) = reopened.high_water();
        assert_eq!(seq, 3);
        assert_eq!(hash, entry(3).hash);
    }

    #[test]
    fn req_0013_014_event_survives_restart_until_acked() {
        let dir = tempfile::tempdir().unwrap();
        let mut spool = open(&dir, SPOOL_CAP_BYTES);
        spool.append(entry(1), Overflow::Discard).unwrap();
        drop(spool);
        let mut reopened = open(&dir, SPOOL_CAP_BYTES);
        assert_eq!(reopened.front().map(|e| e.seq), Some(1));
        reopened.ack(1).unwrap();
        assert!(reopened.front().is_none());
        drop(reopened);
        let mut compacted = open(&dir, SPOOL_CAP_BYTES);
        compacted.compact().unwrap();
        let text = std::fs::read_to_string(dir.path().join("emitter.jsonl")).unwrap();
        assert!(
            !text.contains("\"k\":\"E\""),
            "acked event must be removed: {text}"
        );
        assert!(
            text.contains("\"k\":\"H\""),
            "high water must be kept: {text}"
        );
    }

    #[test]
    fn req_0013_015_overflow_discards_oldest_and_records_one_gap() {
        let dir = tempfile::tempdir().unwrap();
        let mut spool = open(&dir, 2000);
        let mut discarded = 0;
        for seq in 1..=20 {
            discarded += written(spool.append(entry(seq), Overflow::Discard).unwrap());
        }
        assert!(discarded > 0);
        let front = spool.front().map(|e| e.seq).unwrap_or(0);
        assert_eq!(front, discarded + 1, "oldest events are the ones dropped");
        assert_eq!(spool.first_gap(), Some((1, discarded)));
        assert!(spool.usage_ratio() <= 1.0);
        assert_eq!(spool.high_water().0, 20);
    }

    #[test]
    fn req_0013_015_refuse_leaves_spool_unchanged_when_full() {
        let dir = tempfile::tempdir().unwrap();
        let mut spool = open(&dir, 1000);
        let mut outcome = AppendOutcome::Written { discarded: 0 };
        let mut seq = 0;
        while matches!(outcome, AppendOutcome::Written { .. }) {
            seq += 1;
            outcome = spool.append(entry(seq), Overflow::Refuse).unwrap();
        }
        assert_eq!(outcome, AppendOutcome::Full);
        assert_eq!(spool.first_gap(), None);
        assert_eq!(spool.front().map(|e| e.seq), Some(1));
        let size = std::fs::metadata(dir.path().join("emitter.jsonl"))
            .unwrap()
            .len();
        assert!(size <= 1000);
    }

    #[test]
    fn req_0013_015_cleared_gap_is_not_reported_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let mut spool = open(&dir, 2000);
        for seq in 1..=20 {
            spool.append(entry(seq), Overflow::Discard).unwrap();
        }
        assert!(spool.first_gap().is_some());
        spool.clear_gap().unwrap();
        drop(spool);
        let reopened = open(&dir, 2000);
        assert_eq!(reopened.first_gap(), None);
    }

    #[test]
    fn req_0013_015_record_larger_than_cap_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut spool = open(&dir, 100);
        let err = spool.append(entry(1), Overflow::Discard).err();
        assert!(matches!(err, Some(SpoolError::Oversized { cap: 100, .. })));
    }

    #[test]
    fn req_0013_019_torn_tail_is_dropped_and_complete_records_survive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("emitter.jsonl");
        let mut spool = open(&dir, SPOOL_CAP_BYTES);
        spool.append(entry(1), Overflow::Discard).unwrap();
        spool.append(entry(2), Overflow::Discard).unwrap();
        drop(spool);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"0badbad0 {\"k\":\"E\",\"seq\":3").unwrap();
        drop(file);
        let reopened = open(&dir, SPOOL_CAP_BYTES);
        assert_eq!(reopened.pending(), 2);
        assert_eq!(reopened.high_water().0, 2);
        let text = std::fs::read(&path).unwrap();
        assert_eq!(text.last(), Some(&b'\n'));
    }

    #[test]
    fn req_0013_019_damaged_frame_before_tail_is_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("emitter.jsonl");
        let mut spool = open(&dir, SPOOL_CAP_BYTES);
        spool.append(entry(1), Overflow::Discard).unwrap();
        drop(spool);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"00000000 {\"k\":\"A\",\"seq\":1}\n")
            .unwrap();
        file.write_all(b"deadbeef {}\n").unwrap();
        drop(file);
        let err = Spool::open(&path, SPOOL_CAP_BYTES).err();
        assert!(
            matches!(err, Some(SpoolError::Corrupt { line: 2 })),
            "{err:?}"
        );
    }

    #[test]
    fn req_0013_019_checksum_mismatch_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("emitter.jsonl");
        let mut spool = open(&dir, SPOOL_CAP_BYTES);
        spool.append(entry(1), Overflow::Discard).unwrap();
        drop(spool);
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, text.replacen("\"seq\":\"1\"", "\"seq\":\"9\"", 1)).unwrap();
        let err = Spool::open(&path, SPOOL_CAP_BYTES).err();
        assert!(matches!(err, Some(SpoolError::Corrupt { line: 1 })));
    }

    #[cfg(unix)]
    #[test]
    fn req_0013_020_spool_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let spool = open(&dir, SPOOL_CAP_BYTES);
        drop(spool);
        let mode = std::fs::metadata(dir.path().join("emitter.jsonl"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}
