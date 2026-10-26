//! Copy mode for non-git sources (RFC-0012 0012-034). The walk never follows links, limits and
//! path budgets are checked before anything is written, and the copy starts as a fresh baseline
//! repository.

use std::fs;
use std::path::{Path, PathBuf};

use crate::config::CopyLimits;
use crate::error::{ErrorKind, Result, WorkspaceError, io_err};
use crate::platform;

/// Entries to copy, relative to the source root.
pub(crate) struct CopyPlan {
    dirs: Vec<PathBuf>,
    files: Vec<PathBuf>,
}

/// Walks `source` without following links and checks limits and destination path budgets.
/// Nothing is written.
pub(crate) fn plan(source: &Path, dest: &Path, limits: CopyLimits) -> Result<CopyPlan> {
    let budget = platform::entry_path_budget();
    let mut plan = CopyPlan {
        dirs: Vec::new(),
        files: Vec::new(),
    };
    let mut bytes: u64 = 0;
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let entries =
            fs::read_dir(source.join(&rel)).map_err(|e| io_err("read source directory", e))?;
        for entry in entries {
            let entry = entry.map_err(|e| io_err("read source directory", e))?;
            let child = rel.join(entry.file_name());
            let meta = fs::symlink_metadata(entry.path())
                .map_err(|e| io_err("inspect source entry", e))?;
            if platform::is_link(&meta) {
                return Err(link_error(&child, "link or reparse point"));
            }
            platform::check_budget(&dest.join(&child), budget, "copied path")?;
            if meta.is_dir() {
                plan.dirs.push(child.clone());
                stack.push(child);
            } else if meta.is_file() {
                bytes = bytes.saturating_add(meta.len());
                plan.files.push(child);
                let files = u64::try_from(plan.files.len()).unwrap_or(u64::MAX);
                if files > limits.max_files {
                    return Err(limit_error(format!(
                        "source has more than {} files",
                        limits.max_files
                    )));
                }
                if bytes > limits.max_bytes {
                    return Err(limit_error(format!(
                        "source exceeds {} bytes",
                        limits.max_bytes
                    )));
                }
            } else {
                return Err(link_error(&child, "special file"));
            }
        }
    }
    Ok(plan)
}

/// Creates `dest` and copies the planned entries into it.
pub(crate) fn materialize(source: &Path, dest: &Path, plan: &CopyPlan) -> Result<()> {
    fs::create_dir_all(dest).map_err(|e| io_err("create copy directory", e))?;
    for dir in &plan.dirs {
        fs::create_dir_all(dest.join(dir)).map_err(|e| io_err("create copied directory", e))?;
    }
    for file in &plan.files {
        fs::copy(source.join(file), dest.join(file)).map_err(|e| io_err("copy file", e))?;
    }
    Ok(())
}

fn link_error(path: &Path, what: &str) -> WorkspaceError {
    WorkspaceError::new(
        ErrorKind::CopyLinkRejected,
        format!("{} is a {what}; copy mode refuses it", path.display()),
    )
}

fn limit_error(detail: String) -> WorkspaceError {
    WorkspaceError::new(ErrorKind::CopyLimit, detail)
}
