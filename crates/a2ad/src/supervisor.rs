//! Supervisor core (RFC-0012 sec 4.2-4.6, RFC-0015 sec 4.8).
//!
//! One exclusive lock per state directory, bus start first, dependency-ordered
//! child processes, readiness probes, bounded restarts and phased shutdown.
//! Operational events are appended to `<state>/logs/supervisor.jsonl`.

use std::collections::VecDeque;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fd_lock::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::broker::{self, Broker, BrokerError};
use crate::config::{AgentSpec, Config, Probe, Readiness, Restart, is_provider_key};
use crate::creds::{self, CredsError, DELEGATION_FILE, DelegationFile, Grant, LOGS_DIR, RUN_DIR};
use crate::health::{HealthHandle, HealthServeError, HealthServer};
use crate::host;
use crate::platform::{self, Containment, PlatformError};

/// Lock file under `run/`; held for the supervisor lifetime (0012-005).
pub const LOCK_FILE: &str = "supervisor.lock";
/// Holder pid, readable by other processes (the lock region is not).
pub const PID_FILE: &str = "supervisor.pid";
/// Created by `a2ad stop`; consumed by the supervisor loop.
pub const STOP_REQUEST: &str = "stop.request";
/// Operational event log under `logs/`.
pub const EVENTS_LOG: &str = "supervisor.jsonl";

const POLL: Duration = Duration::from_millis(100);
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);
const MAX_PROBE_BODY: u64 = 64 * 1024;
const BACKOFF_BASE: Duration = Duration::from_secs(1);
const BACKOFF_CAP: Duration = Duration::from_secs(60);
const RESTART_WINDOW: Duration = Duration::from_secs(600);
const STABLE_AFTER: Duration = Duration::from_secs(600);
/// Failures inside `RESTART_WINDOW` that abandon a process (0012-012).
pub const MAX_FAILURES: usize = 5;
#[cfg(unix)]
const MAX_UNIX_SOCKET_PATH: usize = 103;
#[cfg(unix)]
const SOCKET_FILE: &str = "delegation.sock";
#[cfg(windows)]
const SHIM_FILE: &str = "a2abus-mcp-shim.exe";
#[cfg(not(windows))]
const SHIM_FILE: &str = "a2abus-mcp-shim";

/// Errors of the supervisor run. Exit codes follow 0012-055.
#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    #[error("supervisor lock held by pid {}", holder.map_or_else(|| "unknown".to_owned(), |pid| pid.to_string()))]
    Locked { holder: Option<u32> },
    #[error("agent {agent}: {detail}")]
    Rejected { agent: String, detail: String },
    #[error(transparent)]
    Bus(#[from] BrokerError),
    #[error("bus connection lost")]
    BusLost,
    #[error(transparent)]
    Creds(#[from] CredsError),
    #[error(transparent)]
    Platform(#[from] PlatformError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl SupervisorError {
    /// Process exit code for this error (0012-055).
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Locked { .. } | Self::Bus(_) | Self::BusLost => 6,
            Self::Rejected { .. } | Self::Creds(_) => 4,
            Self::Platform(_) | Self::Io(_) => 1,
        }
    }
}

/// Restart bookkeeping for one process (0012-011, 0012-012). Pure; the clock is passed in.
#[derive(Debug, Default)]
pub struct RestartTracker {
    failures: VecDeque<Instant>,
}

/// What the supervisor does after a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartDecision {
    /// Restart after this delay.
    After(Duration),
    /// Too many failures; stay down until an operator starts the agent.
    Abandon,
}

impl RestartTracker {
    /// Records a failure at `now` and decides what happens next.
    pub fn record_failure(&mut self, now: Instant) -> RestartDecision {
        while self
            .failures
            .front()
            .is_some_and(|first| now.saturating_duration_since(*first) >= RESTART_WINDOW)
        {
            self.failures.pop_front();
        }
        self.failures.push_back(now);
        if self.failures.len() >= MAX_FAILURES {
            return RestartDecision::Abandon;
        }
        RestartDecision::After(backoff(self.failures.len()))
    }

    /// Forgets all failures (called after 10 minutes in Ready).
    pub fn clear(&mut self) {
        self.failures.clear();
    }

    /// Failures still inside the window.
    #[must_use]
    pub fn failure_count(&self) -> usize {
        self.failures.len()
    }
}

/// Backoff before restart number `failures` (1 s, 2 s, 4 s, ... capped at 60 s).
#[must_use]
pub fn backoff(failures: usize) -> Duration {
    let exponent = u32::try_from(failures.saturating_sub(1).min(16)).unwrap_or(16);
    BACKOFF_BASE
        .saturating_mul(2u32.saturating_pow(exponent))
        .min(BACKOFF_CAP)
}

/// Persisted state of one spawned process, `run/<agent>.json` (0012-016 reasons, orphan recovery).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecord {
    pub agent: String,
    pub pid: u32,
    pub start_token: Option<u64>,
    pub state: String,
    pub started_unix_ms: u64,
    pub delegation_file: Option<String>,
    pub endpoint: Option<String>,
    pub delegation_removed: bool,
    pub stop_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Pending,
    Starting,
    Ready,
    Backoff { until: Instant },
    Abandoned,
    Exited,
    Blocked,
}

fn phase_name(phase: Phase) -> &'static str {
    match phase {
        Phase::Pending => "pending",
        Phase::Starting => "starting",
        Phase::Ready => "ready",
        Phase::Backoff { .. } => "backoff",
        Phase::Abandoned => "abandoned",
        Phase::Exited => "exited",
        Phase::Blocked => "blocked",
    }
}

