//! Lease lifecycle against real git repositories: acquisition, reuse, refusals, release, retention
//! and garbage collection (RFC-0012).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use a2a_workspace::{
    AgentId, CopyLimits, ErrorKind, GIB, LeaseKind, NonGitPolicy, ReleaseOutcome, WorkKey,
    Workspace, WorkspaceConfig, WorkspaceMode, WorkspaceRef,
};
use common::*;

fn retain_dirty(env: &Env, ws: &Workspace, agent: &str, key: &str) -> PathBuf {
    let lease = ws
        .acquire(request(agent, session(key), &env.source))
        .expect("acquire");
    let path = lease.path().to_path_buf();
    fs::write(path.join("notes.txt"), "dirty\n").expect("dirty file");
    match ws.release(lease).expect("release") {
        ReleaseOutcome::Retained { .. } => path,
        other => panic!("expected retained workspace, got {other:?}"),
    }
}

fn plain_dir(env: &Env, name: &str) -> PathBuf {
    let dir = env.dir.path().join(name);
    fs::create_dir_all(dir.join("sub")).expect("plain dirs");
    fs::write(dir.join("a.txt"), "alpha\n").expect("a.txt");
    fs::write(dir.join("sub").join("b.txt"), "beta\n").expect("b.txt");
    dir
}

#[test]
fn req_0012_028_session_workspace_is_shared_by_the_session_tasks() {
    let env = Env::fresh();
    env.init_repo();
    let ws = env.open();
    let first = ws
        .acquire(request("worker-1", session(SESSION), &env.source))
        .unwrap();
    fs::write(first.path().join("scratch.txt"), "kept\n").unwrap();
    let second = ws
        .acquire(request("worker-1", session(SESSION), &env.source))
        .unwrap();
    assert_eq!(second.path(), first.path());
    assert_eq!(second.branch(), first.branch());
    assert_eq!(second.short_id(), SESSION_SHORT);
    assert_eq!(
        fs::read_to_string(second.path().join("scratch.txt")).unwrap(),
        "kept\n"
    );
    assert!(matches!(
        ws.release(second).unwrap(),
        ReleaseOutcome::Retained { .. }
    ));
}

#[test]
fn req_0012_030_worker_directory_is_the_created_worktree() {
    let env = Env::fresh();
    env.init_repo();
    let ws = env.open();
    let lease = ws
        .acquire(request("worker-1", session(SESSION), &env.source))
        .unwrap();
    assert_eq!(lease.kind(), LeaseKind::Worktree);
    assert!(lease.path().join("README.md").is_file());
    assert!(lease.path().starts_with(env.root()));
    assert!(!lease.path().starts_with(&env.source));
    assert_eq!(ws.release(lease).unwrap(), ReleaseOutcome::Removed);
}

