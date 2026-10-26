//! Path normalization, containment, length budgets, link-aware removal and atomic file writes.

use std::fs::{self, Metadata, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{ErrorKind, Result, WorkspaceError, io_err};

/// RFC-0012 0012-037: workspace paths on Windows stay within 180 characters.
const WINDOWS_WORKSPACE_BUDGET: usize = 180;
/// Budget for entries copied or checked out inside a workspace on Windows (MAX_PATH).
const WINDOWS_ENTRY_BUDGET: usize = 260;
/// Linux PATH_MAX.
const POSIX_PATH_MAX: usize = 4096;
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// Budget for workspace roots and lease paths on this platform.
pub(crate) fn workspace_path_budget() -> usize {
    if cfg!(windows) {
        WINDOWS_WORKSPACE_BUDGET
    } else {
        POSIX_PATH_MAX
    }
}

/// Budget for entries inside a workspace on this platform.
pub(crate) fn entry_path_budget() -> usize {
    if cfg!(windows) {
        WINDOWS_ENTRY_BUDGET
    } else {
        POSIX_PATH_MAX
    }
}

/// Length of a path in platform units (UTF-16 units on Windows, bytes elsewhere).
pub(crate) fn path_len(path: &Path) -> usize {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        path.as_os_str().encode_wide().count()
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().len()
    }
}

/// Fails with `path_too_long` when `path` is longer than `budget`.
pub(crate) fn check_budget(path: &Path, budget: usize, what: &str) -> Result<()> {
    let len = path_len(path);
    if len <= budget {
        Ok(())
    } else {
        Err(WorkspaceError::new(
            ErrorKind::PathTooLong,
            format!("{what} is {len} characters, over the {budget}-character budget"),
        ))
    }
}

/// Absolute, `..`-free path with symlinks and junctions resolved for the existing prefix and
/// without the Windows verbatim prefix.
pub(crate) fn normalize(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        return Err(WorkspaceError::new(
            ErrorKind::Config,
            format!("path must be absolute: {}", path.display()),
        ));
    }
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(WorkspaceError::new(
            ErrorKind::PathEscape,
            format!("path contains '..': {}", path.display()),
        ));
    }
    for ancestor in path.ancestors() {
        if ancestor.exists() {
            let base = fs::canonicalize(ancestor).map_err(|e| io_err("canonicalize path", e))?;
            let rest = path
                .strip_prefix(ancestor)
                .unwrap_or_else(|_| Path::new(""));
            let base = strip_verbatim(&base);
            return Ok(if rest.as_os_str().is_empty() {
                base
            } else {
                base.join(rest)
            });
        }
    }
    Err(WorkspaceError::new(
        ErrorKind::Config,
        format!("no existing ancestor for {}", path.display()),
    ))
}

#[cfg(windows)]
fn strip_verbatim(path: &Path) -> PathBuf {
    use std::path::Prefix;

    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return path.to_path_buf();
    };
    let head = match prefix.kind() {
        Prefix::VerbatimDisk(letter) => format!("{}:\\", char::from(letter)),
        Prefix::VerbatimUNC(server, share) => {
            format!(
                "\\\\{}\\{}\\",
                server.to_string_lossy(),
                share.to_string_lossy()
            )
        }
        _ => return path.to_path_buf(),
    };
    let mut out = PathBuf::from(head);
    for component in components {
        if let Component::Normal(name) = component {
            out.push(name);
        }
    }
    out
}

#[cfg(not(windows))]
fn strip_verbatim(path: &Path) -> PathBuf {
    path.to_path_buf()
}

/// Comparison form of a normalized path: lowercase on Windows (case-insensitive file system).
fn comparable(path: &Path) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(path.to_string_lossy().to_lowercase())
    } else {
        path.to_path_buf()
    }
}

/// True when the two normalized paths are equal, or one contains the other.
pub(crate) fn overlaps(a: &Path, b: &Path) -> bool {
    let (a, b) = (comparable(a), comparable(b));
    a.starts_with(&b) || b.starts_with(&a)
}

/// True for symbolic links, junctions and other reparse points.
pub(crate) fn is_link(meta: &Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Removes `path` (a directory tree) if it lies strictly inside `root`. Never follows links:
/// a link is removed as a link. Read-only entries are made writable first.
pub(crate) fn remove_tree_within(root: &Path, path: &Path) -> Result<()> {
    let root = normalize(root)?;
    if let Ok(meta) = fs::symlink_metadata(path) {
        if is_link(&meta) {
            return Err(WorkspaceError::new(
                ErrorKind::PathEscape,
                format!("refusing to remove top-level link {}", path.display()),
            ));
        }
    }
    let target = normalize(path)?;
    let inside = comparable(&target).starts_with(comparable(&root)) && target != root;
    if !inside {
        return Err(WorkspaceError::new(
            ErrorKind::PathEscape,
            format!("{} is outside the workspace root", path.display()),
        ));
    }
    match remove_entry(&target) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_err("remove workspace", e)),
    }
}