struct Slot {
    id: String,
    phase: Phase,
    child: Option<Child>,
    containment: Option<Containment>,
    restart: RestartTracker,
    started: Option<Instant>,
    ready_since: Option<Instant>,
    delegation_file: Option<PathBuf>,
    socket: Option<PathBuf>,
    record: Option<RunRecord>,
    exit_reason: Option<&'static str>,
}

impl Slot {
    fn new(id: String) -> Self {
        Self {
            id,
            phase: Phase::Pending,
            child: None,
            containment: None,
            restart: RestartTracker::default(),
            started: None,
            ready_since: None,
            delegation_file: None,
            socket: None,
            record: None,
            exit_reason: None,
        }
    }
}

struct PreparedDelegation {
    file: PathBuf,
    endpoint: String,
    socket: Option<PathBuf>,
}

/// The supervisor: owns the bus broker and every child process of one state directory.
pub struct Supervisor {
    config: Config,
    config_path: PathBuf,
    stop: Arc<AtomicBool>,
    order: Vec<usize>,
    slots: Vec<Slot>,
    broker: Option<Broker>,
    health_stop: Arc<AtomicBool>,
}

impl fmt::Debug for Supervisor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Supervisor")
            .field("state_dir", &self.config.state_dir())
            .field("agents", &self.slots.len())
            .finish_non_exhaustive()
    }
}