#[test]
fn req_0012_032_worktree_starts_at_base_and_leaves_operator_branches_alone() {
    let env = Env::fresh();
    env.init_repo();
    let ws = env.open();
    let head = git(&env.source, &["rev-parse", "HEAD"]);
    let branches = git(&env.source, &["branch", "--format=%(refname:short)"]);
    let lease = ws
        .acquire(request("worker-1", session(SESSION), &env.source))
        .unwrap();
    assert_eq!(lease.base_commit(), head);
    assert_eq!(git(lease.path(), &["rev-parse", "HEAD"]), head);
    assert_eq!(lease.branch(), format!("a2abus/worker-1/{SESSION_SHORT}"));
    assert!(branch_exists(&env.source, lease.branch()));
    assert_eq!(git(&env.source, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        git(&env.source, &["symbolic-ref", "--short", "HEAD"]),
        "main"
    );
    let branch = lease.branch().to_string();
    assert_eq!(ws.release(lease).unwrap(), ReleaseOutcome::Removed);
    assert!(!branch_exists(&env.source, &branch));
    assert_eq!(
        git(&env.source, &["branch", "--format=%(refname:short)"]),
        branches
    );
}

#[test]
fn req_0012_032_base_ref_selects_the_starting_commit() {
    let env = Env::fresh();
    env.init_repo();
    fs::write(env.source.join("README.md"), "second\n").unwrap();
    git(&env.source, &["commit", "-q", "-am", "second"]);
    let parent = git(&env.source, &["rev-parse", "HEAD~1"]);
    let ws = env.open_with(|config| config.base_ref = "HEAD~1".into());
    let lease = ws
        .acquire(request("worker-1", session(SESSION), &env.source))
        .unwrap();
    assert_eq!(lease.base_commit(), parent);
    assert_eq!(
        fs::read_to_string(lease.path().join("README.md")).unwrap(),
        "init\n"
    );
    assert_eq!(ws.release(lease).unwrap(), ReleaseOutcome::Removed);
}

#[test]
fn req_0012_032_dirty_operator_changes_are_not_copied_into_the_workspace() {
    let env = Env::fresh();
    env.init_repo();
    fs::write(env.source.join("README.md"), "uncommitted\n").unwrap();
    fs::write(env.source.join("notes.txt"), "untracked\n").unwrap();
    let head = git(&env.source, &["rev-parse", "HEAD"]);
    let ws = env.open();
    let lease = ws
        .acquire(request("worker-1", session(SESSION), &env.source))
        .unwrap();
    assert!(lease.source_dirty());
    assert!(!lease.path().join("notes.txt").exists());
    assert_eq!(
        fs::read_to_string(lease.path().join("README.md")).unwrap(),
        "init\n"
    );
    assert_eq!(
        fs::read_to_string(env.source.join("README.md")).unwrap(),
        "uncommitted\n"
    );
    assert_eq!(git(&env.source, &["rev-parse", "HEAD"]), head);
    assert_eq!(ws.release(lease).unwrap(), ReleaseOutcome::Removed);
}

#[test]
fn req_0012_032_existing_branch_with_commits_is_not_replaced() {
    let env = Env::fresh();
    env.init_repo();
    let ws = env.open();
    let branch = format!("a2abus/worker-1/{SESSION_SHORT}");

    git(&env.source, &["branch", &branch]);
    let lease = ws
        .acquire(request("worker-1", session(SESSION), &env.source))
        .unwrap();
    assert_eq!(ws.release(lease).unwrap(), ReleaseOutcome::Removed);
    assert!(!branch_exists(&env.source, &branch));

    git(&env.source, &["checkout", "-q", "-b", &branch]);
    fs::write(env.source.join("work.txt"), "work\n").unwrap();
    git(&env.source, &["add", "-A"]);
    git(&env.source, &["commit", "-q", "-m", "work"]);
    git(&env.source, &["checkout", "-q", "main"]);
    let err = ws
        .acquire(request("worker-1", session(SESSION), &env.source))
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::BranchExists);
    assert!(branch_exists(&env.source, &branch));
    assert_eq!(env.lock_count(), 0);
}

#[test]
fn req_0012_035_clean_release_removes_worktree_and_branch() {
    let env = Env::fresh();
    env.init_repo();
    let ws = env.open();
    let lease = ws
        .acquire(request("worker-1", session(SESSION), &env.source))
        .unwrap();
    let path = lease.path().to_path_buf();
    let branch = lease.branch().to_string();
    assert_eq!(ws.release(lease).unwrap(), ReleaseOutcome::Removed);
    assert!(!path.exists());
    assert!(!branch_exists(&env.source, &branch));
    assert_eq!(env.lock_count(), 0);
}

#[test]
fn req_0012_035_dirty_release_retains_workspace_with_reasons_in_index() {
    let env = Env::fresh();
    env.init_repo();
    let ws = env.open();
    let lease = ws
        .acquire(request("worker-1", session(SESSION), &env.source))
        .unwrap();
    let path = lease.path().to_path_buf();
    let branch = lease.branch().to_string();
    fs::write(path.join("README.md"), "edited\n").unwrap();
    fs::write(path.join("new.txt"), "new\n").unwrap();
    match ws.release(lease).unwrap() {
        ReleaseOutcome::Retained { reasons } => {
            assert!(reasons.contains(&"uncommitted_changes".to_string()));
            assert!(reasons.contains(&"untracked_files".to_string()));
        }
        other => panic!("expected retained, got {other:?}"),
    }
    assert!(path.join("new.txt").exists());
    assert!(branch_exists(&env.source, &branch));
    let entries = env.index_entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["agent"], "worker-1");
    assert_eq!(entries[0]["reasons"][0], "uncommitted_changes");
    assert_eq!(entries[0]["reasons"][1], "untracked_files");
    assert_eq!(env.lock_count(), 0);
}

