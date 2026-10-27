#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Copy-mode exclusions: git ignore rules, the built-in denylist, `copy_exclude` and
//! `excluded_count` (RFC-0012 0012-067, 0012-073, 0012-074).

mod common;

use std::fs;
use std::path::Path;

use a2a_workspace::{
    CopyLimits, ErrorKind, GIB, LeaseKind, NonGitPolicy, Workspace, WorkspaceMode,
};
use common::*;

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("parent dir");
    }
    fs::write(path, contents).expect("write fixture");
}

fn copy_workspace(env: &Env) -> Workspace {
    env.open_with(|config| config.non_git = NonGitPolicy::Copy)
}

/// Paths tracked by the copy's baseline commit (the index is the baseline).
fn tracked(path: &Path) -> Vec<String> {
    git(path, &["ls-files"])
        .lines()
        .map(str::to_string)
        .collect()
}

/// Concatenated content of every lease lock file, without whitespace.
fn lock_text(env: &Env) -> String {
    let mut text = String::new();
    for entry in fs::read_dir(env.root().join("locks")).expect("locks dir") {
        let entry = entry.expect("lock entry");
        text.push_str(&fs::read_to_string(entry.path()).expect("lock file"));
    }
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

// [0012-073]
#[test]
fn req_0012_073_gitignored_files_stay_out_of_copy_and_baseline() {
    let env = Env::fresh();
    write(&env.source.join(".gitignore"), "*.local\nbuild/\n");
    write(&env.source.join("secrets.local"), "token\n");
    write(&env.source.join("build/out.bin"), "bin\n");
    write(&env.source.join("src/main.rs"), "fn main() {}\n");

    let ws = copy_workspace(&env);
    let lease = ws
        .acquire(request("worker-1", session(SESSION), &env.source))
        .expect("acquire copy");

    assert_eq!(lease.kind(), LeaseKind::Copy);
    let work = lease.path();
    assert!(!work.join("secrets.local").exists());
    assert!(
        !work.join("build").exists(),
        "ignored directory must not be created"
    );
    assert_eq!(tracked(work), [".gitignore", "src/main.rs"]);
    assert_eq!(lease.excluded_count(), 2);
}

// [0012-073]
#[test]
fn req_0012_073_nested_gitignore_applies_only_to_its_directory() {
    let env = Env::fresh();
    write(&env.source.join("sub/.gitignore"), "*.tmp\n");
    write(&env.source.join("sub/x.tmp"), "ignored\n");
    write(&env.source.join("other/x.tmp"), "kept\n");

    let ws = copy_workspace(&env);
    let lease = ws
        .acquire(request("worker-1", session(SESSION), &env.source))
        .expect("acquire copy");

    assert!(!lease.path().join("sub/x.tmp").exists());
    assert!(lease.path().join("other/x.tmp").exists());
    assert_eq!(tracked(lease.path()), ["other/x.tmp", "sub/.gitignore"]);
    assert_eq!(lease.excluded_count(), 1);
}

// [0012-073]
#[test]
fn req_0012_073_ignored_env_is_absent_from_git_worktree() {
    let env = Env::fresh();
    env.init_repo();
    write(&env.source.join(".gitignore"), ".env\n");
    git(&env.source, &["add", ".gitignore"]);
    git(&env.source, &["commit", "-q", "-m", "ignore rules"]);
    write(&env.source.join(".env"), "SECRET=1\n");

    let ws = env.open();
    let lease = ws
        .acquire(request("worker-1", session(SESSION), &env.source))
        .expect("acquire worktree");

    assert_eq!(lease.kind(), LeaseKind::Worktree);
    assert!(!lease.path().join(".env").exists());
    assert_eq!(lease.excluded_count(), 0);
}

// [0012-074]
#[test]
fn req_0012_074_denylisted_files_are_not_copied_and_are_counted() {
    let env = Env::fresh();
    write(&env.source.join(".env"), "SECRET=1\n");
    write(&env.source.join("id_rsa"), "key\n");
    write(&env.source.join(".aws/config"), "profile\n");
    write(&env.source.join("keys/server.pem"), "pem\n");
    write(&env.source.join("README.md"), "readme\n");
    write(&env.source.join("src/lib.rs"), "pub fn f() {}\n");

    let ws = copy_workspace(&env);
    let lease = ws
        .acquire(request("worker-1", session(SESSION), &env.source))
        .expect("acquire copy");

    let work = lease.path();
    assert!(!work.join(".env").exists());
    assert!(!work.join("id_rsa").exists());
    assert!(!work.join(".aws").exists());
    assert!(!work.join("keys/server.pem").exists());
    assert_eq!(tracked(work), ["README.md", "src/lib.rs"]);
    assert_eq!(lease.excluded_count(), 4);

    let locks = lock_text(&env);
    assert!(
        locks.contains("\"excluded_count\":4"),
        "lock record: {locks}"
    );
    assert!(!locks.contains("server.pem"), "names must not be recorded");
    assert!(!locks.contains("id_rsa"), "names must not be recorded");
}

// [0012-074]
#[test]
fn req_0012_074_excluded_directory_counts_every_regular_file_below_it() {
    let env = Env::fresh();
    write(&env.source.join(".ssh/id_ed25519"), "key\n");
    write(&env.source.join(".ssh/config"), "host\n");
    write(&env.source.join("deep/.gnupg/a"), "secret\n");
    write(&env.source.join("deep/keep.txt"), "kept\n");

    let ws = copy_workspace(&env);
    let lease = ws
        .acquire(request("worker-1", session(SESSION), &env.source))
        .expect("acquire copy");

    assert_eq!(tracked(lease.path()), ["deep/keep.txt"]);
    assert_eq!(lease.excluded_count(), 3);
}

// [0012-074]
#[test]
fn req_0012_074_copy_exclude_matches_names_and_paths_case_insensitively() {
    let env = Env::fresh();
    write(&env.source.join("notes.txt"), "a\n");
    write(&env.source.join("a/notes.txt"), "b\n");
    write(&env.source.join("b/NOTES.TXT"), "c\n");
    write(&env.source.join("docs/private/plan.md"), "d\n");
    write(&env.source.join("docs/other.md"), "e\n");
    write(&env.source.join("other/docs/private/plan.md"), "f\n");

    let ws = env.open_with(|config| {
        config.non_git = NonGitPolicy::Copy;
        config.copy_exclude = vec!["notes.txt".into(), "docs/private/*.md".into()];
    });
    let lease = ws
        .acquire(request("worker-1", session(SESSION), &env.source))
        .expect("acquire copy");

    assert_eq!(
        tracked(lease.path()),
        ["docs/other.md", "other/docs/private/plan.md"]
    );
    assert_eq!(lease.excluded_count(), 4);
}

// [0012-074]
#[test]
fn req_0012_074_invalid_copy_exclude_pattern_is_config_error() {
    let env = Env::fresh();
    let mut config = env.config(WorkspaceMode::PerSession);
    config.copy_exclude = vec!["[oops".into()];
    let err = Workspace::open(config).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Config);
}

// [0012-074]
#[test]
fn req_0012_074_excluded_entries_do_not_count_toward_copy_limits() {
    let env = Env::fresh();
    write(&env.source.join(".env"), "SECRET=1\n");
    write(&env.source.join("id_rsa"), "key\n");
    write(&env.source.join("a.txt"), "a\n");
    write(&env.source.join("b.txt"), "b\n");

    let ws = env.open_with(|config| {
        config.non_git = NonGitPolicy::Copy;
        config.copy_limits = CopyLimits {
            max_bytes: GIB,
            max_files: 2,
        };
    });
    let lease = ws
        .acquire(request("worker-1", session(SESSION), &env.source))
        .expect("two kept files fit max_files 2");

    assert_eq!(tracked(lease.path()), ["a.txt", "b.txt"]);
    assert_eq!(lease.excluded_count(), 2);
}
