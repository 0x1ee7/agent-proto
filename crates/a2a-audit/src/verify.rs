//! Offline verification of sink files (RFC-0013 0013-026, 0013-027, 0013-028, 0013-029).
//!
//! The verifier recomputes every chain hash, checks linkage and sequence contiguity per
//! emitter, verifies checkpoint signatures, and cross-checks checkpoint heads against the
//! events they name. It reports the first break by file order and line. A consistent rewrite
//! of the whole chain, with no surviving checkpoint covering it, is not detectable (0013-029).

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

use a2a_types::proto::a2abus::AuditEventType;
use serde_json::Value;

use crate::canon::canonical_string;
use crate::chain::{self, CheckpointVerifier, GENESIS_HASH};

/// Verification succeeded.
pub const EXIT_VERIFIED: i32 = 0;
/// A break was found.
pub const EXIT_BROKEN: i32 = 1;
/// Usage error (reported by the CLI, which owns argument parsing).
pub const EXIT_USAGE: i32 = 2;

/// Kind of break found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakKind {
    /// Line is not a JSON object with the required envelope fields.
    Malformed,
    /// Line bytes are not JCS canonical JSON.
    NotCanonical,
    /// Stored `chainHash` does not match the recomputed hash.
    ChainHash,
    /// `prevHash` does not match the previous event's `chainHash`.
    Link,
    /// Sequence number is not beyond the last one seen for the emitter.
    SeqOrder,
    /// A sequence range is missing and no AUDIT_GAP covers it.
    UnexplainedGap,
    /// A checkpoint names an emitter head whose hash differs from the stored event.
    CheckpointMismatch,
    /// A checkpoint signature does not verify.
    BadSignature,
    /// The final line has no terminating newline.
    TruncatedLine,
}

/// The first break found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Break {
    /// File containing the break.
    pub file: PathBuf,
    /// One-based line number.
    pub line: u64,
    /// What is wrong.
    pub kind: BreakKind,
}

/// Verification result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerifyReport {
    /// Lines examined.
    pub lines: u64,
    /// Checkpoints examined.
    pub checkpoints: u64,
    /// First break by file order, then line.
    pub first_break: Option<Break>,
}

impl VerifyReport {
    /// Whether no break was found.
    pub fn is_ok(&self) -> bool {
        self.first_break.is_none()
    }

