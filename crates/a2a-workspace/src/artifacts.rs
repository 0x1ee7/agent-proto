//! Diff artifacts (RFC-0012 0012-040..0012-042): the work a worker produced, written under the
//! artifact root and summarized without absolute paths.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::config::PATCH_TRUNCATED_MARKER;
use crate::error::{ErrorKind, Result, WorkspaceError, io_err};
use crate::git;
use crate::ids::validate_task_id;
use crate::lease::LeaseRecord;

const PATCH_FILE: &str = "diff.patch";
const PARTIAL_FILE: &str = "diff.patch.partial";
const CHUNK: usize = 64 * 1024;

/// Per-file change counts; `None` marks a binary file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileSummary {
    /// Path relative to the workspace root.
    pub path: String,
    /// Inserted lines, or `None` for binary files.
    pub insertions: Option<u64>,
    /// Deleted lines, or `None` for binary files.
    pub deletions: Option<u64>,
}

/// Summary of a written diff artifact. Contains no absolute paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArtifactSummary {
    /// Task the artifact belongs to.
    pub task_id: String,
    /// `<task_id>/diff.patch`, relative to the artifact root.
    pub relative_path: String,
    /// Tracked files changed relative to the base commit.
    pub files: Vec<FileSummary>,
    /// Untracked, non-ignored files. Their paths are listed at the end of the patch.
    pub untracked: Vec<String>,
    /// Total inserted lines over text files.
    pub insertions: u64,
    /// Total deleted lines over text files.
    pub deletions: u64,
    /// Lowercase hex SHA-256 of the patch file as written.
    pub sha256: String,
    /// Size of the patch file in bytes.
    pub patch_bytes: u64,
    /// True when the patch was cut at the artifact cap.
    pub truncated: bool,
}

impl ArtifactSummary {
    /// Plain-text rendering for operator and peer display.
    pub fn render_text(&self) -> String {
        let mut out = format!(
            "{} ({} bytes, sha256 {})",
            self.relative_path, self.patch_bytes, self.sha256
        );
        if self.truncated {
            out.push_str(" [truncated]");
        }
        out.push('\n');
        out.push_str(&format!(
            "{} files, +{} -{}\n",
            self.files.len(),
            self.insertions,
            self.deletions
        ));
        for file in &self.files {
            match (file.insertions, file.deletions) {
                (Some(ins), Some(del)) => out.push_str(&format!("  {} +{ins} -{del}\n", file.path)),
                _ => out.push_str(&format!("  {} (binary)\n", file.path)),
            }
        }
        for path in &self.untracked {
            out.push_str(&format!("  {path} (untracked)\n"));
        }
        out
    }
}

