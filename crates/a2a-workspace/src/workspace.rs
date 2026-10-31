//! Workspace lifecycle: acquire, release, retention and artifacts (RFC-0012 sections 4.8, 4.9).
//!
//! Every mutation of the lease table (acquire, release, gc, index writes) runs while holding the
//! registry mutex `<root>/.mutex`. Lease locks are published only under that mutex.

use std::fs;
use std::path::{Path, PathBuf};

use crate::artifacts::{self, ArtifactSummary};
use crate::config::{NonGitPolicy, WorkspaceConfig, WorkspaceMode, validate_base_ref};
use crate::copy;
use crate::error::{ErrorKind, Result, WorkspaceError, config_err, io_err};
use crate::exclude::Excludes;
use crate::git;
use crate::ids::{WorkScope, WorkspaceRef, validate_slug};
use crate::index::{self, RetainedEntry};
use crate::lease::{AcquireRequest, Lease, LeaseKind, LeaseRecord, ReleaseOutcome};
use crate::owner::{self, OwnerRecord, RootLock};
use crate::platform;

const MUTEX_FILE: &str = ".mutex";
const LOCKS_DIR: &str = "locks";
const BRANCH_PREFIX: &str = "a2abus";
const MIN_PATCH_CAP: u64 = 4096;
const REASON_REMOVAL_FAILED: &str = "removal_failed";

/// Workspace registry rooted at `WorkspaceConfig::root`.
#[derive(Debug)]
pub struct Workspace {
    config: WorkspaceConfig,
    root: PathBuf,
    excludes: Excludes,
}

/// Result of a garbage-collection pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcReport {
    /// Retained workspaces removed in this pass.
    pub removed: usize,
    /// Retained workspaces still listed in `index.json` after the pass.
    pub retained: usize,
    /// Bytes held by the retained workspaces after the pass.
    pub retained_bytes: u64,
    /// Leases of dead owner processes disposed during the pass.
    pub orphans_disposed: usize,
}

enum Disposition {
    Removed,
    Retained(Vec<String>),
}

struct LiveLease {
    lock: PathBuf,
    owner: OwnerRecord,
    lease: LeaseRecord,
}

struct Sweep {
    live: Vec<LiveLease>,
    disposed: usize,
}

impl Workspace {
    /// Validates `config`, creates the roots and returns a handle. Validation runs before any
    /// directory is created.
    pub fn open(config: WorkspaceConfig) -> Result<Self> {
        validate_slug(&config.tenant, "tenant")?;
        validate_base_ref(&config.base_ref)?;
        if !config.root.is_absolute() || !config.artifacts_root.is_absolute() {
            return Err(config_err("root and artifacts_root must be absolute paths"));
        }
        if config.patch_cap_bytes < MIN_PATCH_CAP {
            return Err(config_err(format!(
                "patch_cap_bytes must be at least {MIN_PATCH_CAP}"
            )));
        }
        let excludes = Excludes::new(&config.copy_exclude)?;
        platform::check_budget(
            &config.root,
            platform::workspace_path_budget(),
            "workspace root",
        )?;
        fs::create_dir_all(config.root.join(LOCKS_DIR))
            .map_err(|e| io_err("create workspace root", e))?;
        fs::create_dir_all(&config.artifacts_root)
            .map_err(|e| io_err("create artifacts root", e))?;
        let root = platform::normalize(&config.root)?;
        Ok(Self {
            config,
            root,
            excludes,
        })
    }

    /// Configuration this registry was opened with.
    pub fn config(&self) -> &WorkspaceConfig {
        &self.config
    }

