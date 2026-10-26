//! `<root>/index.json`: records of retained workspaces (RFC-0012 0012-035). Written atomically
//! while the registry mutex is held.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{ErrorKind, Result, WorkspaceError, io_err};
use crate::lease::LeaseRecord;
use crate::platform;

pub(crate) const INDEX_FILE: &str = "index.json";
const VERSION: u32 = 1;

/// A retained workspace and the reasons it was kept.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RetainedEntry {
    #[serde(flatten)]
    pub lease: LeaseRecord,
    pub reasons: Vec<String>,
    pub retained_at_ms: u64,
    pub bytes: u64,
}

#[derive(Serialize)]
struct IndexOut<'a> {
    version: u32,
    entries: &'a [RetainedEntry],
}

#[derive(Deserialize)]
struct IndexIn {
    version: u32,
    entries: Vec<RetainedEntry>,
}

pub(crate) fn load(root: &Path) -> Result<Vec<RetainedEntry>> {
    let path = root.join(INDEX_FILE);
    match fs::read(&path) {
        Ok(bytes) => {
            let file: IndexIn = serde_json::from_slice(&bytes).map_err(|e| {
                WorkspaceError::new(ErrorKind::StateInvalid, format!("index.json: {e}"))
            })?;
            if file.version != VERSION {
                return Err(WorkspaceError::new(
                    ErrorKind::StateInvalid,
                    format!("index.json version {} is not supported", file.version),
                ));
            }
            Ok(file.entries)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(io_err("read index.json", e)),
    }
}

pub(crate) fn save(root: &Path, entries: &[RetainedEntry]) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(&IndexOut {
        version: VERSION,
        entries,
    })
    .map_err(|e| WorkspaceError::new(ErrorKind::StateInvalid, e.to_string()))?;
    platform::write_atomic(&root.join(INDEX_FILE), &bytes)
        .map_err(|e| io_err("write index.json", e))
}