impl Supervisor {
    /// Builds a supervisor for `config`. `config_path` must be absolute; in-process children re-read it.
    #[must_use]
    pub fn new(config: Config, config_path: PathBuf, stop: Arc<AtomicBool>) -> Self {
        let order = config
            .start_order()
            .iter()
            .filter_map(|spec| config.agents().iter().position(|agent| agent.id == spec.id))
            .collect();
        let slots = config
            .agents()
            .iter()
            .map(|spec| Slot::new(spec.id.clone()))
            .collect();
        Self {
            config,
            config_path,
            stop,
            order,
            slots,
            broker: None,
            health_stop: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Runs until stop is requested, then shuts everything down in phases (0012-014).
    pub fn run(&mut self) -> Result<(), SupervisorError> {
        let state = self.config.state_dir().to_path_buf();
        creds::ensure_layout(&state)?;
        self.check_path_budget()?;
        reclaim_stale(&state)?;
        let mut keys = Vec::new();
        for identity in broker::identities(&self.config) {
            let key = creds::ensure_identity(&state, &identity.name)?;
            keys.push((identity, key));
        }
        let _ = fs::remove_file(stop_request_path(&state));
        let health_server = HealthServer::bind(self.config.health_listen())
            .map_err(|err| SupervisorError::Io(io::Error::other(err.to_string())))?;
        self.broker = Some(Broker::start(&self.config, &keys)?);
        let health_handle = HealthHandle::new();
        let health_thread = self.start_health(health_server, &health_handle);
        health_handle.update(|health| health.bus_connected = true);
        let _ = log_event(
            &state,
            "SUPERVISOR_STARTED",
            None,
            json!({"pid": std::process::id()}),
        );
        let outcome = self.supervise();
        health_handle.update(|health| health.bus_connected = false);
        self.shutdown();
        self.health_stop.store(true, Ordering::SeqCst);
        let _ = health_thread.join();
        outcome
    }

    /// Serves `/healthz` and `/metrics` on a thread until `health_stop` is set (0013-033).
    fn start_health(
        &self,
        server: HealthServer,
        handle: &HealthHandle,
    ) -> JoinHandle<Result<(), HealthServeError>> {
        let thread_handle = handle.clone();
        let stop = Arc::clone(&self.health_stop);
        thread::spawn(move || server.run(&thread_handle, &stop))
    }

    fn check_path_budget(&self) -> Result<(), SupervisorError> {
        let long = platform::long_paths_enabled();
        for spec in self.config.agents() {
            if !spec.is_worker() {
                continue;
            }
            let path = self
                .config
                .state_dir()
                .join(RUN_DIR)
                .join(&spec.id)
                .join(DELEGATION_FILE);
            if !platform::path_length_ok(&path, long) {
                return Err(SupervisorError::Rejected {
                    agent: spec.id.clone(),
                    detail:
                        "path_too_long: state directory too deep for delegation paths (0012-066)"
                            .to_owned(),
                });
            }
        }
        Ok(())
    }

    fn supervise(&mut self) -> Result<(), SupervisorError> {
        let state = self.config.state_dir().to_path_buf();
        loop {
            if self.stop.load(Ordering::SeqCst) || take_stop_request(&state) {
                return Ok(());
            }
            if let Some(broker) = self.broker.as_mut()
                && !broker.is_alive()
            {
                let _ = log_event(&state, "BUS_LOST", None, json!({}));
                return Err(SupervisorError::BusLost);
            }
            self.tick(Instant::now());
            std::thread::sleep(POLL);
        }
    }

    fn tick(&mut self, now: Instant) {
        for k in 0..self.order.len() {
            let i = self.order[k];
            self.reap(i, now);
        }
        for k in 0..self.order.len() {
            let i = self.order[k];
            match self.slots[i].phase {
                Phase::Pending => self.try_start(i, now),
                Phase::Backoff { until } if now >= until => self.try_start(i, now),
                Phase::Starting => self.check_ready(i, now),
                Phase::Ready => {
                    if let Some(since) = self.slots[i].ready_since
                        && now.saturating_duration_since(since) >= STABLE_AFTER
                    {
                        self.slots[i].restart.clear();
                    }
                }
                _ => {}
            }
        }
    }

    fn reap(&mut self, i: usize, now: Instant) {
        let Some(child) = self.slots[i].child.as_mut() else {
            return;
        };
        let success = match child.try_wait() {
            Ok(None) => return,
            Ok(Some(status)) => status.success(),
            Err(_) => false,
        };
        self.on_exit(i, success, now);
    }

    fn on_exit(&mut self, i: usize, success: bool, now: Instant) {
        let state = self.config.state_dir().to_path_buf();
        let id = self.slots[i].id.clone();
        self.release(i, if success { "exited" } else { "failed" });
        if success {
            self.slots[i].phase = Phase::Exited;
            let _ = log_event(&state, "AGENT_EXITED", Some(&id), json!({}));
        } else {
            self.fail(i, now, "failed");
        }
        self.persist(i);
    }

    /// Applies the restart policy after a failure (0012-011, 0012-012).
    fn fail(&mut self, i: usize, now: Instant, reason: &str) {
        let state = self.config.state_dir().to_path_buf();
        let id = self.slots[i].id.clone();
        if matches!(self.config.agents()[i].restart, Restart::Never) {
            self.slots[i].phase = Phase::Exited;
            let _ = log_event(&state, "AGENT_FAILED", Some(&id), json!({"reason": reason}));
            return;
        }
        match self.slots[i].restart.record_failure(now) {
            RestartDecision::After(delay) => {
                self.slots[i].phase = Phase::Backoff { until: now + delay };
                let _ = log_event(
                    &state,
                    "AGENT_RESTART_SCHEDULED",
                    Some(&id),
                    json!({"reason": reason, "delay_secs": delay.as_secs()}),
                );
            }
            RestartDecision::Abandon => {
                let failures = self.slots[i].restart.failure_count();
                self.slots[i].phase = Phase::Abandoned;
                let _ = log_event(
                    &state,
                    "AGENT_ABANDONED",
                    Some(&id),
                    json!({"reason": reason, "failures": failures}),
                );
            }
        }
    }

    fn block(&mut self, i: usize, detail: &str) {
        let state = self.config.state_dir().to_path_buf();
        let id = self.slots[i].id.clone();
        self.slots[i].phase = Phase::Blocked;
        let _ = log_event(
            &state,
            "AGENT_BLOCKED",
            Some(&id),
            json!({"detail": detail}),
        );
    }

    fn try_start(&mut self, i: usize, now: Instant) {
        let deps = self.config.agents()[i].depends_on.clone();
        let mut waiting = false;
        for dep in &deps {
            match self
                .slots
                .iter()
                .find(|slot| &slot.id == dep)
                .map(|slot| slot.phase)
            {
                Some(Phase::Ready) => {}
                Some(Phase::Abandoned | Phase::Exited | Phase::Blocked) | None => {
                    self.block(i, &format!("dependency `{dep}` is not available"));
                    return;
                }
                Some(_) => waiting = true,
            }
        }
        if waiting {
            return;
        }
        match self.spawn(i, now) {
            Ok(()) => {}
            Err(SupervisorError::Rejected { detail, .. }) => self.block(i, &detail),
            Err(other) => self.fail(i, now, &other.to_string()),
        }
    }

    fn check_ready(&mut self, i: usize, now: Instant) {
        let state = self.config.state_dir().to_path_buf();
        let spec = &self.config.agents()[i];
        let ready = probe_ready(&spec.readiness, &state, &spec.id);
        let timeout: Duration = spec.readiness.timeout;
        let id = spec.id.clone();
        if ready {
            self.slots[i].phase = Phase::Ready;
            self.slots[i].ready_since = Some(now);
            self.persist(i);
            let _ = log_event(&state, "AGENT_READY", Some(&id), json!({}));
            return;
        }
        let timed_out = self.slots[i]
            .started
            .is_some_and(|started| now.saturating_duration_since(started) >= timeout);
        if timed_out {
            let _ = log_event(
                &state,
                "AGENT_READINESS_TIMEOUT",
                Some(&id),
                json!({"timeout_secs": timeout.as_secs()}),
            );
            self.force_stop(i);
            self.on_exit(i, false, now);
        }
    }

    /// Starts one process, after its dependencies are Ready (0012-007).
    fn spawn(&mut self, i: usize, now: Instant) -> Result<(), SupervisorError> {
        let spec = self.config.agents()[i].clone();
        let state = self.config.state_dir().to_path_buf();
        let _ = fs::remove_file(host::ready_path(&state, &spec.id));

        let parent = parent_env();
        let base = child_env(&parent, &spec, &[]);
        for name in &spec.env_require {
            if !base.iter().any(|(key, _)| key == name) {
                return Err(SupervisorError::Rejected {
                    agent: spec.id.clone(),
                    detail: format!("required environment variable {name} is not set"),
                });
            }
        }

        let mut extra = vec![
            ("A2AB_AGENT_ID".to_owned(), spec.id.clone()),
            ("A2AB_STATE_DIR".to_owned(), state.display().to_string()),
            (
                "A2AB_CREDS_FILE".to_owned(),
                creds::seed_path(&state, &spec.id).display().to_string(),
            ),
        ];
        let delegation = if spec.is_worker() && !spec.delegation.peers.is_empty() {
            Some(self.prepare_delegation(&spec, &state)?)
        } else {
            None
        };
        if let Some(prepared) = &delegation {
            extra.push((
                "A2AB_DELEGATION_FILE".to_owned(),
                prepared.file.display().to_string(),
            ));
        }
        let env = child_env(&parent, &spec, &extra);
        if spec.passthrough_all {
            let _ = log_event(&state, "AGENT_ENV_PASSTHROUGH", Some(&spec.id), json!({}));
        }

        let mut command = match build_command(&spec, &self.config_path) {
            Ok(command) => command,
            Err(err) => {
                remove_delegation_artifacts(delegation.as_ref());
                return Err(err);
            }
        };
        command
            .env_clear()
            .envs(
                env.iter()
                    .map(|(key, value)| (key.as_str(), value.as_str())),
            )
            .current_dir(&state)
            .stdin(Stdio::piped());
        let log_path = state.join(LOGS_DIR).join(format!("{}.log", spec.id));
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        platform::restrict_to_owner(&log_path, false)?;
        command
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log));

        let (child, containment) = match platform::spawn_contained(&mut command) {
            Ok(pair) => pair,
            Err(err) => {
                remove_delegation_artifacts(delegation.as_ref());
                return Err(err.into());
            }
        };
        let pid = containment.pid();
        let record = RunRecord {
            agent: spec.id.clone(),
            pid,
            start_token: platform::process_start_token(pid),
            state: phase_name(Phase::Starting).to_owned(),
            started_unix_ms: unix_ms(),
            delegation_file: delegation.as_ref().map(|d| d.file.display().to_string()),
            endpoint: delegation.as_ref().map(|d| d.endpoint.clone()),
            delegation_removed: false,
            stop_reason: None,
        };
        if let Err(err) = write_record(&state, &record) {
            let _ = containment.forced();
            remove_delegation_artifacts(delegation.as_ref());
            return Err(err.into());
        }

        let slot = &mut self.slots[i];
        slot.child = Some(child);
        slot.containment = Some(containment);
        slot.delegation_file = delegation.as_ref().map(|d| d.file.clone());
        slot.socket = delegation.and_then(|d| d.socket);
        slot.phase = Phase::Starting;
        slot.started = Some(now);
        slot.ready_since = None;
        slot.exit_reason = None;
        slot.record = Some(record);

        let _ = log_event(&state, "AGENT_STARTED", Some(&spec.id), json!({"pid": pid}));
        if spec.dangerous {
            let _ = log_event(
                &state,
                "DANGEROUS_MODE_ACTIVE",
                Some(&spec.id),
                json!({"mode": spec.approval_mode}),
            );
        }
        Ok(())
    }

