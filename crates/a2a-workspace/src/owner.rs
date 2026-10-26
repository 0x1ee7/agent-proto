//! Owner records stored in lock files and the workspace-registry mutex. A record is live only if
//! the owning process exists and its start time matches, which guards against PID reuse.

use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

use crate::error::{ErrorKind, Result, WorkspaceError, io_err};
use crate::lease::LeaseRecord;
use crate::platform;

const LOCK_WAIT: Duration = Duration::from_secs(30);
const RETRY: Duration = Duration::from_millis(15);

/// Content of a lock file or the registry mutex.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct OwnerRecord {
    pub pid: u32,
    pub start_time: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<LeaseRecord>,
}

impl OwnerRecord {
    /// Record for the current process.
    pub(crate) fn this_process(lease: Option<LeaseRecord>) -> Result<Self> {
        let pid = std::process::id();
        let start_time = process_start(pid).ok_or_else(|| {
            WorkspaceError::new(
                ErrorKind::Io,
                "cannot read start time of the current process",
            )
        })?;
        Ok(Self {
            pid,
            start_time,
            lease,
        })
    }

    /// True when the recorded process still exists with the recorded start time.
    pub(crate) fn is_alive(&self) -> bool {
        process_start(self.pid) == Some(self.start_time)
    }

    /// True when the record names the current process.
    pub(crate) fn is_self(&self) -> bool {
        self.pid == std::process::id() && self.is_alive()
    }
}

/// Start time (seconds since the Unix epoch) of a running process, or `None` if it is not running.
pub(crate) fn process_start(pid: u32) -> Option<u64> {
    let pid = Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing(),
    );
    system.process(pid).map(|process| process.start_time())
}

/// Reads an owner record. Missing file gives `None`; unparseable content is `state_invalid`.
pub(crate) fn read(path: &Path) -> Result<Option<OwnerRecord>> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|e| {
            WorkspaceError::new(
                ErrorKind::StateInvalid,
                format!("{} is not a valid owner record: {e}", path.display()),
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_err("read owner record", e)),
    }
}

pub(crate) fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_err("remove lock file", e)),
    }
}

/// Exclusive registry mutex. Every lease-table mutation (acquire, release, gc, index writes)
/// runs while holding it. A stale holder is reclaimed under a second guard file so two
/// processes cannot both reclaim the same dead holder.
#[derive(Debug)]
pub(crate) struct RootLock {
    path: PathBuf,
    pid: u32,
    start_time: u64,
}

impl RootLock {
    /// Waits up to 30 seconds for the mutex at `path`.
    pub(crate) fn acquire(path: &Path) -> Result<Self> {
        let me = OwnerRecord::this_process(None)?;
        let bytes = serde_json::to_vec(&me)
            .map_err(|e| WorkspaceError::new(ErrorKind::StateInvalid, e.to_string()))?;
        let deadline = Instant::now() + LOCK_WAIT;
        loop {
            let created = platform::publish_exclusive(path, &bytes)
                .map_err(|e| io_err("acquire workspace registry mutex", e))?;
            if created {
                return Ok(Self {
                    path: path.to_path_buf(),
                    pid: me.pid,
                    start_time: me.start_time,
                });
            }
            if let Some(current) = read(path)? {
                if !current.is_alive() {
                    reclaim(path, &bytes)?;
                    continue;
                }
            }
            if Instant::now() >= deadline {
                return Err(WorkspaceError::new(
                    ErrorKind::Busy,
                    format!("timed out waiting for {}", path.display()),
                ));
            }
            thread::sleep(RETRY);
        }
    }
}

impl Drop for RootLock {
    fn drop(&mut self) {
        // Best effort: a failed removal leaves a record that the next acquirer reclaims once
        // this process has exited.
        if let Ok(Some(current)) = read(&self.path) {
            if current.pid == self.pid && current.start_time == self.start_time {
                let _ = fs::remove_file(&self.path);
            }
        }
    }
}

fn reclaim(path: &Path, my_bytes: &[u8]) -> Result<()> {
    let guard = PathBuf::from(format!("{}.reclaim", path.display()));
    let created = platform::publish_exclusive(&guard, my_bytes)
        .map_err(|e| io_err("acquire reclaim guard", e))?;
    if created {
        let outcome = reclaim_under_guard(path);
        return outcome.and(remove_if_exists(&guard));
    }
    if let Some(holder) = read(&guard)? {
        if !holder.is_alive() {
            remove_if_exists(&guard)?;
        }
    }
    thread::sleep(RETRY);
    Ok(())
}

fn reclaim_under_guard(path: &Path) -> Result<()> {
    if let Some(current) = read(path)? {
        if !current.is_alive() {
            remove_if_exists(path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    // [0012-021] a record is live only while the same process with the same start time exists.
    #[test]
    fn req_0012_021_dead_pid_and_reused_pid_are_not_live() {
        let mut child = std::process::Command::new("git")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let dead_pid = child.id();
        child.wait().unwrap();
        let dead = OwnerRecord {
            pid: dead_pid,
            start_time: 1,
            lease: None,
        };
        assert!(!dead.is_alive());

        let me = OwnerRecord::this_process(None).unwrap();
        assert!(me.is_alive());
        assert!(me.is_self());
        let reused = OwnerRecord {
            pid: me.pid,
            start_time: me.start_time + 1_000_000,
            lease: None,
        };
        assert!(!reused.is_alive());
        assert!(!reused.is_self());
    }
}