/// Writes `<artifacts_root>/<task_id>/diff.patch` for the lease described by `record`.
///
/// The patch is `git diff --binary` against the base commit followed by one
/// `# untracked: <path>` line per untracked file. Output is capped at `cap` bytes: the diff is
/// cut at the last newline that fits, the untracked block is kept, and the
/// [`PATCH_TRUNCATED_MARKER`] line is appended.
pub(crate) fn write(
    record: &LeaseRecord,
    artifacts_root: &Path,
    task_id: &str,
    cap: u64,
) -> Result<ArtifactSummary> {
    validate_task_id(task_id)?;
    if !record.path.is_dir() {
        return Err(WorkspaceError::new(
            ErrorKind::LeaseLost,
            "leased workspace is missing",
        ));
    }
    let rows = git::numstat(&record.path, &record.base_commit)?;
    let untracked = git::untracked_files(&record.path)?;
    let mut block: String = untracked
        .iter()
        .map(|p| format!("# untracked: {p}\n"))
        .collect();
    let marker = format!("{PATCH_TRUNCATED_MARKER}\n");
    let mut truncated = false;
    let block_budget = usize::try_from(cap / 2).unwrap_or(usize::MAX);
    if block.len() > block_budget {
        block = fit_lines(&block, block_budget);
        truncated = true;
    }
    let limit = cap.saturating_sub(block.len() as u64 + marker.len() as u64);

    let dir = artifacts_root.join(task_id);
    fs::create_dir_all(&dir).map_err(|e| io_err("create artifact directory", e))?;
    let partial = dir.join(PARTIAL_FILE);
    if stream_diff(&record.path, &record.base_commit, &partial, limit)? {
        cut_at_last_newline(&partial)?;
        truncated = true;
    }
    {
        let mut file = OpenOptions::new()
            .append(true)
            .open(&partial)
            .map_err(|e| io_err("open patch", e))?;
        file.write_all(block.as_bytes())
            .map_err(|e| io_err("write untracked block", e))?;
        if truncated {
            file.write_all(marker.as_bytes())
                .map_err(|e| io_err("write patch marker", e))?;
        }
        file.sync_all().map_err(|e| io_err("sync patch", e))?;
    }
    let final_path = dir.join(PATCH_FILE);
    fs::rename(&partial, &final_path).map_err(|e| io_err("publish patch", e))?;
    let patch_bytes = fs::metadata(&final_path)
        .map_err(|e| io_err("stat patch", e))?
        .len();
    let sha256 = sha256_file(&final_path)?;

    let files: Vec<FileSummary> = rows
        .into_iter()
        .map(|row| FileSummary {
            path: row.path,
            insertions: row.insertions,
            deletions: row.deletions,
        })
        .collect();
    let insertions: u64 = files.iter().filter_map(|f| f.insertions).sum();
    let deletions: u64 = files.iter().filter_map(|f| f.deletions).sum();
    Ok(ArtifactSummary {
        task_id: task_id.to_string(),
        relative_path: format!("{task_id}/{PATCH_FILE}"),
        files,
        untracked,
        insertions,
        deletions,
        sha256,
        patch_bytes,
        truncated,
    })
}

/// Streams `git diff --binary <base>` into `partial`, stopping after `limit` bytes. Returns
/// true when the diff was cut short.
fn stream_diff(work: &Path, base: &str, partial: &Path, limit: u64) -> Result<bool> {
    let mut child = git::spawn_diff(work, base)?;
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(WorkspaceError::new(
            ErrorKind::Git,
            "git diff produced no stdout pipe",
        ));
    };
    let mut file = File::create(partial).map_err(|e| io_err("create patch", e))?;
    let mut buf = vec![0u8; CHUNK];
    let mut written: u64 = 0;
    let mut truncated = false;
    loop {
        let n = stdout
            .read(&mut buf)
            .map_err(|e| io_err("read git diff", e))?;
        if n == 0 {
            break;
        }
        let room = limit - written;
        if n as u64 > room {
            file.write_all(&buf[..room as usize])
                .map_err(|e| io_err("write patch", e))?;
            truncated = true;
            break;
        }
        file.write_all(&buf[..n])
            .map_err(|e| io_err("write patch", e))?;
        written += n as u64;
    }
    drop(stdout);
    if truncated {
        let _ = child.kill();
    }
    let status = child.wait().map_err(|e| io_err("wait for git diff", e))?;
    file.sync_all().map_err(|e| io_err("sync patch", e))?;
    if !truncated && !status.success() {
        return Err(WorkspaceError::new(
            ErrorKind::Git,
            format!("git diff failed ({status})"),
        ));
    }
    Ok(truncated)
}

fn cut_at_last_newline(path: &Path) -> Result<()> {
    let bytes = fs::read(path).map_err(|e| io_err("read partial patch", e))?;
    let keep = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|e| io_err("open partial patch", e))?;
    file.set_len(keep as u64)
        .map_err(|e| io_err("truncate patch", e))
}

/// Longest prefix of whole lines of `block` that fits in `max` bytes.
fn fit_lines(block: &str, max: usize) -> String {
    let bytes = block.as_bytes();
    let window = &bytes[..max.min(bytes.len())];
    let end = window
        .iter()
        .rposition(|b| *b == b'\n')
        .map_or(0, |i| i + 1);
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).map_err(|e| io_err("open patch for hashing", e))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| io_err("read patch for hashing", e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}
