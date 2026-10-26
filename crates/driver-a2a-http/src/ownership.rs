//! Per-origin visibility of peer task ids (RFC-0009 0009-077; RFC-0005 0005-052).
//!
//! A task id is recorded only when the peer returns it in a reply to this driver's own
//! SendMessage or SendStreamingMessage for an origin. Lookups (GetTask, SubscribeToTask,
//! CancelTask) are sent only for task ids the requesting origin owns. The map is persisted to the
//! operator-configured `task_map_path` so ownership survives a restart. The file is rewritten
//! atomically before a new id is committed in memory, so a failed write records nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Failure to load, persist, or extend the task map.
#[derive(Debug, thiserror::Error)]
pub enum TaskMapError {
    /// The map file exists but cannot be read.
    #[error("cannot read the task map file")]
    Read(#[source] io::Error),
    /// The map file is not a valid task map.
    #[error("the task map file is not valid")]
    Parse(#[source] serde_json::Error),
    /// The map file cannot be written.
    #[error("cannot write the task map file")]
    Write(#[source] io::Error),
    /// The task id already belongs to a different origin.
    #[error("task id belongs to another origin")]
    Foreign,
}

#[derive(Deserialize)]
struct Document {
    origins: BTreeMap<String, BTreeSet<String>>,
}

#[derive(Serialize)]
struct DocumentRef<'a> {
    origins: &'a BTreeMap<String, BTreeSet<String>>,
}

/// Origin to peer task id ownership, backed by one JSON file.
#[derive(Debug)]
pub struct TaskOwnership {
    path: PathBuf,
    origins: BTreeMap<String, BTreeSet<String>>,
}

impl TaskOwnership {
    /// Loads the map from `path`. A missing file is an empty map.
    ///
    /// # Errors
    /// Returns [`TaskMapError::Read`] or [`TaskMapError::Parse`] for an unreadable or invalid file.
    pub fn load(path: &Path) -> Result<Self, TaskMapError> {
        let origins = match fs::read(path) {
            Ok(bytes) => {
                serde_json::from_slice::<Document>(&bytes)
                    .map_err(TaskMapError::Parse)?
                    .origins
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(TaskMapError::Read(error)),
        };
        Ok(Self {
            path: path.to_owned(),
            origins,
        })
    }

    /// Returns whether `origin` created `task_id` through this driver (0009-077).
    pub fn owns(&self, origin: &str, task_id: &str) -> bool {
        self.origins
            .get(origin)
            .is_some_and(|tasks| tasks.contains(task_id))
    }

    /// Records that `origin` created `task_id`, persisting before the record is committed.
    ///
    /// # Errors
    /// Returns [`TaskMapError::Foreign`] when another origin owns the id, or
    /// [`TaskMapError::Write`] when the file cannot be persisted.
    pub fn record(&mut self, origin: &str, task_id: &str) -> Result<(), TaskMapError> {
        if self
            .origins
            .iter()
            .any(|(owner, tasks)| owner != origin && tasks.contains(task_id))
        {
            return Err(TaskMapError::Foreign);
        }
        if self.owns(origin, task_id) {
            return Ok(());
        }
        self.origins
            .entry(origin.to_owned())
            .or_default()
            .insert(task_id.to_owned());
        if let Err(error) = self.persist() {
            if let Some(tasks) = self.origins.get_mut(origin) {
                tasks.remove(task_id);
            }
            return Err(error);
        }
        Ok(())
    }

    fn persist(&self) -> Result<(), TaskMapError> {
        let body = serde_json::to_vec(&DocumentRef {
            origins: &self.origins,
        })
        .map_err(TaskMapError::Parse)?;
        let mut tmp = self.path.clone().into_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        fs::write(&tmp, &body).map_err(TaskMapError::Write)?;
        fs::rename(&tmp, &self.path).map_err(TaskMapError::Write)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "a2a-http-{name}-{}.json",
            uuid::Uuid::new_v4().simple()
        ))
    }

    /// 0009-077: a recorded id is owned by its origin only.
    #[test]
    fn req_0009_077_ownership_is_per_origin() {
        let path = temp_path("owner");
        let mut map = TaskOwnership::load(&path).expect("load");
        map.record("origin-a", "task-a").expect("record");
        assert!(map.owns("origin-a", "task-a"));
        assert!(!map.owns("origin-b", "task-a"));
        assert!(!map.owns("origin-a", "task-unknown"));
        let _ = fs::remove_file(&path);
    }

    /// 0009-077: an id owned by one origin cannot be recorded for another.
    #[test]
    fn req_0009_077_foreign_task_id_is_refused() {
        let path = temp_path("foreign");
        let mut map = TaskOwnership::load(&path).expect("load");
        map.record("origin-a", "task-a").expect("record");
        assert!(matches!(
            map.record("origin-b", "task-a"),
            Err(TaskMapError::Foreign)
        ));
        assert!(!map.owns("origin-b", "task-a"));
        let _ = fs::remove_file(&path);
    }

    /// 0009-077: ownership survives a reload from the persisted file.
    #[test]
    fn req_0009_077_ownership_persists_across_load() {
        let path = temp_path("persist");
        {
            let mut map = TaskOwnership::load(&path).expect("load");
            map.record("origin-a", "task-a").expect("record");
        }
        let reloaded = TaskOwnership::load(&path).expect("reload");
        assert!(reloaded.owns("origin-a", "task-a"));
        assert!(matches!(
            TaskOwnership::load(&path).map(|mut m| m.record("origin-b", "task-a")),
            Ok(Err(TaskMapError::Foreign))
        ));
        let _ = fs::remove_file(&path);
    }

    /// A failed persist commits nothing in memory.
    #[test]
    fn failed_persist_records_nothing() {
        let path = std::env::temp_dir()
            .join(format!(
                "a2a-http-missing-{}",
                uuid::Uuid::new_v4().simple()
            ))
            .join("map.json");
        let mut map = TaskOwnership::load(&path).expect("load missing file");
        assert!(matches!(
            map.record("origin-a", "task-a"),
            Err(TaskMapError::Write(_))
        ));
        assert!(!map.owns("origin-a", "task-a"));
    }

    #[test]
    fn invalid_file_is_refused() {
        let path = temp_path("invalid");
        fs::write(&path, b"not json").expect("write");
        assert!(matches!(
            TaskOwnership::load(&path),
            Err(TaskMapError::Parse(_))
        ));
        let _ = fs::remove_file(&path);
    }
}
