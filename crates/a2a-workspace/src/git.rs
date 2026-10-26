//! Hardened git invocations: explicit argv, fixed working directory, no prompts, no hooks, no
//! remote operations. Only the typed operations below are reachable; none of them fetch, push,
//! pull or touch remotes, and none checks out or resets an operator branch.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

use crate::error::{ErrorKind, Result, WorkspaceError};
use crate::platform::null_device;

/// Environment variables that could redirect repository discovery or inject configuration.
const STRIPPED_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_EXTERNAL_DIFF",
    "GIT_DIFF_OPTS",
];

/// Per-invocation `-c` settings. Hooks point at the null device; commit identity is fixed.
fn hardening_config() -> Vec<String> {
    let mut config = vec![
        format!("core.hooksPath={}", null_device()),
        "core.fsmonitor=false".to_string(),
        "user.name=a2abus".to_string(),
        "user.email=workspace@a2abus.invalid".to_string(),
        "commit.gpgSign=false".to_string(),
    ];
    if cfg!(windows) {
        config.push("core.longpaths=true".to_string());
    }
    config
}

/// Full argv for an invocation: hardening `-c` flags followed by the subcommand and arguments.
pub(crate) fn argv(sub: &[&OsStr]) -> Vec<OsString> {
    let mut out = Vec::new();
    for item in hardening_config() {
        out.push(OsString::from("-c"));
        out.push(OsString::from(item));
    }
    out.extend(sub.iter().map(|arg| arg.to_os_string()));
    out
}

/// Command with fixed cwd, hardened environment and piped output.
pub(crate) fn command(dir: &Path, sub: &[&OsStr]) -> Command {
    let mut cmd = Command::new("git");
    cmd.args(argv(sub))
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_OPTIONAL_LOCKS", "0");
    for var in STRIPPED_ENV {
        cmd.env_remove(var);
    }
    cmd
}

fn run_raw(dir: &Path, sub: &[&OsStr]) -> Result<Output> {
    command(dir, sub)
        .output()
        .map_err(|e| WorkspaceError::new(ErrorKind::Git, format!("cannot start git: {e}")))
}

fn run(dir: &Path, sub: &[&OsStr]) -> Result<Vec<u8>> {
    let out = run_raw(dir, sub)?;
    if out.status.success() {
        return Ok(out.stdout);
    }
    let verb = sub
        .first()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stderr = String::from_utf8_lossy(&out.stderr);
    let first = stderr.lines().next().unwrap_or("").trim();
    let first: String = first.chars().take(300).collect();
    Err(WorkspaceError::new(
        ErrorKind::Git,
        format!("git {verb} failed ({}): {first}", out.status),
    ))
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_string()
}

/// Top-level directory of the repository containing `dir`, or `None` when `dir` is not inside a
/// git work tree.
pub(crate) fn toplevel(dir: &Path) -> Result<Option<PathBuf>> {
    let out = run_raw(
        dir,
        &[OsStr::new("rev-parse"), OsStr::new("--show-toplevel")],
    )?;
    if !out.status.success() {
        return Ok(None);
    }
    let line = String::from_utf8_lossy(&out.stdout)
        .trim_end_matches(['\r', '\n'])
        .to_string();
    if line.is_empty() {
        Ok(None)
    } else {
        Ok(Some(PathBuf::from(line)))
    }
}

/// Commit that `base_ref` names, as a full SHA.
pub(crate) fn resolve_commit(dir: &Path, base_ref: &str) -> Result<String> {
    let spec = format!("{base_ref}^{{commit}}");
    let out = run_raw(
        dir,
        &[
            OsStr::new("rev-parse"),
            OsStr::new("--verify"),
            OsStr::new("--quiet"),
            OsStr::new(&spec),
        ],
    )?;
    let sha = text(&out.stdout);
    if out.status.success() && !sha.is_empty() {
        Ok(sha)
    } else {
        Err(WorkspaceError::new(
            ErrorKind::Config,
            format!("base_ref {base_ref:?} does not resolve to a commit"),
        ))
    }
}

/// Full SHA of `HEAD`.
pub(crate) fn head(dir: &Path) -> Result<String> {
    run(dir, &[OsStr::new("rev-parse"), OsStr::new("HEAD")]).map(|b| text(&b))
}