    /// Leases a workspace for `request`. Every refusal is decided before a lock or workspace is
    /// created. The call waits for the registry mutex (up to 30 s), so run it on a blocking
    /// thread.
    pub fn acquire(&self, request: AcquireRequest) -> Result<Lease> {
        self.check_mode(&request.scope)?;
        let source = validate_source(&request.source)?;
        let (source_root, kind) = match git::toplevel(&source)? {
            Some(top) => (platform::normalize(&top)?, LeaseKind::Worktree),
            None => match self.config.non_git {
                NonGitPolicy::Refuse => {
                    return Err(WorkspaceError::new(
                        ErrorKind::NonGitRefused,
                        format!(
                            "{} is not a git checkout and the non-git policy is refuse",
                            source.display()
                        ),
                    ));
                }
                NonGitPolicy::Copy => (source, LeaseKind::Copy),
            },
        };
        let key = request.scope.key().clone();
        let agent = request.agent.as_str().to_string();
        let short = key.short().to_string();
        let path = self
            .root
            .join(&self.config.tenant)
            .join(&agent)
            .join(&short);
        platform::check_budget(&path, platform::workspace_path_budget(), "workspace path")?;
        let branch = format!("{BRANCH_PREFIX}/{agent}/{short}");
        let lock = self.lock_path(&self.config.tenant, &agent, &short);

        let _mutex = RootLock::acquire(&self.root.join(MUTEX_FILE))?;
        let sweep = self.sweep_locks()?;
        if let Some(live) = sweep.live.iter().find(|live| live.lock == lock) {
            return reuse(live, &source_root, kind, key.canonical());
        }
        if platform::overlaps(&path, &source_root) {
            return Err(WorkspaceError::new(
                ErrorKind::SharedCheckout,
                format!(
                    "workspace {} would sit inside operator checkout {}",
                    path.display(),
                    source_root.display()
                ),
            ));
        }
        for live in &sweep.live {
            let other = &live.lease;
            if platform::overlaps(&path, &other.path)
                || platform::overlaps(&path, &other.source)
                || platform::overlaps(&path, &source_root)
                || platform::overlaps(&source_root, &other.path)
            {
                return Err(WorkspaceError::new(
                    ErrorKind::SharedCheckout,
                    format!(
                        "workspace overlaps a live lease of agent {} held by pid {}",
                        other.agent, live.owner.pid
                    ),
                ));
            }
        }
        if path.exists() {
            return Err(WorkspaceError::new(
                ErrorKind::PathOccupied,
                format!("{} already exists and is not leased", path.display()),
            ));
        }

        let mut record = LeaseRecord {
            tenant: self.config.tenant.clone(),
            agent,
            scope: request.scope.kind(),
            key: key.canonical().to_string(),
            short_id: short,
            kind,
            path: path.clone(),
            source: source_root.clone(),
            branch: branch.clone(),
            base_commit: String::new(),
            source_dirty: false,
            excluded_count: 0,
        };
        match kind {
            LeaseKind::Worktree => {
                let (base, dirty) = self.create_worktree(&source_root, &branch, &path)?;
                record.base_commit = base;
                record.source_dirty = dirty;
            }
            LeaseKind::Copy => {
                let (base, excluded) = self.create_copy(&source_root, &branch, &path)?;
                record.base_commit = base;
                record.excluded_count = excluded;
            }
        }
        if let Err(err) = write_lock(&lock, &record) {
            let _ = self.remove_workspace(&record);
            return Err(err);
        }
        Ok(Lease::new(record))
    }

    /// Releases a lease. A clean workspace (no uncommitted, untracked or unmerged commits) is
    /// removed together with its branch. Otherwise it is kept and listed in `index.json` with
    /// the reasons. Writes made after release are not tracked.
    pub fn release(&self, lease: Lease) -> Result<ReleaseOutcome> {
        let record = lease.into_record();
        let lock = self.lock_path(&record.tenant, &record.agent, &record.short_id);
        let _mutex = RootLock::acquire(&self.root.join(MUTEX_FILE))?;
        let held = owner::read(&lock)?.is_some_and(|owner| {
            owner.is_self() && owner.lease.as_ref().is_some_and(|l| l.path == record.path)
        });
        if !held {
            return Err(WorkspaceError::new(
                ErrorKind::LeaseLost,
                format!(
                    "lease for agent {} is not held by this process",
                    record.agent
                ),
            ));
        }
        match self.finish(&record, &lock, &[])? {
            Disposition::Removed => Ok(ReleaseOutcome::Removed),
            Disposition::Retained(reasons) => Ok(ReleaseOutcome::Retained { reasons }),
        }
    }

