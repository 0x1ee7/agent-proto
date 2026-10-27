//! Durable append-only [`TaskStore`] over one file (RFC-0005 0005-015).
//!
//! Each record is a 4-byte big-endian length followed by the record bytes. An
//! append writes the frame and calls `sync_data` before it returns, so a
//! successful append is durable. A torn trailing frame left by a crash during an
//! append was never acknowledged, and is ignored on read.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use a2a_core::task::{StoreError, TaskStore};

/// File-backed task store. Blocking I/O: call from a blocking context in async hosts.
#[derive(Debug)]
pub struct FileStore {
    path: PathBuf,
    file: File,
}

impl FileStore {
    /// Opens or creates the store file at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&path)
            .map_err(|e| StoreError(format!("open {}: {e}", path.display())))?;
        Ok(Self { path, file })
    }

    /// The file path of this store.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl TaskStore for FileStore {
    fn append(&mut self, record: &[u8]) -> Result<(), StoreError> {
        let len = u32::try_from(record.len())
            .map_err(|_| StoreError("record exceeds 4 GiB".to_owned()))?;
        let mut frame = Vec::with_capacity(4 + record.len());
        frame.extend_from_slice(&len.to_be_bytes());
        frame.extend_from_slice(record);
        self.file
            .write_all(&frame)
            .and_then(|()| self.file.sync_data())
            .map_err(|e| StoreError(format!("append: {e}")))
    }

    fn records(&self) -> Result<Vec<Vec<u8>>, StoreError> {
        let mut file = File::open(&self.path)
            .map_err(|e| StoreError(format!("read {}: {e}", self.path.display())))?;
        file.seek(SeekFrom::Start(0))
            .map_err(|e| StoreError(format!("seek: {e}")))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|e| StoreError(format!("read: {e}")))?;
        Ok(split_frames(&bytes))
    }
}

/// Splits concatenated frames. A trailing partial frame is dropped (never acknowledged).
fn split_frames(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut rest = bytes;
    while let Some((header, tail)) = rest.split_first_chunk::<4>() {
        let len = u32::from_be_bytes(*header) as usize;
        if tail.len() < len {
            break;
        }
        let (record, after) = tail.split_at(len);
        out.push(record.to_vec());
        rest = after;
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("a2a-worker-store-{}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&dir);
        dir
    }

    /// Guards 0005-015: appended records are returned in order after reopen.
    #[test]
    fn records_survive_reopen_in_order() {
        let path = temp_path("order");
        {
            let mut store = FileStore::open(&path).expect("open");
            store.append(b"one").expect("append");
            store.append(b"two").expect("append");
        }
        let store = FileStore::open(&path).expect("reopen");
        let records = store.records().expect("records");
        assert_eq!(records, vec![b"one".to_vec(), b"two".to_vec()]);
        let _ = std::fs::remove_file(&path);
    }

    /// Guards 0005-015: a torn trailing frame is ignored, earlier records remain.
    #[test]
    fn torn_trailing_frame_is_ignored() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&3u32.to_be_bytes());
        bytes.extend_from_slice(b"abc");
        bytes.extend_from_slice(&9u32.to_be_bytes());
        bytes.extend_from_slice(b"xy");
        assert_eq!(split_frames(&bytes), vec![b"abc".to_vec()]);
    }
}