#[test]
fn req_0012_035_commits_made_in_the_workspace_retain_it() {
    let env = Env::fresh();
    env.init_repo();
    let ws = env.open();
    let lease = ws
        .acquire(request("worker-1", session(SESSION), &env.source))
        .unwrap();
    fs::write(lease.path().join("feature.txt"), "feature\n").unwrap();
    git(lease.path(), &["add", "-A"]);
    git(lease.path(), &["commit", "-q", "-m", "feature"]);
    match ws.release(lease).unwrap() {
        ReleaseOutcome::Retained { reasons } => {
            assert_eq!(reasons, vec!["commits_beyond_base".to_string()]);
        }
        other => panic!("expected retained, got {other:?}"),
    }
}

#[test]
fn req_0012_031_workspace_root_inside_operator_checkout_is_refused() {
    let env = Env::fresh();
    env.init_repo();
    let ws = Workspace::open(WorkspaceConfig::new(
        env.source.join(".a2a-ws"),
        env.artifacts(),
        "tenant-a",
        WorkspaceMode::PerSession,
    ))
    .unwrap();
    let err = ws
        .acquire(request("worker-1", session(SESSION), &env.source))
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::SharedCheckout);
    assert_eq!(env.lock_count(), 0);
}

#[test]
fn req_0012_031_two_sessions_on_one_checkout_get_separate_worktrees() {
    let env = Env::fresh();
    env.init_repo();
    let ws = env.open();
    let first = ws
        .acquire(request("worker-1", session(SESSION), &env.source))
        .unwrap();
    let second = ws
        .acquire(request("worker-2", session(SESSION_TWO), &env.source))
        .unwrap();
    assert_ne!(first.path(), second.path());
    assert!(!first.path().starts_with(second.path()));
    assert!(!second.path().starts_with(first.path()));
    assert_eq!(first.base_commit(), second.base_commit());
    assert_eq!(ws.release(first).unwrap(), ReleaseOutcome::Removed);
    assert_eq!(ws.release(second).unwrap(), ReleaseOutcome::Removed);
}

#[test]
fn req_0012_029_scope_must_match_the_configured_mode() {
    let env = Env::fresh();
    env.init_repo();
    let per_session = env.open();
    let err = per_session
        .acquire(request("worker-1", task(TASK), &env.source))
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Config);
    let per_task = env.open_with(|config| config.mode = WorkspaceMode::PerTask);
    let err = per_task
        .acquire(request("worker-1", session(SESSION), &env.source))
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Config);
    assert_eq!(env.lock_count(), 0);
}

#[test]
fn req_0012_029_per_task_mode_gives_each_task_its_own_worktree() {
    let env = Env::fresh();
    env.init_repo();
    let ws = env.open_with(|config| config.mode = WorkspaceMode::PerTask);
    let first = ws
        .acquire(request("worker-1", task(TASK), &env.source))
        .unwrap();
    let second = ws
        .acquire(request("worker-1", task(TASK_TWO), &env.source))
        .unwrap();
    assert_ne!(first.path(), second.path());
    assert!(first.path().is_dir() && second.path().is_dir());
    assert_eq!(ws.release(first).unwrap(), ReleaseOutcome::Removed);
    assert_eq!(ws.release(second).unwrap(), ReleaseOutcome::Removed);
}

/// Child-process body for `req_0012_021_live_holder_blocks_second_acquire_until_it_dies`. It does
/// nothing unless the parent sets the environment; it then holds a lease until it is killed.
#[test]
fn req_0012_021_child_holder() {
    let Some(root) = std::env::var_os("A2A_WS_TEST_ROOT") else {
        return;
    };
    let artifacts = std::env::var_os("A2A_WS_TEST_ARTIFACTS").expect("artifacts env");
    let source = std::env::var_os("A2A_WS_TEST_SOURCE").expect("source env");
    let config = WorkspaceConfig::new(
        PathBuf::from(root),
        PathBuf::from(artifacts),
        "tenant-a",
        WorkspaceMode::PerSession,
    );
    let ws = Workspace::open(config).expect("child open");
    let _lease = ws
        .acquire(request("worker-1", session(SESSION), Path::new(&source)))
        .expect("child acquire");
    std::thread::sleep(Duration::from_secs(300));
}