/// `(untracked, modified)` flags for the work tree. Ignored files are not reported.
pub(crate) fn status(dir: &Path) -> Result<(bool, bool)> {
    let out = run(
        dir,
        &[
            OsStr::new("status"),
            OsStr::new("--porcelain=v1"),
            OsStr::new("-z"),
            OsStr::new("--untracked-files=all"),
        ],
    )?;
    let mut untracked = false;
    let mut modified = false;
    let mut records = out.split(|b| *b == 0).filter(|r| !r.is_empty());
    while let Some(record) = records.next() {
        if record.len() < 3 {
            continue;
        }
        let (x, y) = (record[0], record[1]);
        if x == b'?' && y == b'?' {
            untracked = true;
        } else {
            modified = true;
        }
        // Renames and copies carry the original path as a following NUL-separated field.
        if x == b'R' || x == b'C' {
            records.next();
        }
    }
    Ok((untracked, modified))
}

/// Number of commits reachable from `tip` but not from `base`.
pub(crate) fn commits_between(dir: &Path, base: &str, tip: &str) -> Result<u64> {
    let range = format!("{base}..{tip}");
    let out = run(
        dir,
        &[
            OsStr::new("rev-list"),
            OsStr::new("--count"),
            OsStr::new(&range),
        ],
    )?;
    text(&out)
        .parse::<u64>()
        .map_err(|e| WorkspaceError::new(ErrorKind::Git, format!("rev-list count: {e}")))
}

/// Untracked, non-ignored paths relative to the work tree.
pub(crate) fn untracked_files(dir: &Path) -> Result<Vec<String>> {
    let out = run(
        dir,
        &[
            OsStr::new("ls-files"),
            OsStr::new("--others"),
            OsStr::new("--exclude-standard"),
            OsStr::new("-z"),
        ],
    )?;
    Ok(out
        .split(|b| *b == 0)
        .filter(|r| !r.is_empty())
        .map(|r| String::from_utf8_lossy(r).into_owned())
        .collect())
}

/// One row of `git diff --numstat`. `None` counts mean a binary file.
pub(crate) struct NumstatRow {
    pub path: String,
    pub insertions: Option<u64>,
    pub deletions: Option<u64>,
}

/// Tracked changes from `base` to the work tree, with per-file counts.
pub(crate) fn numstat(dir: &Path, base: &str) -> Result<Vec<NumstatRow>> {
    let out = run(
        dir,
        &[
            OsStr::new("diff"),
            OsStr::new("--numstat"),
            OsStr::new("--no-renames"),
            OsStr::new("--no-ext-diff"),
            OsStr::new("-z"),
            OsStr::new(base),
        ],
    )?;
    let mut rows = Vec::new();
    for record in out.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let line = String::from_utf8_lossy(record).into_owned();
        let mut parts = line.splitn(3, '\t');
        let (Some(ins), Some(del), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
            return Err(WorkspaceError::new(
                ErrorKind::Git,
                "unexpected numstat record",
            ));
        };
        rows.push(NumstatRow {
            path: path.to_string(),
            insertions: ins.parse().ok(),
            deletions: del.parse().ok(),
        });
    }
    Ok(rows)
}

/// Longest tracked path, in characters, at `commit`.
pub(crate) fn longest_tracked_name(dir: &Path, commit: &str) -> Result<usize> {
    let out = run(
        dir,
        &[
            OsStr::new("ls-tree"),
            OsStr::new("-r"),
            OsStr::new("-z"),
            OsStr::new("--name-only"),
            OsStr::new(commit),
        ],
    )?;
    Ok(out
        .split(|b| *b == 0)
        .filter(|r| !r.is_empty())
        .map(|r| String::from_utf8_lossy(r).chars().count())
        .max()
        .unwrap_or(0))
}

/// Whether `branch` exists as a local branch.
pub(crate) fn branch_exists(dir: &Path, branch: &str) -> Result<bool> {
    let full = format!("refs/heads/{branch}");
    let out = run_raw(
        dir,
        &[
            OsStr::new("show-ref"),
            OsStr::new("--verify"),
            OsStr::new("--quiet"),
            OsStr::new(&full),
        ],
    )?;
    Ok(out.status.success())
}

/// Adds a worktree at `path` on a new `branch` starting from `commit`.
pub(crate) fn worktree_add(repo: &Path, branch: &str, path: &Path, commit: &str) -> Result<()> {
    run(
        repo,
        &[
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("--quiet"),
            OsStr::new("-b"),
            OsStr::new(branch),
            path.as_os_str(),
            OsStr::new(commit),
        ],
    )
    .map(|_| ())
}