    /// Garbage-collects workspaces. Under the registry mutex it disposes leases of dead owners,
    /// retries failed removals and missing paths, removes retained workspaces older than
    /// `retention.max_age` (oldest first), then removes the oldest until the retained total is
    /// within `retention.max_retained_bytes`. Workspaces listed in `referenced` are never removed
    /// by age or size. `now_ms` is milliseconds since the Unix epoch.
    pub fn gc(&self, now_ms: u64, referenced: &[WorkspaceRef]) -> Result<GcReport> {
        let _mutex = RootLock::acquire(&self.root.join(MUTEX_FILE))?;
        let sweep = self.sweep_locks()?;
        let mut entries = index::load(&self.root)?;
        entries.sort_by_key(|entry| entry.retained_at_ms);
        for entry in &mut entries {
            entry.bytes = if entry.lease.path.exists() {
                platform::dir_size(&entry.lease.path).unwrap_or(0)
            } else {
                0
            };
        }
        let protected: Vec<bool> = entries
            .iter()
            .map(|entry| {
                referenced.iter().any(|r| {
                    r.agent.as_str() == entry.lease.agent && r.key.canonical() == entry.lease.key
                })
            })
            .collect();
        let max_age = self.config.retention.max_age.as_millis();
        let max_bytes = self.config.retention.max_retained_bytes;
        let mut keep = vec![true; entries.len()];
        let mut removed = 0usize;

        for (i, entry) in entries.iter_mut().enumerate() {
            if protected[i] {
                continue;
            }
            let age = u128::from(now_ms.saturating_sub(entry.retained_at_ms));
            let expired = age > max_age;
            let retry = !entry.lease.path.exists()
                || entry.reasons.iter().any(|r| r == REASON_REMOVAL_FAILED);
            if (expired || retry) && self.remove_retained(entry) {
                keep[i] = false;
                removed += 1;
            }
        }

        let mut total: u64 = entries
            .iter()
            .zip(&keep)
            .filter(|(_, kept)| **kept)
            .map(|(entry, _)| entry.bytes)
            .sum();
        for (i, entry) in entries.iter_mut().enumerate() {
            if total <= max_bytes {
                break;
            }
            if !keep[i] || protected[i] {
                continue;
            }
            let bytes = entry.bytes;
            if self.remove_retained(entry) {
                keep[i] = false;
                removed += 1;
                total = total.saturating_sub(bytes);
            }
        }

        let remaining: Vec<RetainedEntry> = entries
            .into_iter()
            .zip(keep)
            .filter_map(|(entry, kept)| kept.then_some(entry))
            .collect();
        index::save(&self.root, &remaining)?;
        Ok(GcReport {
            removed,
            retained: remaining.len(),
            retained_bytes: total,
            orphans_disposed: sweep.disposed,
        })
    }

    /// Writes `<artifacts_root>/<task_id>/diff.patch` for `lease` and returns its summary. Call
    /// before [`Workspace::release`]. Patches are capped at `config.patch_cap_bytes`.
    pub fn write_artifacts(&self, lease: &Lease, task_id: &str) -> Result<ArtifactSummary> {
        artifacts::write(
            lease.record(),
            &self.config.artifacts_root,
            task_id,
            self.config.patch_cap_bytes,
        )
    }

    fn check_mode(&self, scope: &WorkScope) -> Result<()> {
        let matches_mode = matches!(
            (self.config.mode, scope),
            (WorkspaceMode::PerSession, WorkScope::Session(_))
                | (WorkspaceMode::PerTask, WorkScope::Task(_))
        );
        if matches_mode {
            Ok(())
        } else {
            Err(config_err(format!(
                "scope {:?} does not match workspace mode {:?}",
                scope.kind(),
                self.config.mode
            )))
        }
    }

    fn lock_path(&self, tenant: &str, agent: &str, short: &str) -> PathBuf {
        self.root
            .join(LOCKS_DIR)
            .join(format!("{tenant}.{agent}.{short}.lock"))
    }

    fn create_worktree(
        &self,
        checkout: &Path,
        branch: &str,
        path: &Path,
    ) -> Result<(String, bool)> {
        let base = git::resolve_commit(checkout, &self.config.base_ref)?;
        let longest = git::longest_tracked_name(checkout, &base)?;
        let needed = platform::path_len(path) + 1 + longest;
        let budget = platform::entry_path_budget();
        if needed > budget {
            return Err(WorkspaceError::new(
                ErrorKind::PathTooLong,
                format!(
                    "a tracked path would need {needed} characters inside the workspace, over the {budget}-character budget"
                ),
            ));
        }
        if git::branch_exists(checkout, branch)? {
            if git::commits_between(checkout, &base, branch)? > 0 {
                return Err(WorkspaceError::new(
                    ErrorKind::BranchExists,
                    format!("branch {branch} has commits beyond the base and is not replaced"),
                ));
            }
            git::branch_delete(checkout, branch)?;
        }
        let (untracked, modified) = git::status(checkout)?;
        ensure_parent(path)?;
        if let Err(err) = git::worktree_add(checkout, branch, path, &base) {
            self.discard_partial(checkout, branch, path);
            return Err(err);
        }
        Ok((base, untracked || modified))
    }