#[test]
fn req_0012_021_live_holder_blocks_second_acquire_until_it_dies() {
    let env = Env::fresh();
    env.init_repo();
    let exe = std::env::current_exe().expect("test binary path");
    let mut child: Child = Command::new(exe)
        .args(["--exact", "req_0012_021_child_holder", "--test-threads=1"])
        .env("A2A_WS_TEST_ROOT", env.root())
        .env("A2A_WS_TEST_ARTIFACTS", env.artifacts())
        .env("A2A_WS_TEST_SOURCE", &env.source)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn holder");
    let held = wait_until(Duration::from_secs(120), || env.lock_count() == 1);
    assert!(held, "holder process never published its lease");

    let ws = env.open();
    let err = ws
        .acquire(request("worker-1", session(SESSION), &env.source))
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::SharedCheckout);
    assert_eq!(
        env.lock_count(),
        1,
        "refused acquire must not create a lock"
    );

    child.kill().expect("kill holder");
    child.wait().expect("reap holder");
    let lease = ws
        .acquire(request("worker-1", session(SESSION), &env.source))
        .expect("dead holder is reclaimed");
    assert_eq!(env.lock_count(), 1);
    assert_eq!(ws.release(lease).unwrap(), ReleaseOutcome::Removed);
    assert_eq!(env.lock_count(), 0);
}

#[test]
fn req_0014_022_non_git_source_is_refused_before_any_lock_or_copy() {
    let env = Env::fresh();
    let plain = plain_dir(&env, "plain");
    let ws = env.open();
    let err = ws
        .acquire(request("worker-1", session(SESSION), &plain))
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::NonGitRefused);
    assert_eq!(env.lock_count(), 0);
    assert!(!env.root().join("tenant-a").exists());
}

#[test]
fn req_0012_034_copy_policy_leases_a_baseline_repository() {
    let env = Env::fresh();
    let plain = plain_dir(&env, "plain");
    let ws = env.open_with(|config| config.non_git = NonGitPolicy::Copy);
    let lease = ws
        .acquire(request("worker-1", session(SESSION), &plain))
        .unwrap();
    assert_eq!(lease.kind(), LeaseKind::Copy);
    assert!(!lease.source_dirty());
    assert_eq!(
        fs::read_to_string(lease.path().join("sub").join("b.txt")).unwrap(),
        "beta\n"
    );
    assert_eq!(
        git(lease.path(), &["rev-parse", "HEAD"]),
        lease.base_commit()
    );
    assert_eq!(git(lease.path(), &["status", "--porcelain"]), "");

    fs::write(plain.join("a.txt"), "changed later\n").unwrap();
    assert_eq!(
        fs::read_to_string(lease.path().join("a.txt")).unwrap(),
        "alpha\n"
    );
    assert_eq!(ws.release(lease).unwrap(), ReleaseOutcome::Removed);
}

#[test]
fn req_0012_034_copy_limits_refuse_oversized_sources() {
    let env = Env::fresh();
    let plain = plain_dir(&env, "plain");
    let by_files = env.open_with(|config| {
        config.non_git = NonGitPolicy::Copy;
        config.copy_limits = CopyLimits {
            max_bytes: GIB,
            max_files: 1,
        };
    });
    let err = by_files
        .acquire(request("worker-1", session(SESSION), &plain))
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::CopyLimit);

    let by_bytes = env.open_with(|config| {
        config.non_git = NonGitPolicy::Copy;
        config.copy_limits = CopyLimits {
            max_bytes: 4,
            max_files: 100_000,
        };
    });
    let err = by_bytes
        .acquire(request("worker-1", session(SESSION), &plain))
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::CopyLimit);
    assert_eq!(env.lock_count(), 0);
}