    /// Writes the grant file and picks the endpoint (0015-080..0015-083). The worker host binds the endpoint.
    fn prepare_delegation(
        &self,
        spec: &AgentSpec,
        state: &Path,
    ) -> Result<PreparedDelegation, SupervisorError> {
        let run_dir = state.join(RUN_DIR).join(&spec.id);
        fs::create_dir_all(&run_dir)?;
        platform::restrict_to_owner(&run_dir, true)?;
        let shim = match &spec.delegation.shim_path {
            Some(path) => path.clone(),
            None => default_shim_path()?,
        };
        if !shim.is_absolute() {
            return Err(SupervisorError::Rejected {
                agent: spec.id.clone(),
                detail: "delegation shim path must be absolute".to_owned(),
            });
        }
        if !shim.is_file() {
            return Err(SupervisorError::Rejected {
                agent: spec.id.clone(),
                detail: format!("shim not found at {} (dependency_missing)", shim.display()),
            });
        }
        let grant = Grant::generate()?;
        let (endpoint, socket) = delegation_endpoint(&run_dir, &spec.id, &grant)?;
        let file = run_dir.join(DELEGATION_FILE);
        let body = DelegationFile::new(
            &grant,
            endpoint.clone(),
            &shim,
            spec.delegation.peers.clone(),
            (
                spec.delegation.max_inflight,
                spec.delegation.max_sends_per_min,
                spec.delegation.max_calls_per_min,
            ),
        );
        creds::write_delegation(&file, &body)?;
        Ok(PreparedDelegation {
            file,
            endpoint,
            socket,
        })
    }

    /// Stops one slot's containment forcibly and waits for the process.
    fn force_stop(&mut self, i: usize) {
        if let Some(containment) = self.slots[i].containment.as_ref() {
            let _ = containment.forced();
        }
        if let Some(child) = self.slots[i].child.as_mut() {
            let _ = child.wait();
        }
    }

    /// Removes per-run artifacts (delegation file, socket) and records the stop reason.
    fn release(&mut self, i: usize, reason: &str) {
        let slot = &mut self.slots[i];
        if let Some(file) = slot.delegation_file.take()
            && creds::remove_delegation(&file).is_ok()
            && let Some(record) = slot.record.as_mut()
        {
            record.delegation_removed = true;
        }
        if let Some(socket) = slot.socket.take() {
            let _ = fs::remove_file(socket);
        }
        slot.child = None;
        slot.containment = None;
        if let Some(record) = slot.record.as_mut()
            && record.stop_reason.is_none()
        {
            record.stop_reason = Some(reason.to_owned());
        }
    }