    /// Process exit code for this report.
    pub fn exit_code(&self) -> i32 {
        if self.is_ok() {
            EXIT_VERIFIED
        } else {
            EXIT_BROKEN
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Position {
    file: usize,
    line: u64,
}

#[derive(Debug, Default)]
struct Recorder {
    best: Option<(Position, BreakKind)>,
}

impl Recorder {
    /// Keeps the earliest break by (file index, line). Ties keep the first kind noted.
    fn note(&mut self, at: Position, kind: BreakKind) {
        match self.best {
            Some((best, _)) if best <= at => {}
            _ => self.best = Some((at, kind)),
        }
    }
}

#[derive(Debug, Clone)]
struct Last {
    seq: u64,
    hash: String,
}

#[derive(Debug, Clone)]
struct Missing {
    first: u64,
    last: u64,
    at: Position,
}

#[derive(Debug, Default)]
struct EmitterState {
    last: Option<Last>,
    missing: Vec<Missing>,
}

#[derive(Debug, Clone)]
struct HeadRef {
    at: Position,
    hash: String,
}

/// Verifies the given sink files in order. Each file holds one canonical JSON line per event;
/// the caller passes files in name order.
pub fn verify_paths(
    paths: &[PathBuf],
    verifier: &dyn CheckpointVerifier,
) -> io::Result<VerifyReport> {
    let mut recorder = Recorder::default();
    let mut emitters: BTreeMap<String, EmitterState> = BTreeMap::new();
    let mut wanted: BTreeMap<(String, u64), Vec<HeadRef>> = BTreeMap::new();
    let mut report = VerifyReport::default();

    for (index, path) in paths.iter().enumerate() {
        for_each_line(path, |line, body, complete| {
            report.lines += 1;
            let at = Position { file: index, line };
            if !complete {
                recorder.note(at, BreakKind::TruncatedLine);
                return;
            }
            let Ok(value) = serde_json::from_slice::<Value>(body) else {
                recorder.note(at, BreakKind::Malformed);
                return;
            };
            match canonical_string(&value) {
                Ok(text) if text.as_bytes() == body => {}
                _ => recorder.note(at, BreakKind::NotCanonical),
            }
            let (Some(emitter), Some(seq), Some(prev), Some(stored)) = (
                text(&value, "emitter"),
                text(&value, "seq").and_then(|seq| seq.parse::<u64>().ok()),
                text(&value, "prevHash"),
                text(&value, "chainHash"),
            ) else {
                recorder.note(at, BreakKind::Malformed);
                return;
            };
            if seq == 0 {
                recorder.note(at, BreakKind::Malformed);
                return;
            }
            match chain::preimage(&value) {
                Ok(preimage) => {
                    if chain::chain_hash(&preimage) != stored {
                        recorder.note(at, BreakKind::ChainHash);
                    }
                    if text(&value, "type") == Some(AuditEventType::AuditCheckpoint.as_str_name()) {
                        report.checkpoints += 1;
                        verify_checkpoint(
                            &value,
                            &preimage,
                            verifier,
                            at,
                            &mut recorder,
                            &mut wanted,
                        );
                    }
                }
                Err(_) => recorder.note(at, BreakKind::Malformed),
            }

            let state = emitters.entry(emitter.to_owned()).or_default();
            match state.last.clone() {
                None => {
                    if seq == 1 && prev != GENESIS_HASH {
                        recorder.note(at, BreakKind::Link);
                    }
                }
                Some(last) if seq <= last.seq => {
                    // A late arrival fills part of an earlier jump; it is reordered, not missing.
                    state.missing = remove_range(std::mem::take(&mut state.missing), seq, seq);
                    recorder.note(at, BreakKind::SeqOrder);
                }
                Some(last) if seq == last.seq + 1 => {
                    if prev != last.hash {
                        recorder.note(at, BreakKind::Link);
                    }
                }
                Some(last) => state.missing.push(Missing {
                    first: last.seq + 1,
                    last: seq - 1,
                    at,
                }),
            }
            if state.last.as_ref().is_none_or(|last| seq > last.seq) {
                state.last = Some(Last {
                    seq,
                    hash: stored.to_owned(),
                });
            }
            if text(&value, "type") == Some(AuditEventType::AuditGap.as_str_name()) {
                match gap_range(&value) {
                    Some((first, last)) => {
                        state.missing =
                            remove_range(std::mem::take(&mut state.missing), first, last);
                    }
                    None => recorder.note(at, BreakKind::Malformed),
                }
            }
        })?;
    }

    for state in emitters.values() {
        for missing in &state.missing {
            recorder.note(missing.at, BreakKind::UnexplainedGap);
        }
    }

    if !wanted.is_empty() {
        cross_check(paths, &wanted, &mut recorder)?;
    }

    report.first_break = recorder.best.map(|(at, kind)| Break {
        file: paths[at.file].clone(),
        line: at.line,
        kind,
    });
    Ok(report)
}

fn verify_checkpoint(
    value: &Value,
    preimage: &[u8],
    verifier: &dyn CheckpointVerifier,
    at: Position,
    recorder: &mut Recorder,
    wanted: &mut BTreeMap<(String, u64), Vec<HeadRef>>,
) {
    let signature = text(value, "signature").and_then(chain::decode_signature);
    match (text(value, "keyId"), signature) {
        (Some(key_id), Some(signature)) if verifier.verify(key_id, preimage, &signature) => {}
        _ => recorder.note(at, BreakKind::BadSignature),
    }
    let Some(emitters) = value
        .get("details")
        .and_then(|details| details.get("emitters"))
        .and_then(Value::as_object)
    else {
        recorder.note(at, BreakKind::Malformed);
        return;
    };
    for (name, head) in emitters {
        let seq = text(head, "seq").and_then(|seq| seq.parse::<u64>().ok());
        match (seq, text(head, "chain_hash")) {
            (Some(seq), Some(hash)) => {
                wanted
                    .entry((name.clone(), seq))
                    .or_default()
                    .push(HeadRef {
                        at,
                        hash: hash.to_owned(),
                    })
            }
            _ => recorder.note(at, BreakKind::Malformed),
        }
    }
}

/// Second pass: each checkpoint head must equal the stored chain hash of the event it names.
/// Heads whose event is absent (a gap, or beyond the file set) are not checkable here.
fn cross_check(
    paths: &[PathBuf],
    wanted: &BTreeMap<(String, u64), Vec<HeadRef>>,
    recorder: &mut Recorder,
) -> io::Result<()> {
    for path in paths {
        for_each_line(path, |_, body, complete| {
            if !complete {
                return;
            }
            let Ok(value) = serde_json::from_slice::<Value>(body) else {
                return;
            };
            let (Some(emitter), Some(seq), Some(stored)) = (
                text(&value, "emitter"),
                text(&value, "seq").and_then(|seq| seq.parse::<u64>().ok()),
                text(&value, "chainHash"),
            ) else {
                return;
            };
            if let Some(heads) = wanted.get(&(emitter.to_owned(), seq)) {
                for head in heads {
                    if head.hash != stored {
                        recorder.note(head.at, BreakKind::CheckpointMismatch);
                    }
                }
            }
        })?;
    }
    Ok(())
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// Reads `first_seq` and `last_seq` from an AUDIT_GAP event's details.
fn gap_range(value: &Value) -> Option<(u64, u64)> {
    let details = value.get("details")?;
    let first = text(details, "first_seq")?.parse::<u64>().ok()?;
    let last = text(details, "last_seq")?.parse::<u64>().ok()?;
    (first >= 1 && first <= last).then_some((first, last))
}

/// Removes `first..=last` from the missing ranges, splitting ranges that straddle it.
fn remove_range(missing: Vec<Missing>, first: u64, last: u64) -> Vec<Missing> {
    let mut kept = Vec::with_capacity(missing.len());
    for range in missing {
        if range.last < first || range.first > last {
            kept.push(range);
            continue;
        }
        if range.first < first {
            kept.push(Missing {
                first: range.first,
                last: first - 1,
                at: range.at,
            });
        }
        if range.last > last {
            kept.push(Missing {
                first: last + 1,
                last: range.last,
                at: range.at,
            });
        }
    }
    kept
}

/// Calls `visit` for each line of `path` with its one-based number, the bytes without the
/// newline, and whether a newline terminated it.
fn for_each_line(path: &Path, mut visit: impl FnMut(u64, &[u8], bool)) -> io::Result<()> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut buf = Vec::new();
    let mut line = 0;
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        line += 1;
        let complete = buf.last() == Some(&b'\n');
        let body = if complete {
            &buf[..buf.len() - 1]
        } else {
            &buf[..]
        };
        visit(line, body, complete);
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::Arc;

    use a2a_types::ids::TaskId;
    use a2a_types::proto::a2abus::{AuditEventType, EmitterKind};
    use chrono::{TimeZone, Utc};
    use serde_json::json;

    use super::*;
    use crate::chain::CheckpointSigner;
    use crate::clock::ManualClock;
    use crate::emitter::{Emitter, EmitterIdentity, Sealed};
    use crate::event::EventBuilder;
    use crate::redact::Redactor;
    use crate::testkit::{DigestSigner, DigestVerifier};

    struct Fixture {
        dir: tempfile::TempDir,
        clock: ManualClock,
    }

    fn fixture() -> Fixture {
        Fixture {
            dir: tempfile::tempdir().unwrap(),
            clock: ManualClock::new(Utc.with_ymd_and_hms(2026, 5, 1, 12, 0, 0).single().unwrap()),
        }
    }

    fn worker(clock: &ManualClock) -> Emitter {
        Emitter::new(
            EmitterIdentity::new("tenant-a", "agent-a", EmitterKind::Worker).unwrap(),
            Arc::new(clock.clone()),
            Redactor::new(),
            None,
        )
    }

    fn supervisor(clock: &ManualClock) -> Emitter {
        Emitter::new(
            EmitterIdentity::new("tenant-a", "supervisor", EmitterKind::Supervisor).unwrap(),
            Arc::new(clock.clone()),
            Redactor::new(),
            Some(Arc::new(DigestSigner::new("key-1"))),
        )
    }

    fn approval(task: &TaskId, request: &str) -> EventBuilder {
        EventBuilder::new(AuditEventType::ApprovalRequested)
            .task(task.clone())
            .detail("request_id", json!(request))
    }

    fn checkpoint(seq: u64, hash: &str) -> EventBuilder {
        EventBuilder::new(AuditEventType::AuditCheckpoint)
            .detail(
                "emitters",
                json!({"agent-a": {"seq": seq.to_string(), "chain_hash": hash}}),
            )
            .detail("signed_at", json!("2026-05-01T12:00:00.000Z"))
    }

    /// Five worker events followed by a supervisor checkpoint covering seq 5.
    fn clean_stream(f: &Fixture) -> Vec<Sealed> {
        let mut w = worker(&f.clock);
        let mut s = supervisor(&f.clock);
        let task = TaskId::generate();
        let mut out = Vec::new();
        for i in 1..=5 {
            out.push(w.seal(approval(&task, &format!("req-{i}"))).unwrap());
        }
        let head = out.last().unwrap().chain_hash.clone();
        out.push(s.seal(checkpoint(5, &head)).unwrap());
        out
    }

    fn write(dir: &Path, name: &str, sealed: &[Sealed]) -> PathBuf {
        let path = dir.join(name);
        let mut text = String::new();
        for item in sealed {
            text.push_str(&canonical_string(&item.value).unwrap());
            text.push('\n');
        }
        std::fs::write(&path, text).unwrap();
        path
    }

    fn lines_of(path: &Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn put(path: &Path, lines: &[String]) {
        let mut text = lines.join("\n");
        text.push('\n');
        std::fs::write(path, text).unwrap();
    }

    fn run(path: &Path) -> VerifyReport {
        verify_paths(&[path.to_path_buf()], &DigestVerifier::new("key-1")).unwrap()
    }

    fn kind(report: &VerifyReport) -> Option<(u64, BreakKind)> {
        report
            .first_break
            .as_ref()
            .map(|found| (found.line, found.kind))
    }

    #[test]
    fn req_0013_026_clean_chain_verifies() {
        let f = fixture();
        let path = write(f.dir.path(), "events-20260501-1.jsonl", &clean_stream(&f));
        let report = run(&path);
        assert!(report.is_ok(), "{report:?}");
        assert_eq!(report.lines, 6);
        assert_eq!(report.checkpoints, 1);
        assert_eq!(report.exit_code(), EXIT_VERIFIED);
    }

    #[test]
    fn req_0013_026_stream_starting_mid_sequence_is_accepted() {
        let f = fixture();
        let sealed = clean_stream(&f);
        let path = write(f.dir.path(), "events-20260501-1.jsonl", &sealed[1..]);
        assert!(run(&path).is_ok(), "{:?}", run(&path));
    }

    #[test]
    fn req_0013_026_first_seq_one_must_link_to_genesis() {
        let f = fixture();
        let sealed = clean_stream(&f);
        let mut lines = lines_of(&write(f.dir.path(), "events-20260501-1.jsonl", &sealed));
        let mut value: Value = serde_json::from_str(&lines[0]).unwrap();
        value["prevHash"] = json!("1".repeat(64));
        let preimage = chain::preimage(&value).unwrap();
        value["chainHash"] = json!(chain::chain_hash(&preimage));
        lines[0] = canonical_string(&value).unwrap();
        let path = f.dir.path().join("tampered.jsonl");
        put(&path, &lines);
        assert_eq!(kind(&run(&path)), Some((1, BreakKind::Link)));
    }

    #[test]
    fn req_0013_028_edited_line_is_first_break_by_line() {
        let f = fixture();
        let path = write(f.dir.path(), "events-20260501-1.jsonl", &clean_stream(&f));
        let mut lines = lines_of(&path);
        lines[2] = lines[2].replace("req-3", "req-9");
        put(&path, &lines);
        let report = run(&path);
        assert_eq!(kind(&report), Some((3, BreakKind::ChainHash)));
        assert_eq!(report.exit_code(), EXIT_BROKEN);
    }

    #[test]
    fn req_0013_028_deleted_line_is_unexplained_gap() {
        let f = fixture();
        let path = write(f.dir.path(), "events-20260501-1.jsonl", &clean_stream(&f));
        let mut lines = lines_of(&path);
        lines.remove(2);
        put(&path, &lines);
        assert_eq!(kind(&run(&path)), Some((3, BreakKind::UnexplainedGap)));
    }

    #[test]
    fn req_0013_028_reordered_lines_are_seq_order_break() {
        let f = fixture();
        let path = write(f.dir.path(), "events-20260501-1.jsonl", &clean_stream(&f));
        let mut lines = lines_of(&path);
        lines.swap(2, 3);
        put(&path, &lines);
        assert_eq!(kind(&run(&path)), Some((4, BreakKind::SeqOrder)));
    }

    #[test]
    fn req_0013_028_truncated_final_line_is_reported() {
        let f = fixture();
        let path = write(f.dir.path(), "events-20260501-1.jsonl", &clean_stream(&f));
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.pop();
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(kind(&run(&path)), Some((6, BreakKind::TruncatedLine)));
    }

    #[test]
    fn req_0013_028_malformed_line_is_reported() {
        let f = fixture();
        let path = write(f.dir.path(), "events-20260501-1.jsonl", &clean_stream(&f));
        let mut lines = lines_of(&path);
        lines[1] = "{not json".to_owned();
        put(&path, &lines);
        assert_eq!(kind(&run(&path)), Some((2, BreakKind::Malformed)));
    }

    #[test]
    fn req_0013_028_non_canonical_bytes_are_reported() {
        let f = fixture();
        let path = write(f.dir.path(), "events-20260501-1.jsonl", &clean_stream(&f));
        let mut lines = lines_of(&path);
        let value: Value = serde_json::from_str(&lines[0]).unwrap();
        lines[0] = serde_json::to_string_pretty(&value)
            .unwrap()
            .replace('\n', " ");
        put(&path, &lines);
        assert_eq!(kind(&run(&path)), Some((1, BreakKind::NotCanonical)));
    }

    #[test]
    fn req_0013_028_forged_checkpoint_signature_is_reported() {
        let f = fixture();
        let path = write(f.dir.path(), "events-20260501-1.jsonl", &clean_stream(&f));
        let forged = DigestSigner::new("key-2").sign(b"anything");
        let mut lines = lines_of(&path);
        let mut value: Value = serde_json::from_str(&lines[5]).unwrap();
        value["signature"] = json!(chain::encode_signature(&forged));
        lines[5] = canonical_string(&value).unwrap();
        put(&path, &lines);
        assert_eq!(kind(&run(&path)), Some((6, BreakKind::BadSignature)));
    }

    #[test]
    fn req_0013_027_wrong_verifier_key_fails_checkpoint() {
        let f = fixture();
        let path = write(f.dir.path(), "events-20260501-1.jsonl", &clean_stream(&f));
        let report = verify_paths(&[path], &DigestVerifier::new("key-2")).unwrap();
        assert_eq!(kind(&report), Some((6, BreakKind::BadSignature)));
    }

    #[test]
    fn req_0013_028_first_break_is_earliest_across_files() {
        let f = fixture();
        let sealed = clean_stream(&f);
        let first = write(f.dir.path(), "events-20260501-1.jsonl", &sealed[..3]);
        let second = write(f.dir.path(), "events-20260501-2.jsonl", &sealed[3..]);
        let mut lines = lines_of(&second);
        lines[0] = lines[0].replace("req-4", "req-8");
        put(&second, &lines);
        let report = verify_paths(&[first, second.clone()], &DigestVerifier::new("key-1")).unwrap();
        let found = report.first_break.unwrap();
        assert_eq!(found.file, second);
        assert_eq!((found.line, found.kind), (1, BreakKind::ChainHash));
    }

    #[test]
    fn req_0013_027_checkpoint_head_must_match_stored_event() {
        let f = fixture();
        let mut w = worker(&f.clock);
        let mut s = supervisor(&f.clock);
        let task = TaskId::generate();
        let first = w.seal(approval(&task, "req-1")).unwrap();
        let cp = s.seal(checkpoint(1, &"b".repeat(64))).unwrap();
        let path = write(f.dir.path(), "events-20260501-1.jsonl", &[first, cp]);
        assert_eq!(kind(&run(&path)), Some((2, BreakKind::CheckpointMismatch)));
    }

    #[test]
    fn req_0013_029_full_recomputed_rewrite_passes_without_checkpoint() {
        let f = fixture();
        let mut w = worker(&f.clock);
        let task = TaskId::generate();
        let rewritten: Vec<Sealed> = (1..=3)
            .map(|i| w.seal(approval(&task, &format!("forged-{i}"))).unwrap())
            .collect();
        let path = write(f.dir.path(), "events-20260501-1.jsonl", &rewritten);
        assert!(run(&path).is_ok(), "{:?}", run(&path));
    }

    #[test]
    fn req_0013_029_rewrite_is_caught_by_later_checkpoint() {
        let f = fixture();
        let mut original = worker(&f.clock);
        let task = TaskId::generate();
        let first: Vec<Sealed> = (1..=3)
            .map(|i| original.seal(approval(&task, &format!("req-{i}"))).unwrap())
            .collect();
        let mut s = supervisor(&f.clock);
        let head = first.last().unwrap().chain_hash.clone();
        let cp = s.seal(checkpoint(3, &head)).unwrap();

        let mut rewritten = worker(&f.clock);
        let mut combined: Vec<Sealed> = (1..=3)
            .map(|i| {
                rewritten
                    .seal(approval(&task, &format!("forged-{i}")))
                    .unwrap()
            })
            .collect();
        combined.push(cp);
        let path = write(f.dir.path(), "events-20260501-1.jsonl", &combined);
        assert_eq!(kind(&run(&path)), Some((4, BreakKind::CheckpointMismatch)));
    }

    #[test]
    fn req_0013_028_gap_event_covers_missing_range() {
        let f = fixture();
        let mut w = worker(&f.clock);
        let task = TaskId::generate();
        let kept_first = w.seal(approval(&task, "req-1")).unwrap();
        let _dropped = w.seal(approval(&task, "req-2")).unwrap();
        let kept_last = w.seal(approval(&task, "req-3")).unwrap();
        let gap = w
            .seal(
                EventBuilder::new(AuditEventType::AuditGap)
                    .detail("first_seq", json!("2"))
                    .detail("last_seq", json!("2")),
            )
            .unwrap();
        let path = write(
            f.dir.path(),
            "events-20260501-1.jsonl",
            &[kept_first, kept_last, gap],
        );
        assert!(run(&path).is_ok(), "{:?}", run(&path));
    }

    #[test]
    fn req_0013_028_missing_range_without_gap_event_is_reported() {
        let f = fixture();
        let mut w = worker(&f.clock);
        let task = TaskId::generate();
        let kept_first = w.seal(approval(&task, "req-1")).unwrap();
        let _dropped = w.seal(approval(&task, "req-2")).unwrap();
        let kept_last = w.seal(approval(&task, "req-3")).unwrap();
        let path = write(
            f.dir.path(),
            "events-20260501-1.jsonl",
            &[kept_first, kept_last],
        );
        assert_eq!(kind(&run(&path)), Some((2, BreakKind::UnexplainedGap)));
    }

    #[test]
    fn req_0013_028_gap_event_covers_only_its_range() {
        let f = fixture();
        let mut w = worker(&f.clock);
        let task = TaskId::generate();
        let kept_first = w.seal(approval(&task, "req-1")).unwrap();
        let _dropped = w.seal(approval(&task, "req-2")).unwrap();
        let _dropped2 = w.seal(approval(&task, "req-3")).unwrap();
        let kept_last = w.seal(approval(&task, "req-4")).unwrap();
        let gap = w
            .seal(
                EventBuilder::new(AuditEventType::AuditGap)
                    .detail("first_seq", json!("2"))
                    .detail("last_seq", json!("2")),
            )
            .unwrap();
        let path = write(
            f.dir.path(),
            "events-20260501-1.jsonl",
            &[kept_first, kept_last, gap],
        );
        assert_eq!(kind(&run(&path)), Some((2, BreakKind::UnexplainedGap)));
    }

    #[test]
    fn req_0013_028_exit_codes_match_outcome() {
        assert_eq!(VerifyReport::default().exit_code(), EXIT_VERIFIED);
        let broken = VerifyReport {
            first_break: Some(Break {
                file: PathBuf::from("x"),
                line: 1,
                kind: BreakKind::Malformed,
            }),
            ..VerifyReport::default()
        };
        assert_eq!(broken.exit_code(), EXIT_BROKEN);
        assert_ne!(EXIT_USAGE, EXIT_BROKEN);
        assert_ne!(EXIT_USAGE, EXIT_VERIFIED);
    }

    #[test]
    fn req_0013_028_remove_range_splits_straddling_ranges() {
        let at = Position { file: 0, line: 1 };
        let kept = remove_range(
            vec![Missing {
                first: 3,
                last: 9,
                at,
            }],
            5,
            6,
        );
        let spans: Vec<(u64, u64)> = kept.iter().map(|range| (range.first, range.last)).collect();
        assert_eq!(spans, vec![(3, 4), (7, 9)]);
    }
}
