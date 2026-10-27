//! Workspace lease hook: acquires and releases a work lease around each backend turn.
//!
//! Implements RFC-0012 sections 4.8 and 4.9. A turn runs under an `a2a-workspace` lease,
//! scoped per task (`PerTask`) or per context (`PerSession`). The lease is acquired before
//! the task starts, so a refusal fails the task from SUBMITTED with `WORKSPACE_FAILED`
//! (RFC-0005 table, SUBMITTED to FAILED). Artifacts are written before the terminal event,
//! and the lease is released after that event commits. Garbage collection runs from `pump`
//! on the virtual clock and never removes a workspace that a held lease references.

use std::collections::BTreeMap;
use std::path::PathBuf;

use a2a_core::task::{ArtifactChunk, Effect, StopReason, TaskError, TaskStore};
use a2a_workspace::{
    AcquireRequest, AgentId, Lease, ReleaseOutcome, WorkKey, WorkScope, Workspace, WorkspaceConfig,
    WorkspaceMode, WorkspaceRef,
};
use bus_api::bus::Bus;

use crate::backend::Backend;
use crate::config::WorkerConfig;
use crate::error::WorkerError;
use crate::worker::Worker;

/// Artifact id of the workspace summary published before each turn's terminal event.
const SUMMARY_ARTIFACT_ID: &str = "workspace-summary";

/// RFC-0006 0006-006 token for a workspace mode.
fn mode_token(mode: WorkspaceMode) -> &'static str {
    match mode {
        WorkspaceMode::PerTask => "per-task",
        WorkspaceMode::PerSession => "per-session",
    }
}

/// Workspace settings of a worker (RFC-0012 sections 4.8 and 4.9).
#[derive(Debug, Clone)]
pub struct WorkspaceBinding {
    /// Roots, tenant, workspace mode and retention policy of the agent.
    pub config: WorkspaceConfig,
    /// Operator checkout each workspace is based on.
    pub source: PathBuf,
    /// Virtual-clock interval between garbage-collection passes, in milliseconds.
    pub gc_interval_ms: u64,
}

/// Lease a turn runs under: one per task, or one per context in `PerSession` mode.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Slot {
    Task(String),
    Session(String),
}

/// A held lease with the reference that protects its workspace from garbage collection.
struct Held {
    lease: Lease,
    reference: WorkspaceRef,
}

/// Lease bookkeeping of one worker. Present only when a [`WorkspaceBinding`] is configured.
pub(crate) struct WorkspaceHook {
    workspace: Workspace,
    agent: AgentId,
    source: PathBuf,
    gc_interval_ms: u64,
    next_gc_ms: u64,
    draining: bool,
    held: BTreeMap<Slot, Held>,
    users: BTreeMap<String, Slot>,
}

impl WorkspaceHook {
    /// Opens the workspace root of `config`, or returns `None` when no binding is configured.
    pub(crate) fn for_config(config: &WorkerConfig) -> Result<Option<Self>, WorkerError> {
        let Some(binding) = config.workspace.clone() else {
            return Ok(None);
        };
        let agent = AgentId::parse(config.agent.as_str())
            .map_err(|_| WorkerError::Config("agent identity is not a workspace id"))?;
        let workspace = Workspace::open(binding.config)
            .map_err(|_| WorkerError::Config("workspace configuration refused"))?;
        Ok(Some(Self {
            workspace,
            agent,
            source: binding.source,
            gc_interval_ms: binding.gc_interval_ms,
            next_gc_ms: 0,
            draining: false,
            held: BTreeMap::new(),
            users: BTreeMap::new(),
        }))
    }