    fn persist(&mut self, i: usize) {
        let state = self.config.state_dir().to_path_buf();
        let slot = &mut self.slots[i];
        let Some(record) = slot.record.as_mut() else {
            return;
        };
        phase_name(slot.phase).clone_into(&mut record.state);
        let _ = write_record(&state, record);
    }

    /// Waits for children to exit until `deadline`; records `reason` for those that exit.
    fn await_exits(&mut self, deadline: Instant, reason: &'static str) {
        loop {
            let mut running = false;
            for slot in &mut self.slots {
                let Some(child) = slot.child.as_mut() else {
                    continue;
                };
                match child.try_wait() {
                    Ok(None) => running = true,
                    Ok(Some(_)) | Err(_) => {
                        slot.child = None;
                        slot.containment = None;
                        slot.exit_reason = Some(reason);
                    }
                }
            }
            if !running || Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(POLL);
        }
    }

    /// Phased shutdown, reverse dependency order (0012-014..0012-016): stdin close (drain),
    /// graceful signal (grace), forced kill; then the broker last.
    fn shutdown(&mut self) {
        let state = self.config.state_dir().to_path_buf();
        let grace = self.config.grace();
        let _ = log_event(&state, "SUPERVISOR_DRAINING", None, json!({}));
        for slot in &mut self.slots {
            if let Some(child) = slot.child.as_mut() {
                drop(child.stdin.take());
            }
        }
        let drain_deadline = Instant::now() + self.config.drain();
        self.await_exits(drain_deadline, "exited_during_drain");

        for slot in &self.slots {
            if slot.child.is_some()
                && let Some(containment) = slot.containment.as_ref()
            {
                let _ = containment.graceful();
            }
        }
        self.await_exits(Instant::now() + grace, "graceful");

        for i in 0..self.slots.len() {
            if self.slots[i].child.is_some() {
                self.force_stop(i);
                self.slots[i].exit_reason = Some("forced");
            }
        }

        for i in 0..self.slots.len() {
            let reason = self.slots[i].exit_reason.unwrap_or("stopped");
            self.slots[i].phase = Phase::Exited;
            self.release(i, reason);
            self.persist(i);
        }
        if let Some(mut broker) = self.broker.take() {
            broker.stop(grace);
        }
        let _ = log_event(&state, "SUPERVISOR_STOPPED", None, json!({}));
    }
}

/// Runs the supervisor under the exclusive state lock (0012-005). Returns the exit error when refused.
pub fn run_locked(
    config: Config,
    config_path: PathBuf,
    stop: Arc<AtomicBool>,
) -> Result<(), SupervisorError> {
    let state = config.state_dir().to_path_buf();
    creds::ensure_layout(&state)?;
    let mut lock = open_lock_file(&lock_path(&state))?;
    let mut guard = match lock.try_write() {
        Ok(guard) => guard,
        Err(_) => {
            return Err(SupervisorError::Locked {
                holder: holder_pid(&state),
            });
        }
    };
    let _ = guard.set_len(0);
    guard.write_all(b"")?;
    fs::write(pid_path(&state), format!("{}\n", std::process::id()))?;
    platform::restrict_to_owner(&pid_path(&state), false)?;
    let mut supervisor = Supervisor::new(config, config_path, stop);
    let result = supervisor.run();
    let _ = fs::remove_file(pid_path(&state));
    drop(guard);
    result
}

fn open_lock_file(path: &Path) -> Result<RwLock<File>, SupervisorError> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    platform::restrict_to_owner(path, false)?;
    Ok(RwLock::new(file))
}

fn lock_path(state: &Path) -> PathBuf {
    state.join(RUN_DIR).join(LOCK_FILE)
}

fn pid_path(state: &Path) -> PathBuf {
    state.join(RUN_DIR).join(PID_FILE)
}

fn stop_request_path(state: &Path) -> PathBuf {
    state.join(RUN_DIR).join(STOP_REQUEST)
}

/// Pid of the running supervisor, from its pid file.
#[must_use]
pub fn holder_pid(state: &Path) -> Option<u32> {
    fs::read_to_string(pid_path(state))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Asks a running supervisor to stop (consumed by its loop).
pub fn request_stop(state: &Path) -> io::Result<()> {
    fs::write(stop_request_path(state), b"stop\n")
}

fn take_stop_request(state: &Path) -> bool {
    let path = stop_request_path(state);
    if path.exists() {
        let _ = fs::remove_file(&path);
        return true;
    }
    false
}

/// Reads all run records of the state directory.
pub fn read_records(state: &Path) -> io::Result<Vec<RunRecord>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(state.join(RUN_DIR))? {
        let path = entry?.path();
        if !path.extension().is_some_and(|ext| ext == "json") {
            continue;
        }
        let text = fs::read_to_string(&path)?;
        if let Ok(record) = serde_json::from_str::<RunRecord>(&text) {
            out.push(record);
        }
    }
    out.sort_by(|a, b| a.agent.cmp(&b.agent));
    Ok(out)
}

fn write_record(state: &Path, record: &RunRecord) -> io::Result<()> {
    let path = state.join(RUN_DIR).join(format!("{}.json", record.agent));
    let bytes = serde_json::to_vec_pretty(record).map_err(io::Error::other)?;
    fs::write(&path, bytes)?;
    platform::restrict_to_owner(&path, false).map_err(|err| io::Error::other(err.to_string()))
}