#[test]
fn req_0012_034_copy_rejects_links_in_the_source() {
    let env = Env::fresh();
    let plain = plain_dir(&env, "plain");
    make_dir_link(&plain.join("link"), &plain.join("sub"));
    let ws = env.open_with(|config| config.non_git = NonGitPolicy::Copy);
    let err = ws
        .acquire(request("worker-1", session(SESSION), &plain))
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::CopyLinkRejected);
    assert!(
        !env.root()
            .join("tenant-a")
            .join("worker-1")
            .join(SESSION_SHORT)
            .exists()
    );
    assert_eq!(env.lock_count(), 0);
}

#[cfg(windows)]
#[test]
fn req_0012_037_root_beyond_the_windows_path_budget_is_refused_at_open() {
    let env = Env::fresh();
    let root = env.dir.path().join("x".repeat(170));
    let config = WorkspaceConfig::new(root, env.artifacts(), "tenant-a", WorkspaceMode::PerSession);
    let err = Workspace::open(config).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::PathTooLong);
}

#[test]
fn req_0012_038_open_refuses_relative_roots_and_unsafe_tenants() {
    let env = Env::fresh();
    let relative =
        WorkspaceConfig::new("ws", env.artifacts(), "tenant-a", WorkspaceMode::PerSession);
    assert_eq!(
        Workspace::open(relative).unwrap_err().kind(),
        ErrorKind::Config
    );
    let mut unsafe_tenant = env.config(WorkspaceMode::PerSession);
    unsafe_tenant.tenant = "../escape".to_string();
    assert_eq!(
        Workspace::open(unsafe_tenant).unwrap_err().kind(),
        ErrorKind::Config
    );
}

#[test]
fn req_0012_036_gc_removes_retained_workspaces_older_than_fourteen_days() {
    let env = Env::fresh();
    env.init_repo();
    let ws = env.open();
    let path = retain_dirty(&env, &ws, "worker-1", SESSION);

    let fresh = ws.gc(now_ms(), &[]).unwrap();
    assert_eq!((fresh.removed, fresh.retained), (0, 1));
    assert!(path.exists());

    let expired = ws.gc(now_ms() + FIFTEEN_DAYS_MS, &[]).unwrap();
    assert_eq!((expired.removed, expired.retained), (1, 0));
    assert_eq!(expired.retained_bytes, 0);
    assert!(!path.exists());
    assert!(env.index_entries().is_empty());
}

#[test]
fn req_0012_036_gc_removes_oldest_first_and_protects_referenced_workspaces() {
    let env = Env::fresh();
    env.init_repo();
    let ws = env.open_with(|config| config.retention.max_retained_bytes = 0);
    let older = retain_dirty(&env, &ws, "worker-1", SESSION);
    std::thread::sleep(Duration::from_millis(5));
    let newer = retain_dirty(&env, &ws, "worker-2", SESSION_TWO);

    let protected = WorkspaceRef {
        agent: AgentId::parse("worker-1").unwrap(),
        key: WorkKey::parse(SESSION).unwrap(),
    };
    let report = ws.gc(now_ms(), &[protected]).unwrap();
    assert_eq!(report.removed, 1);
    assert_eq!(report.retained, 1);
    assert!(older.exists(), "referenced workspace must be kept");
    assert!(
        !newer.exists(),
        "unreferenced workspace must be removed to meet the byte budget"
    );

    let report = ws.gc(now_ms(), &[]).unwrap();
    assert_eq!(report.removed, 1);
    assert!(!older.exists());
}

#[test]
fn req_0012_035_removal_never_follows_links_out_of_the_workspace() {
    let env = Env::fresh();
    env.init_repo();
    let outside = env.dir.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("keep.txt"), "operator data\n").unwrap();
    let ws = env.open();
    let lease = ws
        .acquire(request("worker-1", session(SESSION), &env.source))
        .unwrap();
    let path = lease.path().to_path_buf();
    make_dir_link(&path.join("link"), &outside);
    fs::write(path.join("README.md"), "edited\n").unwrap();
    assert!(matches!(
        ws.release(lease).unwrap(),
        ReleaseOutcome::Retained { .. }
    ));

    let report = ws.gc(now_ms() + FIFTEEN_DAYS_MS, &[]).unwrap();
    assert_eq!(report.removed, 1);
    assert!(!path.exists());
    assert_eq!(
        fs::read_to_string(outside.join("keep.txt")).unwrap(),
        "operator data\n"
    );
}