/// Removes a worktree registration and its files (`--force` allows ignored files).
pub(crate) fn worktree_remove(repo: &Path, path: &Path) -> Result<()> {
    run(
        repo,
        &[
            OsStr::new("worktree"),
            OsStr::new("remove"),
            OsStr::new("--force"),
            path.as_os_str(),
        ],
    )
    .map(|_| ())
}

/// Drops registrations of worktrees whose directories no longer exist.
pub(crate) fn worktree_prune(repo: &Path) -> Result<()> {
    run(repo, &[OsStr::new("worktree"), OsStr::new("prune")]).map(|_| ())
}

/// Deletes a local branch.
pub(crate) fn branch_delete(repo: &Path, branch: &str) -> Result<()> {
    run(
        repo,
        &[OsStr::new("branch"), OsStr::new("-D"), OsStr::new(branch)],
    )
    .map(|_| ())
}

/// Initializes a repository whose initial branch is `branch`.
pub(crate) fn init_repo(dir: &Path, branch: &str) -> Result<()> {
    run(
        dir,
        &[
            OsStr::new("init"),
            OsStr::new("--quiet"),
            OsStr::new("-b"),
            OsStr::new(branch),
        ],
    )
    .map(|_| ())
}

/// Stages every file, including ignored ones, for the copy baseline.
pub(crate) fn add_all(dir: &Path) -> Result<()> {
    run(
        dir,
        &[
            OsStr::new("add"),
            OsStr::new("-A"),
            OsStr::new("-f"),
            OsStr::new("."),
        ],
    )
    .map(|_| ())
}

/// Creates the copy baseline commit.
pub(crate) fn commit_baseline(dir: &Path) -> Result<()> {
    run(
        dir,
        &[
            OsStr::new("commit"),
            OsStr::new("--quiet"),
            OsStr::new("--allow-empty"),
            OsStr::new("--no-verify"),
            OsStr::new("-m"),
            OsStr::new("a2abus baseline"),
        ],
    )
    .map(|_| ())
}

/// Starts `git diff --binary <base>` with stdout piped. Stderr is discarded; the exit status
/// carries failure.
pub(crate) fn spawn_diff(dir: &Path, base: &str) -> Result<Child> {
    let mut cmd = command(
        dir,
        &[
            OsStr::new("diff"),
            OsStr::new("--binary"),
            OsStr::new("--no-ext-diff"),
            OsStr::new("--no-color"),
            OsStr::new("--no-renames"),
            OsStr::new(base),
        ],
    );
    cmd.stderr(Stdio::null());
    cmd.spawn()
        .map_err(|e| WorkspaceError::new(ErrorKind::Git, format!("cannot start git diff: {e}")))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn env_value(cmd: &Command, key: &str) -> Option<String> {
        cmd.get_envs().find_map(|(k, v)| {
            (k == OsStr::new(key)).then(|| {
                v.map(|v| v.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
        })
    }

    // [0012-039] no prompts, no system or global config, fixed cwd, argv not shell-parsed.
    #[test]
    fn req_0012_039_invocation_is_hardened() {
        let dir = std::env::temp_dir();
        let cmd = command(&dir, &[OsStr::new("status")]);
        assert_eq!(env_value(&cmd, "GIT_TERMINAL_PROMPT").as_deref(), Some("0"));
        assert_eq!(env_value(&cmd, "GIT_CONFIG_NOSYSTEM").as_deref(), Some("1"));
        assert_eq!(
            env_value(&cmd, "GIT_CONFIG_GLOBAL").as_deref(),
            Some(null_device())
        );
        assert_eq!(cmd.get_current_dir(), Some(dir.as_path()));
        let args: Vec<String> = argv(&[OsStr::new("status")])
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&format!("core.hooksPath={}", null_device())));
        assert_eq!(args.last().map(String::as_str), Some("status"));
    }

    // [0012-037] Windows invocations force long paths; other platforms do not need it.
    #[test]
    fn req_0012_037_longpaths_flag_only_on_windows() {
        let args: Vec<String> = argv(&[OsStr::new("status")])
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args.contains(&"core.longpaths=true".to_string()),
            cfg!(windows)
        );
    }
}