/// Kills trees recorded by a previous supervisor that are still alive, and removes stale grant files (orphan recovery).
fn reclaim_stale(state: &Path) -> Result<(), SupervisorError> {
    for entry in fs::read_dir(state.join(RUN_DIR))? {
        let path = entry?.path();
        if !path.extension().is_some_and(|ext| ext == "json") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(record) = serde_json::from_str::<RunRecord>(&text) {
            if let Some(token) = record.start_token
                && platform::process_start_token(record.pid) == Some(token)
            {
                let _ = platform::kill_recorded_tree(record.pid);
                let _ = log_event(
                    state,
                    "STALE_TREE_KILLED",
                    Some(&record.agent),
                    json!({"pid": record.pid}),
                );
            }
            if let Some(file) = record.delegation_file.as_deref() {
                let _ = creds::remove_delegation(Path::new(file));
            }
            if let Some(endpoint) = record.endpoint.as_deref() {
                let _ = fs::remove_file(endpoint);
            }
        }
        let _ = fs::remove_file(&path);
    }
    Ok(())
}

fn remove_delegation_artifacts(delegation: Option<&PreparedDelegation>) {
    if let Some(prepared) = delegation {
        let _ = creds::remove_delegation(&prepared.file);
        if let Some(socket) = prepared.socket.as_ref() {
            let _ = fs::remove_file(socket);
        }
    }
}

/// Environment for a child: parent minus provider keys (0012-062) and parent `A2AB_*`,
/// plus the agent's `env` table, plus `extra` (supervisor-set `A2AB_*`).
#[must_use]
pub fn child_env(
    parent: &[(String, String)],
    spec: &AgentSpec,
    extra: &[(String, String)],
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = parent
        .iter()
        .filter(|(key, _)| {
            !key.starts_with("A2AB_")
                && (spec.passthrough_all
                    || !is_provider_key(key)
                    || spec.env_allow.iter().any(|allowed| allowed == key))
        })
        .cloned()
        .collect();
    for (key, value) in &spec.env {
        upsert(&mut out, key, value);
    }
    for (key, value) in extra {
        upsert(&mut out, key, value);
    }
    out
}

fn upsert(out: &mut Vec<(String, String)>, key: &str, value: &str) {
    match out.iter_mut().find(|(existing, _)| existing == key) {
        Some(entry) => entry.1 = value.to_owned(),
        None => out.push((key.to_owned(), value.to_owned())),
    }
}

fn parent_env() -> Vec<(String, String)> {
    std::env::vars_os()
        .map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            )
        })
        .collect()
}

fn build_command(spec: &AgentSpec, config_path: &Path) -> Result<Command, SupervisorError> {
    if spec.is_worker() || spec.gateway_config.is_some() {
        let mut command = Command::new(std::env::current_exe()?);
        command
            .arg("host")
            .arg(&spec.id)
            .arg("--config")
            .arg(config_path);
        return Ok(command);
    }
    match spec.command.split_first() {
        Some((program, args)) => {
            let mut command = Command::new(program);
            command.args(args);
            Ok(command)
        }
        None => Err(SupervisorError::Rejected {
            agent: spec.id.clone(),
            detail: "no command configured".to_owned(),
        }),
    }
}

fn default_shim_path() -> Result<PathBuf, SupervisorError> {
    let exe = std::env::current_exe()?;
    let dir = exe
        .parent()
        .ok_or_else(|| io::Error::other("current executable has no parent directory"))?;
    Ok(dir.join(SHIM_FILE))
}

#[cfg(unix)]
fn delegation_endpoint(
    run_dir: &Path,
    id: &str,
    _grant: &Grant,
) -> Result<(String, Option<PathBuf>), SupervisorError> {
    let socket = run_dir.join(SOCKET_FILE);
    if socket.as_os_str().len() > MAX_UNIX_SOCKET_PATH {
        return Err(SupervisorError::Rejected {
            agent: id.to_owned(),
            detail: "delegation socket path too long (path_too_long)".to_owned(),
        });
    }
    Ok((socket.display().to_string(), Some(socket)))
}

#[cfg(windows)]
fn delegation_endpoint(
    _run_dir: &Path,
    id: &str,
    grant: &Grant,
) -> Result<(String, Option<PathBuf>), SupervisorError> {
    Ok((
        format!(r"\\.\pipe\a2abus-{id}-{}", hex(&grant.id[..8])),
        None,
    ))
}

#[cfg(windows)]
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// One readiness check (0012-010). Probes are loopback-only and send no credentials.
fn probe_ready(readiness: &Readiness, state: &Path, id: &str) -> bool {
    match &readiness.probe {
        Probe::Tcp { address } => address.to_string().parse::<SocketAddr>().is_ok_and(|addr| {
            addr.ip().is_loopback() && TcpStream::connect_timeout(&addr, PROBE_TIMEOUT).is_ok()
        }),
        Probe::Http { url } => {
            loopback_get(url).is_some_and(|(status, _)| (200..300).contains(&status))
        }
        Probe::Card { url } => loopback_get(url).is_some_and(|(status, body)| {
            (200..300).contains(&status)
                && serde_json::from_slice::<Value>(&body).is_ok_and(|value| value.is_object())
        }),
        Probe::BusPresence => host::ready_path(state, id).is_file(),
    }
}