    fn create_copy(&self, source: &Path, branch: &str, path: &Path) -> Result<(String, usize)> {
        let plan = copy::plan(source, &self.excludes)?;
        let result = copy::create(source, path, branch, plan, self.config.copy_limits);
        if result.is_err() {
            let _ = platform::remove_tree_within(&self.root, path);
        }
        result.map(|created| (created.base_commit, created.excluded))
    }

    fn discard_partial(&self, checkout: &Path, branch: &str, path: &Path) {
        let _ = platform::remove_tree_within(&self.root, path);
        let _ = git::worktree_prune(checkout);
        if git::branch_exists(checkout, branch).unwrap_or(false) {
            let _ = git::branch_delete(checkout, branch);
        }
    }

    /// Reads and reclaims lock files. Locks of dead owners are disposed (removed when clean,
    /// retained when dirty). Live leases are returned.
    fn sweep_locks(&self) -> Result<Sweep> {
        let mut sweep = Sweep {
            live: Vec::new(),
            disposed: 0,
        };
        let entries = fs::read_dir(self.root.join(LOCKS_DIR))
            .map_err(|e| io_err("read lock directory", e))?;
        for entry in entries {
            let lock = entry.map_err(|e| io_err("read lock directory", e))?.path();
            if lock.extension().and_then(|e| e.to_str()) != Some("lock") {
                continue;
            }
            let Some(owner) = owner::read(&lock)? else {
                continue;
            };
            match (owner.is_alive(), owner.lease.clone()) {
                (true, Some(lease)) => sweep.live.push(LiveLease { lock, owner, lease }),
                (true, None) => {
                    return Err(WorkspaceError::new(
                        ErrorKind::StateInvalid,
                        format!("{} holds no lease record", lock.display()),
                    ));
                }
                (false, Some(lease)) => {
                    self.finish(&lease, &lock, &["owner_exited"])?;
                    sweep.disposed += 1;
                }
                (false, None) => {
                    owner::remove_if_exists(&lock)?;
                    sweep.disposed += 1;
                }
            }
        }
        Ok(sweep)
    }

    /// Removes a clean workspace, or retains it with reasons. Removal failure is retained as
    /// `removal_failed` so gc retries it. The lock is removed in every case.
    fn finish(&self, record: &LeaseRecord, lock: &Path, extra: &[&str]) -> Result<Disposition> {
        let mut reasons = self.dirty_reasons(record);
        if reasons.is_empty() {
            match self.remove_workspace(record) {
                Ok(()) => {
                    owner::remove_if_exists(lock)?;
                    return Ok(Disposition::Removed);
                }
                Err(_) => reasons.push(REASON_REMOVAL_FAILED.to_string()),
            }
        }
        reasons.extend(extra.iter().map(|reason| (*reason).to_string()));
        self.retain(record, reasons.clone())?;
        owner::remove_if_exists(lock)?;
        Ok(Disposition::Retained(reasons))
    }

    fn retain(&self, record: &LeaseRecord, reasons: Vec<String>) -> Result<()> {
        let mut entries = index::load(&self.root)?;
        entries.retain(|entry| entry.lease.path != record.path);
        let bytes = platform::dir_size(&record.path).unwrap_or(0);
        entries.push(RetainedEntry {
            lease: record.clone(),
            reasons,
            retained_at_ms: platform::now_ms(),
            bytes,
        });
        index::save(&self.root, &entries)
    }

    /// Removes a retained entry for gc. On failure the entry gains a `removal_failed` reason.
    fn remove_retained(&self, entry: &mut RetainedEntry) -> bool {
        if self.remove_workspace(&entry.lease).is_ok() {
            return true;
        }
        if !entry.reasons.iter().any(|r| r == REASON_REMOVAL_FAILED) {
            entry.reasons.push(REASON_REMOVAL_FAILED.to_string());
        }
        false
    }

