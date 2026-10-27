//! Copy mode for non-git sources (RFC-0012 0012-034, 0012-073, 0012-074).
//!
//! The walk never follows links and writes nothing. Excluded entries (built-in denylist and the
//! operator's `copy_exclude`) are counted and never copied. The copy is then built in three steps:
//! `.gitignore` files are copied first so git can read them, `git check-ignore` decides which
//! candidate files are ignored, and only the remaining files are copied. Limits and destination
//! path budgets apply to that final set. Directories are created only as parents of copied files,
//! so an ignored or excluded tree leaves no trace in the copy. The copy starts as a fresh baseline
//! repository.

use std::fs;
use std::path::Path;

use crate::config::CopyLimits;
use crate::error::{ErrorKind, Result, WorkspaceError, io_err};
use crate::exclude::{Excludes, base_name};
use crate::git;
use crate::platform;

/// Name of the ignore file whose rules decide which copied files are ignored.
const GITIGNORE: &str = ".gitignore";

/// One regular file to copy. `rel` is `/`-separated and relative to the source root.
pub(crate) struct CopyFile {
    rel: String,
    bytes: u64,
}

/// What the source walk found. Nothing has been written yet.
pub(crate) struct CopyPlan {
    /// Candidate files after the denylist and `copy_exclude` have been applied.
    files: Vec<CopyFile>,
    /// Entries removed by the denylist or `copy_exclude`, counting every file below an
    /// excluded directory.
    excluded: usize,
}

/// Result of building a copy.
pub(crate) struct Created {
    /// Baseline commit the workspace starts from.
    pub(crate) base_commit: String,
    /// Entries excluded by any rule: denylist, `copy_exclude`, or git ignore rules.
    pub(crate) excluded: usize,
}

/// Walks `source` without following links. Excluded entries are counted and skipped before any
/// link or special-file check, so an excluded entry is never copied and never rejected. Nothing
/// is written.
pub(crate) fn plan(source: &Path, excludes: &Excludes) -> Result<CopyPlan> {
    let mut plan = CopyPlan {
        files: Vec::new(),
        excluded: 0,
    };
    let mut stack = vec![String::new()];
    while let Some(dir) = stack.pop() {
        let entries =
            fs::read_dir(source.join(&dir)).map_err(|e| io_err("read source directory", e))?;
        for entry in entries {
            let entry = entry.map_err(|e| io_err("read source directory", e))?;
            let name = entry
                .file_name()
                .to_str()
                .ok_or_else(|| {
                    WorkspaceError::new(
                        ErrorKind::Config,
                        "copy source contains a non-UTF-8 file name",
                    )
                })?
                .to_string();
            let rel = if dir.is_empty() {
                name
            } else {
                format!("{dir}/{name}")
            };
            let meta = fs::symlink_metadata(entry.path())
                .map_err(|e| io_err("inspect source entry", e))?;
            let link = platform::is_link(&meta);
            let is_dir = !link && meta.is_dir();
            let excluded = if is_dir {
                excludes.dir_excluded(&rel)
            } else {
                excludes.file_excluded(&rel)
            };
            if excluded {
                plan.excluded += if is_dir {
                    count_excluded_files(&entry.path())?
                } else {
                    1
                };
                continue;
            }
            if link {
                return Err(link_error(&rel, "link or reparse point"));
            }
            if is_dir {
                stack.push(rel);
            } else if meta.is_file() {
                plan.files.push(CopyFile {
                    rel,
                    bytes: meta.len(),
                });
            } else {
                return Err(link_error(&rel, "special file"));
            }
        }
    }
    Ok(plan)
}

