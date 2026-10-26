//! Owner-only audit files per platform (RFC-0013 0013-020). Linux uses mode 0600. Windows
//! replaces the DACL with a single grant to the current user and opens sink files with
//! share-read and share-delete so readers, rotation and retention are never blocked.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

/// Arguments of the `icacls` call that leaves only `user` with full control of `path`. Pure, so
/// the Windows argument list is unit-tested on every platform.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub(crate) fn owner_only_icacls_args(path: &Path, user: &str) -> Vec<OsString> {
    vec![
        path.as_os_str().to_owned(),
        OsString::from("/inheritance:r"),
        OsString::from("/grant:r"),
        OsString::from(format!("{user}:(F)")),
    ]
}

/// Restricts `path` to its owner.
#[cfg(unix)]
pub(crate) fn restrict_to_owner(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

/// Restricts `path` to the current user through `icacls`.
#[cfg(windows)]
pub(crate) fn restrict_to_owner(path: &Path) -> io::Result<()> {
    let user = std::env::var("USERNAME").map_err(io::Error::other)?;
    let status = std::process::Command::new("icacls")
        .args(owner_only_icacls_args(path, &user))
        .stdout(std::process::Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("icacls exited with {status}")))
    }
}

/// Creates a new file, failing if it exists, that only the owner can read and write (0013-020).
#[cfg(unix)]
pub(crate) fn create_owner_only(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// Windows variant: the DACL is replaced with the owner-only grant after creation.
#[cfg(windows)]
pub(crate) fn create_owner_only(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new().write(true).create_new(true).open(path)?;
    restrict_to_owner(path)?;
    Ok(file)
}

/// Opens an existing or new file for read and write, without truncation.
pub(crate) fn open_rw(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

/// Opens a sink file for appending. On Windows the handle allows concurrent readers and deletion.
#[cfg(unix)]
pub(crate) fn open_shared_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

/// Opens a sink file for appending with FILE_SHARE_READ | FILE_SHARE_DELETE (0013-020).
#[cfg(windows)]
pub(crate) fn open_shared_append(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_DELETE: u32 = 0x4;
    OpenOptions::new()
        .create(true)
        .append(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
        .open(path)
}

/// Flushes the directory entry of `path` so a rename survives power loss.
#[cfg(unix)]
pub(crate) fn sync_parent(path: &Path) -> io::Result<()> {
    match path.parent() {
        Some(dir) => File::open(dir)?.sync_all(),
        None => Ok(()),
    }
}

/// Directory fsync is not available on Windows; the rename is durable through the file system.
#[cfg(not(unix))]
pub(crate) fn sync_parent(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// 0013-020: the Windows ACL call grants only the named user, with inheritance removed.
    /// Checked on every platform, since the argument list is pure.
    #[test]
    fn req_0013_020_icacls_args_grant_only_the_owner() {
        let args = owner_only_icacls_args(Path::new("C:/state/audit/x.jsonl"), "alice");
        let text: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            text,
            vec![
                "C:/state/audit/x.jsonl",
                "/inheritance:r",
                "/grant:r",
                "alice:(F)"
            ]
        );
    }

    /// 0013-020: on Unix the restricted file is mode 0600.
    #[cfg(unix)]
    #[test]
    fn req_0013_020_unix_mode_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.jsonl");
        std::fs::write(&path, b"x").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        restrict_to_owner(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    /// 0013-020: a reader can open a sink file while the sink holds it open for writing.
    #[test]
    fn req_0013_020_reader_opens_file_while_writer_holds_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events-20261026-1.jsonl");
        let writer = open_shared_append(&path).unwrap();
        let reader = File::open(&path);
        assert!(reader.is_ok());
        drop(writer);
    }
}