    /// Removes the workspace directory and, for worktrees, its registration and branch.
    /// Directories containing links are removed link by link without following them. Worktrees
    /// without links use `git worktree remove`.
    fn remove_workspace(&self, record: &LeaseRecord) -> Result<()> {
        let exists = record.path.exists();
        match record.kind {
            LeaseKind::Worktree => {
                let links = exists
                    && platform::tree_has_links(&record.path)
                        .map_err(|e| io_err("inspect workspace", e))?;
                if exists && !links {
                    if git::worktree_remove(&record.source, &record.path).is_err() {
                        platform::remove_tree_within(&self.root, &record.path)?;
                    }
                } else if exists {
                    platform::remove_tree_within(&self.root, &record.path)?;
                }
                let _ = git::worktree_prune(&record.source);
                if git::branch_exists(&record.source, &record.branch)? {
                    git::branch_delete(&record.source, &record.branch)?;
                }
                Ok(())
            }
            LeaseKind::Copy => {
                if exists {
                    platform::remove_tree_within(&self.root, &record.path)?;
                }
                Ok(())
            }
        }
    }

    /// Reasons a workspace must be kept. An unreadable state yields `state_unknown` and keeps it.
    fn dirty_reasons(&self, record: &LeaseRecord) -> Vec<String> {
        if !record.path.exists() {
            return Vec::new();
        }
        let check = || -> Result<Vec<&'static str>> {
            let (untracked, modified) = git::status(&record.path)?;
            let mut reasons = Vec::new();
            if modified {
                reasons.push("uncommitted_changes");
            }
            if untracked {
                reasons.push("untracked_files");
            }
            if git::commits_between(&record.path, &record.base_commit, "HEAD")? > 0 {
                reasons.push("commits_beyond_base");
            }
            Ok(reasons)
        };
        match check() {
            Ok(reasons) => reasons.into_iter().map(str::to_string).collect(),
            Err(_) => vec!["state_unknown".to_string()],
        }
    }
}

/// Reuses a live lease. The directory name is the 48-bit short key, so two canonical keys can
/// map to one directory (a UUIDv7 short key is its millisecond timestamp). The lease is shared
/// only when the canonical key is equal; a different canonical key is refused.
fn reuse(live: &LiveLease, source_root: &Path, kind: LeaseKind, canonical: &str) -> Result<Lease> {
    if live.lease.key != canonical {
        return Err(WorkspaceError::new(
            ErrorKind::SharedCheckout,
            format!(
                "short id {} is leased for a different session or task key",
                live.lease.short_id
            ),
        ));
    }
    if !live.owner.is_self() {
        return Err(WorkspaceError::new(
            ErrorKind::SharedCheckout,
            format!("workspace is leased by live process {}", live.owner.pid),
        ));
    }
    if live.lease.source.as_path() != source_root || live.lease.kind != kind {
        return Err(config_err(
            "session is already leased from a different source or kind",
        ));
    }
    if !live.lease.path.exists() {
        return Err(WorkspaceError::new(
            ErrorKind::LeaseLost,
            format!("leased workspace {} is missing", live.lease.path.display()),
        ));
    }
    Ok(Lease::new(live.lease.clone()))
}

fn write_lock(lock: &Path, record: &LeaseRecord) -> Result<()> {
    let owner = OwnerRecord::this_process(Some(record.clone()))?;
    let bytes = serde_json::to_vec(&owner)
        .map_err(|e| WorkspaceError::new(ErrorKind::StateInvalid, e.to_string()))?;
    let created = platform::publish_exclusive(lock, &bytes).map_err(|e| io_err("write lock", e))?;
    if created {
        Ok(())
    } else {
        Err(WorkspaceError::new(
            ErrorKind::SharedCheckout,
            "lease lock was created concurrently",
        ))
    }
}

fn validate_source(source: &Path) -> Result<PathBuf> {
    if !source.is_dir() {
        return Err(config_err(format!(
            "source is not a directory: {}",
            source.display()
        )));
    }
    platform::normalize(source)
}

fn ensure_parent(path: &Path) -> Result<()> {
    match path.parent() {
        Some(parent) => {
            fs::create_dir_all(parent).map_err(|e| io_err("create workspace parent", e))
        }
        None => Ok(()),
    }
}
