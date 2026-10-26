//! Diff artifacts and summaries written from a live lease (RFC-0012 sections 4.8 and 4.9).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::fs;
use std::path::Path;

use a2a_workspace::{PATCH_TRUNCATED_MARKER, ReleaseOutcome};
use common::*;
use sha2::{Digest, Sha256};

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn collect_strings<'a>(value: &'a serde_json::Value, out: &mut Vec<&'a str>) {
    match value {
        serde_json::Value::String(text) => out.push(text),
        serde_json::Value::Array(items) => items.iter().for_each(|item| collect_strings(item, out)),
        serde_json::Value::Object(map) => map.values().for_each(|item| collect_strings(item, out)),
        _ => {}
    }
}

#[test]
fn req_0012_040_diff_artifact_is_git_diff_binary_plus_untracked_block() {
    let env = Env::fresh();
    env.init_repo();
    let ws = env.open();
    let lease = ws
        .acquire(request("worker-1", session(SESSION), &env.source))
        .unwrap();
    fs::write(lease.path().join("README.md"), "changed\n").unwrap();
    fs::write(lease.path().join("new.txt"), "new\n").unwrap();

    let summary = ws.write_artifacts(&lease, "task-1").unwrap();

    let mut expected = git_bytes(lease.path(), &["diff", "--binary", lease.base_commit()]);
    expected.extend_from_slice(b"# untracked: new.txt\n");
    let body = fs::read(env.artifacts().join("task-1").join("diff.patch")).unwrap();
    assert_eq!(body, expected);
    assert_eq!(summary.relative_path, "task-1/diff.patch");
    assert_eq!(summary.patch_bytes, body.len() as u64);
    assert_eq!(summary.sha256, sha256_hex(&body));
    assert_eq!(summary.untracked, vec!["new.txt".to_string()]);
    assert_eq!(summary.files.len(), 1);
    assert_eq!(summary.files[0].path, "README.md");
    assert_eq!(summary.files[0].insertions, Some(1));
    assert_eq!(summary.files[0].deletions, Some(1));
    assert_eq!((summary.insertions, summary.deletions), (1, 1));
    assert!(!summary.truncated);
    assert!(matches!(
        ws.release(lease).unwrap(),
        ReleaseOutcome::Retained { .. }
    ));
}

#[test]
fn req_0012_041_patch_over_the_cap_is_cut_at_a_line_with_marker_and_flag() {
    let env = Env::fresh();
    env.init_repo();
    let ws = env.open_with(|config| config.patch_cap_bytes = 4096);
    let lease = ws
        .acquire(request("worker-1", session(SESSION), &env.source))
        .unwrap();
    let big: String = (0..5000).map(|index| format!("line {index}\n")).collect();
    fs::write(lease.path().join("README.md"), big).unwrap();

    let summary = ws.write_artifacts(&lease, "task-big").unwrap();

    let body = fs::read(env.artifacts().join("task-big").join("diff.patch")).unwrap();
    assert!(summary.truncated);
    assert!(
        body.len() <= 4096,
        "patch is {} bytes, cap is 4096",
        body.len()
    );
    assert_eq!(summary.patch_bytes, body.len() as u64);
    assert_eq!(summary.sha256, sha256_hex(&body));
    let text = String::from_utf8(body).unwrap();
    let marker_line = format!("{PATCH_TRUNCATED_MARKER}\n");
    assert!(text.ends_with(&marker_line));
    let cut = text.strip_suffix(&marker_line).unwrap();
    assert!(cut.ends_with('\n'), "diff must be cut at a line boundary");
    assert!(cut.starts_with("diff --git"));
    assert!(matches!(
        ws.release(lease).unwrap(),
        ReleaseOutcome::Retained { .. }
    ));
}

#[test]
fn req_0012_042_summary_carries_no_absolute_paths() {
    let env = Env::fresh();
    env.init_repo();
    let ws = env.open();
    let lease = ws
        .acquire(request("worker-1", session(SESSION), &env.source))
        .unwrap();
    fs::write(lease.path().join("README.md"), "changed\n").unwrap();
    fs::write(lease.path().join("new.txt"), "new\n").unwrap();

    let summary = ws.write_artifacts(&lease, "task-1").unwrap();

    let json = serde_json::to_value(&summary).unwrap();
    let mut strings: Vec<&str> = Vec::new();
    collect_strings(&json, &mut strings);
    assert!(!strings.is_empty());
    for text in strings {
        assert!(
            !Path::new(text).is_absolute(),
            "absolute path in summary: {text}"
        );
        assert!(
            !text.contains(env.dir.path().to_string_lossy().as_ref()),
            "leaks temp dir: {text}"
        );
    }
    assert!(matches!(
        ws.release(lease).unwrap(),
        ReleaseOutcome::Retained { .. }
    ));
}

#[test]
fn req_0012_041_patch_cap_below_minimum_is_refused_at_open() {
    let env = Env::fresh();
    let config = {
        let mut config = env.config(a2a_workspace::WorkspaceMode::PerSession);
        config.patch_cap_bytes = 100;
        config
    };
    assert_eq!(
        a2a_workspace::Workspace::open(config).unwrap_err().kind(),
        a2a_workspace::ErrorKind::Config
    );
}