/// Builds the copy at `dest` and its baseline commit. On success the workspace contains exactly
/// the files that survive every exclusion rule. Limits and path budgets are checked on that set
/// before the remaining files are copied.
pub(crate) fn create(
    source: &Path,
    dest: &Path,
    branch: &str,
    plan: CopyPlan,
    limits: CopyLimits,
) -> Result<Created> {
    let budget = platform::entry_path_budget();
    fs::create_dir_all(dest).map_err(|e| io_err("create copy directory", e))?;
    git::init_repo(dest, branch)?;

    let mut excluded = plan.excluded;
    let (mut rules, mut pending): (Vec<CopyFile>, Vec<CopyFile>) =
        plan.files.into_iter().partition(is_gitignore);
    for file in &rules {
        copy_kept(source, dest, file, budget)?;
    }
    if !rules.is_empty() {
        let ignored = {
            let candidates: Vec<&str> = rules
                .iter()
                .chain(pending.iter())
                .map(|f| f.rel.as_str())
                .collect();
            git::check_ignore(dest, &candidates)?
        };
        if !ignored.is_empty() {
            let before = rules.len() + pending.len();
            for file in rules.iter().filter(|f| ignored.contains(&f.rel)) {
                fs::remove_file(dest.join(&file.rel))
                    .map_err(|e| io_err("remove ignored file", e))?;
            }
            rules.retain(|f| !ignored.contains(&f.rel));
            pending.retain(|f| !ignored.contains(&f.rel));
            excluded += before - rules.len() - pending.len();
        }
    }

    let kept: Vec<&CopyFile> = rules.iter().chain(pending.iter()).collect();
    check_limits(&kept, limits)?;
    for file in &pending {
        copy_kept(source, dest, file, budget)?;
    }

    git::add_all(dest)?;
    git::commit_baseline(dest)?;
    let base_commit = git::head(dest)?;
    Ok(Created {
        base_commit,
        excluded,
    })
}

fn is_gitignore(file: &CopyFile) -> bool {
    base_name(&file.rel) == GITIGNORE
}

/// Copies one file that survived every rule, creating its parent directories. The destination
/// path budget is checked first.
fn copy_kept(source: &Path, dest: &Path, file: &CopyFile, budget: usize) -> Result<()> {
    let target = dest.join(&file.rel);
    platform::check_budget(&target, budget, "copied path")?;
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|e| io_err("create copied directory", e))?;
    }
    fs::copy(source.join(&file.rel), &target)
        .map(|_| ())
        .map_err(|e| io_err("copy file", e))
}

/// Enforces `copy_limits` on the files that will be copied.
fn check_limits(files: &[&CopyFile], limits: CopyLimits) -> Result<()> {
    let count = u64::try_from(files.len()).unwrap_or(u64::MAX);
    if count > limits.max_files {
        return Err(limit_error(format!(
            "source has more than {} files",
            limits.max_files
        )));
    }
    let bytes = files
        .iter()
        .fold(0u64, |acc, file| acc.saturating_add(file.bytes));
    if bytes > limits.max_bytes {
        return Err(limit_error(format!(
            "source exceeds {} bytes",
            limits.max_bytes
        )));
    }
    Ok(())
}

/// Counts the regular files below an excluded directory. Links are neither followed nor copied,
/// and they are not counted.
fn count_excluded_files(dir: &Path) -> Result<usize> {
    let mut count = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = fs::read_dir(&current).map_err(|e| io_err("read excluded directory", e))?;
        for entry in entries {
            let entry = entry.map_err(|e| io_err("read excluded directory", e))?;
            let meta = fs::symlink_metadata(entry.path())
                .map_err(|e| io_err("inspect excluded entry", e))?;
            if platform::is_link(&meta) {
                continue;
            }
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                count += 1;
            }
        }
    }
    Ok(count)
}

fn link_error(rel: &str, what: &str) -> WorkspaceError {
    WorkspaceError::new(
        ErrorKind::CopyLinkRejected,
        format!("{rel} is a {what}; copy mode refuses it"),
    )
}

fn limit_error(detail: String) -> WorkspaceError {
    WorkspaceError::new(ErrorKind::CopyLimit, detail)
}
