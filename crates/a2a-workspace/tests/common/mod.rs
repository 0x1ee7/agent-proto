//! Shared fixtures for the integration tests: operator repositories built with the git CLI,
//! workspace configuration and small helpers. Each test binary includes this module with
//! `mod common;`, so helpers unused by one binary are allowed.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use a2a_workspace::{
    AcquireRequest, AgentId, WorkKey, WorkScope, Workspace, WorkspaceConfig, WorkspaceMode,
};
use tempfile::TempDir;

pub const SESSION: &str = "123e4567-e89b-42d3-a456-426614174000";
pub const SESSION_TWO: &str = "423e4567-e89b-42d3-a456-426614174001";
pub const SESSION_SHORT: &str = "123e4567e89b";
pub const TASK: &str = "223e4567-e89b-42d3-a456-426614174000";
pub const TASK_TWO: &str = "323e4567-e89b-42d3-a456-426614174001";

/// Temporary state directory, artifact root and operator checkout for one test.
pub struct Env {
    pub dir: TempDir,
    pub state: PathBuf,
    pub source: PathBuf,
}

impl Env {
    pub fn fresh() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let state = dir.path().join("state");
        let source = dir.path().join("operator");
        fs::create_dir_all(&source).expect("operator dir");
        Self { dir, state, source }
    }

    pub fn root(&self) -> PathBuf {
        self.state.join("ws")
    }

    pub fn artifacts(&self) -> PathBuf {
        self.state.join("artifacts")
    }

    pub fn config(&self, mode: WorkspaceMode) -> WorkspaceConfig {
        WorkspaceConfig::new(self.root(), self.artifacts(), "tenant-a", mode)
    }

    pub fn open(&self) -> Workspace {
        self.open_with(|_| {})
    }

    pub fn open_with(&self, tweak: impl FnOnce(&mut WorkspaceConfig)) -> Workspace {
        let mut config = self.config(WorkspaceMode::PerSession);
        tweak(&mut config);
        Workspace::open(config).expect("open workspace")
    }

    /// Initializes the operator checkout as a git repository with one commit.
    pub fn init_repo(&self) {
        make_repo(&self.source);
    }

    /// Number of lease lock files currently present.
    pub fn lock_count(&self) -> usize {
        fs::read_dir(self.root().join("locks"))
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "lock"))
                    .count()
            })
            .unwrap_or(0)
    }

    /// Entries of `index.json`, empty when the file does not exist.
    pub fn index_entries(&self) -> Vec<serde_json::Value> {
        match fs::read(self.root().join("index.json")) {
            Ok(bytes) => {
                let value: serde_json::Value = serde_json::from_slice(&bytes).expect("index json");
                value["entries"].as_array().cloned().unwrap_or_default()
            }
            Err(_) => Vec::new(),
        }
    }
}

/// Creates `dir` as a git repository on branch `main` whose only commit contains `README.md`.
pub fn make_repo(dir: &Path) {
    fs::create_dir_all(dir).expect("repo dir");
    git(dir, &["init", "-q", "-b", "main"]);
    fs::write(dir.join("README.md"), "init\n").expect("readme");
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

/// Runs git in `dir` and returns raw stdout, failing the test on a non-zero exit.
pub fn git_bytes(dir: &Path, args: &[&str]) -> Vec<u8> {
    let out = Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
        ])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// Runs git in `dir` and returns trimmed UTF-8 stdout.
pub fn git(dir: &Path, args: &[&str]) -> String {
    String::from_utf8(git_bytes(dir, args))
        .expect("utf-8 git output")
        .trim()
        .to_string()
}

/// Whether `refs/heads/<branch>` exists in `repo`.
pub fn branch_exists(repo: &Path, branch: &str) -> bool {
    Command::new("git")
        .current_dir(repo)
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git")
        .success()
}

/// Acquire request for `agent` on a session scope.
pub fn session(key: &str) -> WorkScope {
    WorkScope::Session(WorkKey::parse(key).expect("session key"))
}

/// Acquire request scope for a task.
pub fn task(key: &str) -> WorkScope {
    WorkScope::Task(WorkKey::parse(key).expect("task key"))
}

pub fn request(agent: &str, scope: WorkScope, source: &Path) -> AcquireRequest {
    AcquireRequest {
        agent: AgentId::parse(agent).expect("agent id"),
        scope,
        source: source.to_path_buf(),
    }
}

/// Creates a directory link at `link` pointing to `target`: a symlink on Unix and a junction
/// (`mklink /J`, no privilege needed) on Windows.
pub fn make_dir_link(link: &Path, target: &Path) {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).expect("symlink");
    }
    #[cfg(windows)]
    {
        let status = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .stdout(Stdio::null())
            .status()
            .expect("spawn cmd");
        assert!(status.success(), "mklink /J failed");
    }
}

/// Polls `condition` every 50 ms until it holds or `timeout` elapses.
pub fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    condition()
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    let elapsed = SystemTime::now().duration_since(UNIX_EPOCH).expect("clock");
    u64::try_from(elapsed.as_millis()).expect("clock fits u64")
}

/// Fifteen days in milliseconds, past the default 14-day retention.
pub const FIFTEEN_DAYS_MS: u64 = 15 * 24 * 60 * 60 * 1000;