/// Minimal loopback HTTP GET without credentials. Returns (status, body).
fn loopback_get(url: &str) -> Option<(u16, Vec<u8>)> {
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(pos) => (&rest[..pos], &rest[pos..]),
        None => (rest, "/"),
    };
    let addr = authority
        .to_socket_addrs()
        .ok()?
        .find(|addr| addr.ip().is_loopback())?;
    let mut stream = TcpStream::connect_timeout(&addr, PROBE_TIMEOUT).ok()?;
    stream.set_read_timeout(Some(PROBE_TIMEOUT * 2)).ok()?;
    stream.set_write_timeout(Some(PROBE_TIMEOUT)).ok()?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\nAccept: application/json\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).ok()?;
    let mut raw = Vec::new();
    let _ = (&mut stream).take(MAX_PROBE_BODY).read_to_end(&mut raw);
    let split = raw.windows(4).position(|window| window == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&raw[..split]).ok()?;
    let status = head
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse::<u16>()
        .ok()?;
    Some((status, raw[split + 4..].to_vec()))
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Appends one operational event (no secrets: ids, pids, reasons only).
fn log_event(state: &Path, event: &str, agent: Option<&str>, detail: Value) -> io::Result<()> {
    let line = json!({
        "ts_unix_ms": unix_ms(),
        "event": event,
        "agent": agent,
        "detail": detail,
    });
    let path = state.join(LOGS_DIR).join(EVENTS_LOG);
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{line}")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn fixture(state: &Path, agents: &str) -> Config {
        let text = format!("config_version = 1\n[bus]\nsha256 = \"{SHA}\"\n{agents}");
        Config::parse(&text, state.to_path_buf()).expect("fixture config parses")
    }

    fn fake_agent(id: &str, depends: &str) -> String {
        let exe = std::env::current_exe()
            .expect("test binary path")
            .display()
            .to_string()
            .replace('\\', "\\\\");
        format!(
            "[[agent]]\nid = \"{id}\"\nkind = \"external\"\ncommand = [\"{exe}\", \"--exact\", \"supervisor::tests::fake_child_waits_for_stdin_eof\", \"--nocapture\"]\nenv = {{ FAKE_CHILD_MARKER = \"1\" }}\n{depends}readiness = {{ kind = \"tcp\", address = \"127.0.0.1:9\", timeout_ms = 1000 }}\n"
        )
    }

    /// Child body for the fake-agent tests: runs only when spawned by a supervisor.
    #[test]
    fn fake_child_waits_for_stdin_eof() {
        if std::env::var_os("FAKE_CHILD_MARKER").is_none() {
            return;
        }
        let mut sink = Vec::new();
        let _ = std::io::stdin().read_to_end(&mut sink);
    }

    #[test]
    fn stable_0012_011_backoff_doubles_and_caps() {
        assert_eq!(backoff(1), Duration::from_secs(1));
        assert_eq!(backoff(2), Duration::from_secs(2));
        assert_eq!(backoff(3), Duration::from_secs(4));
        assert_eq!(backoff(4), Duration::from_secs(8));
        assert_eq!(backoff(7), Duration::from_secs(60));
        assert_eq!(backoff(40), Duration::from_secs(60));
    }

    #[test]
    fn stable_0012_011_restart_delays_then_abandon() {
        let base = Instant::now();
        let mut tracker = RestartTracker::default();
        assert_eq!(
            tracker.record_failure(base),
            RestartDecision::After(Duration::from_secs(1))
        );
        assert_eq!(
            tracker.record_failure(base + Duration::from_secs(1)),
            RestartDecision::After(Duration::from_secs(2))
        );
        assert_eq!(
            tracker.record_failure(base + Duration::from_secs(2)),
            RestartDecision::After(Duration::from_secs(4))
        );
        assert_eq!(
            tracker.record_failure(base + Duration::from_secs(3)),
            RestartDecision::After(Duration::from_secs(8))
        );
    }

    #[test]
    fn stable_0012_012_fifth_failure_in_window_abandons() {
        let base = Instant::now();
        let mut tracker = RestartTracker::default();
        for step in 0..4 {
            let at = base + Duration::from_secs(step);
            assert!(matches!(
                tracker.record_failure(at),
                RestartDecision::After(_)
            ));
        }
        assert_eq!(
            tracker.record_failure(base + Duration::from_secs(4)),
            RestartDecision::Abandon
        );
    }

    #[test]
    fn stable_0012_012_failures_outside_window_do_not_count() {
        let base = Instant::now();
        let mut tracker = RestartTracker::default();
        for step in 0..4 {
            let _ = tracker.record_failure(base + Duration::from_secs(step));
        }
        let later = base + RESTART_WINDOW + Duration::from_secs(10);
        assert!(matches!(
            tracker.record_failure(later),
            RestartDecision::After(_)
        ));
        assert_eq!(tracker.failure_count(), 1);
    }

    #[test]
    fn stable_0012_011_stable_ready_clears_backoff() {
        let base = Instant::now();
        let mut tracker = RestartTracker::default();
        let _ = tracker.record_failure(base);
        let _ = tracker.record_failure(base);
        tracker.clear();
        assert_eq!(
            tracker.record_failure(base),
            RestartDecision::After(Duration::from_secs(1))
        );
    }

    #[test]
    fn stable_0012_026_provider_keys_never_reach_child_unless_allowed() {
        let state = tempfile::tempdir().expect("tempdir");
        let config = fixture(state.path(), &fake_agent("peer", ""));
        let spec = config.agent("peer").expect("agent").clone();
        let parent = vec![
            ("ANTHROPIC_API_KEY".to_owned(), "k1".to_owned()),
            ("OPENAI_API_KEY".to_owned(), "k2".to_owned()),
            ("OMP_API_KEY".to_owned(), "k3".to_owned()),
            ("A2AB_LEFTOVER".to_owned(), "x".to_owned()),
            ("PATH".to_owned(), "/bin".to_owned()),
        ];
        let env = child_env(&parent, &spec, &[]);
        let keys: Vec<&str> = env.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(keys, vec!["PATH", "FAKE_CHILD_MARKER"]);

        let mut allowed = spec.clone();
        allowed.env_allow = vec!["ANTHROPIC_API_KEY".to_owned()];
        let env = child_env(&parent, &allowed, &[]);
        assert!(
            env.iter()
                .any(|(key, value)| key == "ANTHROPIC_API_KEY" && value == "k1")
        );
        assert!(!env.iter().any(|(key, _)| key == "OPENAI_API_KEY"));
    }

    #[test]
    fn stable_0012_026_passthrough_all_keeps_provider_keys() {
        let state = tempfile::tempdir().expect("tempdir");
        let config = fixture(state.path(), &fake_agent("peer", ""));
        let mut spec = config.agent("peer").expect("agent").clone();
        spec.passthrough_all = true;
        let parent = vec![("OPENAI_API_KEY".to_owned(), "k2".to_owned())];
        assert!(
            child_env(&parent, &spec, &[])
                .iter()
                .any(|(key, _)| key == "OPENAI_API_KEY")
        );
    }

    #[test]
    fn stable_0012_027_child_env_extras_carry_paths_not_seeds() {
        let state = tempfile::tempdir().expect("tempdir");
        let config = fixture(state.path(), &fake_agent("peer", ""));
        let spec = config.agent("peer").expect("agent").clone();
        let seed_file = creds::seed_path(state.path(), "peer");
        let extra = vec![(
            "A2AB_CREDS_FILE".to_owned(),
            seed_file.display().to_string(),
        )];
        let env = child_env(&[], &spec, &extra);
        let value = env
            .iter()
            .find(|(key, _)| key == "A2AB_CREDS_FILE")
            .map(|(_, value)| value.clone())
            .expect("creds path present");
        assert!(value.ends_with(".nk"));
        assert!(!env.iter().any(|(_, value)| value.starts_with("SU")));
    }

    #[test]
    fn stable_0012_007_dependencies_start_first() {
        let state = tempfile::tempdir().expect("tempdir");
        let agents = format!(
            "{}{}",
            fake_agent("b", "depends_on = [\"a\"]\n"),
            fake_agent("a", "")
        );
        let config = fixture(state.path(), &agents);
        let ids: Vec<&str> = config
            .start_order()
            .iter()
            .map(|spec| spec.id.as_str())
            .collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn stable_0012_005_second_lock_is_refused() {
        let state = tempfile::tempdir().expect("tempdir");
        creds::ensure_layout(state.path()).expect("layout");
        let path = lock_path(state.path());
        let mut first = open_lock_file(&path).expect("open first");
        let _held = first.try_write().expect("first lock");
        let mut second = open_lock_file(&path).expect("open second");
        assert!(second.try_write().is_err());
    }

    #[test]
    fn stable_0012_010_loopback_http_probe_sends_no_credentials() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = Vec::new();
            let mut buf = [0u8; 2048];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut buf).expect("read");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..read]);
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
                .expect("write");
            String::from_utf8_lossy(&request).into_owned()
        });
        let (status, body) = loopback_get(&format!("http://{addr}/health")).expect("response");
        assert_eq!(status, 200);
        assert_eq!(body, b"{}");
        let request = server.join().expect("server");
        assert!(request.starts_with("GET /health HTTP/1.1"), "{request:?}");
        assert!(!request.to_ascii_lowercase().contains("authorization"));
    }

    #[test]
    fn stable_0012_010_probe_refuses_non_loopback_and_closed_ports() {
        assert!(loopback_get("http://10.0.0.5:9/health").is_none());
        let closed = TcpListener::bind("127.0.0.1:0")
            .expect("bind")
            .local_addr()
            .expect("addr");
        let tcp = Readiness {
            probe: Probe::Tcp {
                address: closed.to_string(),
            },
            timeout: Duration::from_secs(1),
        };
        assert!(!probe_ready(&tcp, Path::new("."), "peer"));
    }

    #[test]
    fn stable_0012_014_stdin_close_drains_child() {
        let state = tempfile::tempdir().expect("tempdir");
        creds::ensure_layout(state.path()).expect("layout");
        let config = fixture(state.path(), &fake_agent("fake", ""));
        let mut sup = Supervisor::new(
            config,
            state.path().join("unused.toml"),
            Arc::new(AtomicBool::new(false)),
        );
        sup.spawn(0, Instant::now()).expect("spawn");
        let records = read_records(state.path()).expect("records");
        assert_eq!(records.len(), 1);
        assert!(records[0].pid > 0);
        assert!(records[0].start_token.is_some());

        sup.shutdown();

        let after = read_records(state.path()).expect("records");
        assert_eq!(after[0].state, "exited");
        assert_eq!(after[0].stop_reason.as_deref(), Some("exited_during_drain"));
        assert!(!after[0].delegation_removed);
    }

    #[test]
    fn stable_0012_015_default_drain_and_grace_are_applied() {
        let state = tempfile::tempdir().expect("tempdir");
        let config = fixture(state.path(), &fake_agent("fake", ""));
        assert_eq!(config.drain(), Duration::from_millis(15_000));
        assert_eq!(config.grace(), Duration::from_millis(10_000));
    }
}
