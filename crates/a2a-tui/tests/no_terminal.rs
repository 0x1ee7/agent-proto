//! Without a terminal the console fails cleanly, before any bus handle exists (no runtime panic).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::{Command, Stdio};

#[test]
fn non_terminal_stdout_fails_with_one_line_error() {
    let out = Command::new(env!("CARGO_BIN_EXE_a2a-tui"))
        .args(["--seed-file", "does-not-exist.nk"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "stderr: {stderr}");
    assert_eq!(stderr.trim().lines().count(), 1, "stderr: {stderr}");
    assert!(stderr.contains("not a terminal"), "stderr: {stderr}");
    assert!(!stderr.contains("panicked"), "stderr: {stderr}");
}