fn remove_entry(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if is_link(&meta) {
        return remove_link(path);
    }
    if meta.is_dir() {
        for entry in fs::read_dir(path)? {
            remove_entry(&entry?.path())?;
        }
        make_writable(path)?;
        fs::remove_dir(path)
    } else {
        make_writable(path)?;
        fs::remove_file(path)
    }
}

/// Removes a link without touching its target. Windows directory links and junctions need
/// `remove_dir`; file symlinks and Unix symlinks take `remove_file`.
fn remove_link(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(_) => fs::remove_dir(path),
    }
}

/// Makes a read-only entry writable so it can be deleted. On Unix only the owner write bit is
/// added; `set_readonly(false)` would make the entry world-writable.
#[allow(clippy::permissions_set_readonly_false)]
fn make_writable(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    let mut perms = meta.permissions();
    if !perms.readonly() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(perms.mode() | 0o200);
    }
    #[cfg(not(unix))]
    perms.set_readonly(false);
    fs::set_permissions(path, perms)
}

/// True if any entry under `root` is a link or reparse point. Does not follow links.
pub(crate) fn tree_has_links(root: &Path) -> io::Result<bool> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let meta = fs::symlink_metadata(&path)?;
        if is_link(&meta) {
            return Ok(true);
        }
        if meta.is_dir() {
            for entry in fs::read_dir(&path)? {
                stack.push(entry?.path());
            }
        }
    }
    Ok(false)
}

/// Sum of regular file sizes under `root`. Does not follow links.
pub(crate) fn dir_size(root: &Path) -> io::Result<u64> {
    let mut total: u64 = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let meta = fs::symlink_metadata(&path)?;
        if is_link(&meta) {
            continue;
        }
        if meta.is_dir() {
            for entry in fs::read_dir(&path)? {
                stack.push(entry?.path());
            }
        } else {
            total = total.saturating_add(meta.len());
        }
    }
    Ok(total)
}

/// Creates `path` with `bytes` only if it does not exist. The content is complete before the
/// name becomes visible (temporary file plus hard link). Returns `false` when it already exists.
pub(crate) fn publish_exclusive(path: &Path, bytes: &[u8]) -> io::Result<bool> {
    let tmp = sibling_tmp(path);
    let result = write_synced(&tmp, bytes).and_then(|()| fs::hard_link(&tmp, path));
    let _ = fs::remove_file(&tmp);
    match result {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e),
    }
}

/// Replaces `path` atomically with `bytes`.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = sibling_tmp(path);
    let result = write_synced(&tmp, bytes).and_then(|()| fs::rename(&tmp, path));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn write_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn sibling_tmp(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    path.with_file_name(format!("{name}.tmp-{}", unique_suffix()))
}

fn unique_suffix() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(
        "{}-{nanos}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Milliseconds since the Unix epoch, saturating at zero before it.
pub(crate) fn system_time_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Current time in milliseconds since the Unix epoch.
pub(crate) fn now_ms() -> u64 {
    system_time_ms(SystemTime::now())
}

/// Null device for hook and config redirection.
pub(crate) fn null_device() -> &'static str {
    if cfg!(windows) { "NUL" } else { "/dev/null" }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    // [0012-037] a path one character over the budget is refused, the budget itself is allowed.
    #[test]
    fn req_0012_037_length_budget_boundary() {
        let at = PathBuf::from("a".repeat(180));
        let over = PathBuf::from("a".repeat(181));
        assert!(check_budget(&at, 180, "p").is_ok());
        let err = check_budget(&over, 180, "p").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::PathTooLong);
    }

    #[test]
    fn overlaps_detects_equal_contained_and_containing_paths() {
        let base = Path::new("/w/ws/t/a/x");
        assert!(overlaps(base, base));
        assert!(overlaps(base, Path::new("/w/ws/t/a")));
        assert!(overlaps(Path::new("/w/ws/t/a"), base));
        assert!(!overlaps(base, Path::new("/w/ws/t/a/y")));
        assert!(!overlaps(Path::new("/w/ws/t/ab"), Path::new("/w/ws/t/a")));
    }

    #[test]
    fn normalize_rejects_relative_and_parent_components() {
        assert_eq!(
            normalize(Path::new("relative/x")).unwrap_err().kind(),
            ErrorKind::Config
        );
        let dotted = std::env::temp_dir().join("..").join("x");
        assert_eq!(
            normalize(&dotted).unwrap_err().kind(),
            ErrorKind::PathEscape
        );
    }

    #[test]
    fn publish_exclusive_refuses_existing_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        assert!(publish_exclusive(&path, b"one").unwrap());
        assert!(!publish_exclusive(&path, b"two").unwrap());
        assert_eq!(fs::read(&path).unwrap(), b"one");
    }
}