    /// Acquires the lease `task` runs under. In `PerSession` mode a lease already held for the
    /// context is reused (RFC-0012 0012-067). A refusal returns the RFC token of the failure.
    fn acquire(&mut self, task: &str, context: &str) -> Result<(), &'static str> {
        let (slot, key) = match &self.workspace.config().mode {
            WorkspaceMode::PerTask => (Slot::Task(task.to_owned()), task),
            WorkspaceMode::PerSession => (Slot::Session(context.to_owned()), context),
        };
        let key = WorkKey::parse(key).map_err(|err| err.kind().token())?;
        if !self.held.contains_key(&slot) {
            let scope = match &slot {
                Slot::Task(_) => WorkScope::Task(key.clone()),
                Slot::Session(_) => WorkScope::Session(key.clone()),
            };
            let request = AcquireRequest {
                agent: self.agent.clone(),
                scope,
                source: self.source.clone(),
            };
            let lease = self
                .workspace
                .acquire(request)
                .map_err(|err| err.kind().token())?;
            let reference = WorkspaceRef {
                agent: self.agent.clone(),
                key,
            };
            self.held.insert(slot.clone(), Held { lease, reference });
        }
        self.users.insert(task.to_owned(), slot);
        Ok(())
    }

    /// Lease the task runs under, if one is held.
    fn lease_of(&self, task: &str) -> Option<&Lease> {
        self.users
            .get(task)
            .and_then(|slot| self.held.get(slot))
            .map(|held| &held.lease)
    }

    /// Ends the task's use of its lease. A `PerTask` lease is released here; a `PerSession`
    /// lease stays with its context until the worker drains (RFC-0012 0012-035).
    fn end_task(&mut self, task: &str) -> Option<String> {
        let slot = self.users.remove(task)?;
        if matches!(slot, Slot::Session(_)) {
            return None;
        }
        let held = self.held.remove(&slot)?;
        Some(self.finish(held))
    }

    /// Releases a lease and returns its audit note.
    fn finish(&self, held: Held) -> String {
        match self.workspace.release(held.lease) {
            Ok(ReleaseOutcome::Removed) => "WORKSPACE_REMOVED".to_owned(),
            Ok(ReleaseOutcome::Retained { reasons }) => {
                format!("WORKSPACE_RETAINED: {}", reasons.join(", "))
            }
            Err(err) => format!("WORKSPACE_RELEASE_FAILED: {}", err.kind().token()),
        }
    }

    /// Releases the session leases of a drained, idle worker and runs garbage collection when
    /// due (RFC-0012 0012-036). Returns audit notes; nothing here stops the worker.
    fn maintain(&mut self, now: u64, idle: bool) -> Vec<String> {
        let mut notes = Vec::new();
        if self.draining && idle {
            let sessions: Vec<Slot> = self
                .held
                .keys()
                .filter(|slot| matches!(slot, Slot::Session(_)))
                .cloned()
                .collect();
            for slot in sessions {
                if let Some(held) = self.held.remove(&slot) {
                    notes.push(self.finish(held));
                }
            }
        }
        if now >= self.next_gc_ms {
            self.next_gc_ms = now.saturating_add(self.gc_interval_ms);
            let referenced: Vec<WorkspaceRef> = self
                .held
                .values()
                .map(|held| held.reference.clone())
                .collect();
            match self.workspace.gc(now, &referenced) {
                Ok(report) if report.removed > 0 || report.orphans_disposed > 0 => {
                    notes.push(format!(
                        "WORKSPACE_GC: removed={} retained={} retained_bytes={} orphans_disposed={}",
                        report.removed,
                        report.retained,
                        report.retained_bytes,
                        report.orphans_disposed
                    ));
                }
                Ok(_) => {}
                Err(err) => notes.push(format!("WORKSPACE_GC_FAILED: {}", err.kind().token())),
            }
        }
        notes
    }

    /// Path and workspace-mode token of the lease `task` holds, if any.
    fn lease_context(&self, task: &str) -> Option<(String, String)> {
        let held = self.held.get(self.users.get(task)?)?;
        let mode = mode_token(self.workspace.config().mode);
        Some((
            held.lease.path().to_string_lossy().into_owned(),
            mode.to_owned(),
        ))
    }
}

impl<B: Bus, D: Backend, S: TaskStore> Worker<B, D, S> {
    /// Acquires the lease for a turn about to start. A refusal returns the RFC token.
    pub(crate) fn lease_for_turn(&mut self, task: &str, context: &str) -> Result<(), &'static str> {
        match self.workspace.as_mut() {
            Some(hook) => hook.acquire(task, context),
            None => Ok(()),
        }
    }

    /// Working directory and workspace mode `task` runs under, as RFC-0006 [0006-006] reports them
    /// on an approval request. The lease path applies when the task holds one; otherwise the
    /// configured directory and mode apply (never the process working directory).
    pub(crate) fn working_context(&self, task: &str) -> (String, String) {
        match self
            .workspace
            .as_ref()
            .and_then(|hook| hook.lease_context(task))
        {
            Some(context) => context,
            None => (
                self.config.working_directory.to_string_lossy().into_owned(),
                mode_token(self.config.working_mode).to_owned(),
            ),
        }
    }

    /// Commits WORKSPACE_FAILED for `task` (SUBMITTED or WORKING to FAILED, RFC-0005). A task
    /// that already ended keeps its state.
    pub(crate) fn fail_workspace(
        &mut self,
        task: &str,
        token: &'static str,
        now: u64,
    ) -> Result<Vec<Effect>, WorkerError> {
        self.audit(Some(task), format!("WORKSPACE_FAILED: {token}"));
        match self.tasks.workspace_failed(task, token, now) {
            Ok(effects) => Ok(effects),
            Err(TaskError::Terminal | TaskError::NotActive) => Ok(Vec::new()),
            Err(err) => Err(err.into()),
        }
    }

    /// Writes the lease's artifacts and publishes their summary before the terminal event.
    /// Returns the failure token when writing fails.
    fn publish_artifacts(
        &mut self,
        task: &str,
        now: u64,
    ) -> Result<Option<&'static str>, WorkerError> {
        let Some(hook) = self.workspace.as_ref() else {
            return Ok(None);
        };
        let Some(lease) = hook.lease_of(task) else {
            return Ok(None);
        };
        let summary = match hook.workspace.write_artifacts(lease, task) {
            Ok(summary) => summary,
            Err(err) => return Ok(Some(err.kind().token())),
        };
        let payload = summary.render_text().into_bytes();
        match self.tasks.append_artifact(
            task,
            ArtifactChunk {
                artifact_id: SUMMARY_ARTIFACT_ID.to_owned(),
                name: SUMMARY_ARTIFACT_ID.to_owned(),
                media_type: "text/plain".to_owned(),
                append: true,
                last_chunk: true,
                payload,
            },
            now,
        ) {
            Ok(()) => Ok(None),
            Err(TaskError::Terminal | TaskError::NotActive) => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    /// Ends a turn: writes artifacts, commits the terminal state, then releases the lease. A
    /// failed artifact write commits WORKSPACE_FAILED instead of the backend's outcome.
    pub(crate) fn commit_turn_end(
        &mut self,
        task: &str,
        reason: StopReason,
        now: u64,
    ) -> Result<Vec<Effect>, WorkerError> {
        let result = match self.publish_artifacts(task, now) {
            Ok(None) => self
                .tasks
                .backend_stopped(task, reason, now)
                .map_err(WorkerError::from),
            Ok(Some(token)) => self.fail_workspace(task, token, now),
            Err(err) => Err(err),
        };
        self.release_task(task);
        result
    }

    /// Writes a killed task's artifacts before the kill commits its end. A failure is audited
    /// only: the kill already decides the end state (RFC-0005 kill-then-commit).
    pub(crate) fn publish_before_kill(&mut self, task: &str, now: u64) -> Result<(), WorkerError> {
        if let Some(token) = self.publish_artifacts(task, now)? {
            self.audit(Some(task), format!("WORKSPACE_ARTIFACTS_FAILED: {token}"));
        }
        Ok(())
    }

    /// Releases the lease `task` ran under, if any, and audits the outcome.
    pub(crate) fn release_task(&mut self, task: &str) {
        let note = match self.workspace.as_mut() {
            Some(hook) => hook.end_task(task),
            None => None,
        };
        if let Some(note) = note {
            self.audit(Some(task), note);
        }
    }

    /// Records the worker's drain state; a drained idle worker releases its session leases.
    pub(crate) fn set_workspace_draining(&mut self, draining: bool) {
        if let Some(hook) = self.workspace.as_mut() {
            hook.draining = draining;
        }
    }

    /// Runs workspace upkeep from `pump`: releases drained session leases and runs gc when due.
    pub(crate) fn workspace_maintenance(&mut self, now: u64) {
        let idle = self.tasks.active_count() == 0;
        let notes = match self.workspace.as_mut() {
            Some(hook) => hook.maintain(now, idle),
            None => Vec::new(),
        };
        for note in notes {
            self.audit(None, note);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use super::*;

    const TASK_1: &str = "0190a0a0-0000-7000-8000-000000000001";
    const TASK_2: &str = "0190a0a0-0000-7000-8000-000000000002";
    const CONTEXT: &str = "0190a0a0-0000-7000-8000-0000000000c1";

    fn run_git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(dir)
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
            ])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .status()
            .expect("spawn git");
        assert!(status.success(), "git {args:?} failed");
    }

    /// Creates `dir` as a git repository on branch `main` with one commit.
    fn make_repo(dir: &Path) {
        fs::create_dir_all(dir).expect("repo dir");
        run_git(dir, &["init", "-q", "-b", "main"]);
        fs::write(dir.join("README.md"), "init\n").expect("readme");
        run_git(dir, &["add", "-A"]);
        run_git(dir, &["commit", "-q", "-m", "init"]);
    }

    /// Hook over a fresh workspace root under the temp directory. Returns the base directory
    /// so the caller can remove it.
    fn hook(mode: WorkspaceMode, tag: &str) -> (WorkspaceHook, PathBuf) {
        let base = std::env::temp_dir().join(format!("a2a-ws-hook-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let source = base.join("source");
        make_repo(&source);
        let config =
            WorkspaceConfig::new(base.join("root"), base.join("artifacts"), "tenant-a", mode);
        let workspace = Workspace::open(config).expect("open workspace");
        let hook = WorkspaceHook {
            workspace,
            agent: AgentId::parse("agent-a").expect("agent id"),
            source,
            gc_interval_ms: 1_000,
            next_gc_ms: 0,
            draining: false,
            held: BTreeMap::new(),
            users: BTreeMap::new(),
        };
        (hook, base)
    }

    /// RFC-0012 0012-067: a session's lease is acquired once and reused by its later tasks.
    #[test]
    fn req_0012_067_session_lease_is_reused_within_a_context() {
        let (mut hook, base) = hook(WorkspaceMode::PerSession, "reuse");
        hook.acquire(TASK_1, CONTEXT).expect("first task");
        let first = hook.lease_of(TASK_1).expect("lease").path().to_path_buf();
        assert!(
            hook.end_task(TASK_1).is_none(),
            "session lease outlives its task"
        );
        hook.acquire(TASK_2, CONTEXT).expect("second task");
        let second = hook.lease_of(TASK_2).expect("lease").path().to_path_buf();
        assert_eq!(first, second);
        assert_eq!(hook.held.len(), 1);
        drop(hook);
        let _ = fs::remove_dir_all(base);
    }

    /// RFC-0012 0012-035: a per-task lease is released when its task reaches a terminal state.
    #[test]
    fn req_0012_035_task_lease_is_released_at_task_end() {
        let (mut hook, base) = hook(WorkspaceMode::PerTask, "task-end");
        hook.acquire(TASK_1, CONTEXT).expect("acquire");
        assert!(hook.lease_of(TASK_1).is_some());
        let note = hook.end_task(TASK_1).expect("released");
        assert_eq!(note, "WORKSPACE_REMOVED");
        assert!(hook.held.is_empty());
        drop(hook);
        let _ = fs::remove_dir_all(base);
    }

    /// RFC-0012 0012-035: a session lease is released when the worker drains while idle.
    #[test]
    fn req_0012_035_session_lease_is_released_when_drained_and_idle() {
        let (mut hook, base) = hook(WorkspaceMode::PerSession, "drain");
        hook.acquire(TASK_1, CONTEXT).expect("acquire");
        hook.end_task(TASK_1);
        assert_eq!(hook.held.len(), 1);
        hook.draining = true;
        let notes = hook.maintain(0, true);
        assert!(hook.held.is_empty());
        assert!(notes.iter().any(|note| note == "WORKSPACE_REMOVED"));
        drop(hook);
        let _ = fs::remove_dir_all(base);
    }

    /// RFC-0005 0005-053: an acquire the workspace cannot honour is refused with a token.
    #[test]
    fn req_0005_053_unparseable_task_id_refuses_acquire() {
        let (mut hook, base) = hook(WorkspaceMode::PerTask, "refuse");
        let refused = hook.acquire("not-a-uuid", CONTEXT);
        assert!(refused.is_err());
        assert!(hook.held.is_empty());
        drop(hook);
        let _ = fs::remove_dir_all(base);
    }

    /// RFC-0012 0012-036: garbage collection never removes a workspace a held lease references.
    #[test]
    fn req_0012_036_gc_keeps_held_workspace() {
        let (mut hook, base) = hook(WorkspaceMode::PerTask, "gc");
        hook.acquire(TASK_1, CONTEXT).expect("acquire");
        let path = hook.lease_of(TASK_1).expect("lease").path().to_path_buf();
        let notes = hook.maintain(0, false);
        assert!(notes.is_empty(), "nothing is collectable: {notes:?}");
        assert!(path.exists());
        hook.end_task(TASK_1);
        drop(hook);
        let _ = fs::remove_dir_all(base);
    }

    /// RFC-0006 0006-006: an approval raised under a held lease reports the lease path and
    /// its workspace mode; a task without a lease has no lease context, so the configured
    /// directory and mode apply.
    #[test]
    fn req_0006_006_lease_context_supplies_path_and_mode() {
        let (mut hook, base) = hook(WorkspaceMode::PerSession, "lease-ctx");
        assert!(
            hook.lease_context(TASK_1).is_none(),
            "no lease, no lease context"
        );
        hook.acquire(TASK_1, CONTEXT).expect("acquire");
        let path = hook.lease_of(TASK_1).expect("lease").path().to_path_buf();
        let (dir, mode) = hook.lease_context(TASK_1).expect("lease context");
        assert_eq!(dir, path.to_string_lossy());
        assert_eq!(mode, mode_token(WorkspaceMode::PerSession));
        assert!(
            hook.lease_context(TASK_2).is_none(),
            "other tasks hold nothing"
        );
        drop(hook);
        let _ = fs::remove_dir_all(base);
    }
}
