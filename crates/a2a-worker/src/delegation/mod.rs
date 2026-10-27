//! Peer-call delegation (RFC-0015): the local IPC endpoint the MCP shim calls,
//! the per-task bindings and grants that authenticate it, and the lifecycle of
//! the remote children a task creates.
//!
//! [`Worker::pump_delegation`] takes the delegation state out of the worker for
//! the duration of a pass, so requests are served without re-entering the worker.
//! Effects produced by the pass are applied after the state is restored.

mod policy;
mod result;
mod send;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};

use a2a_client::{CallOptions, Client, Config as ClientConfig};
use a2a_core::budget::Usage;
use a2a_core::task::{ChildEnd, Effect, TaskError, TaskStore};
use a2a_mcp::ipc::{
    ClientFrame, GRANT_CREDENTIAL_BYTES, GRANT_ID_BYTES, IpcError, Response, SESSION_SLOT_BYTES,
    Secret, TaskResult, constant_time_eq, reason,
};
use a2a_mcp::ipc_server::{GrantTable, ServerHandle, ServerRequest};
use a2a_types::ids::{AgentId, TaskId};
use a2a_types::proto::a2a::{self as pb, TaskState};
use base64::Engine as _;
use bus_api::bus::Bus;

use crate::backend::{Backend, PeerTools, SecretText};
use crate::error::WorkerError;
use crate::worker::Worker;
use policy::Window;

/// Interval at which the endpoint is polled when no other deadline is nearer.
pub(crate) const IPC_POLL_MS: u64 = 100;
/// Interval between `get` polls of a non-terminal remote child.
pub(crate) const CHILD_POLL_MS: u64 = 250;

const DEFAULT_MAX_INFLIGHT: u32 = 4;
const DEFAULT_MAX_SENDS_PER_MIN: u32 = 30;
const DEFAULT_MAX_CALLS_PER_MIN: u32 = 120;

/// Settings of the delegation endpoint (RFC-0015 section 4.6, 0015-087).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegationConfig {
    /// Local IPC endpoint the MCP shim connects to.
    pub endpoint: PathBuf,
    /// Path of the `a2abus-mcp-shim` binary started by the agent.
    pub shim_path: PathBuf,
    /// Allowlisted peers: the only agents a task may delegate to or discover.
    pub peers: Vec<AgentId>,
    /// Non-terminal children one task may hold at once (1 to 16).
    pub max_inflight: u32,
    /// Admitted `a2a_send` calls per task per minute (1 to 600).
    pub max_sends_per_min: u32,
    /// Admitted calls per task per minute, all tools (1 to 1200).
    pub max_calls_per_min: u32,
}

impl DelegationConfig {
    /// Creates a configuration with the default limits.
    pub fn new(
        endpoint: impl Into<PathBuf>,
        shim_path: impl Into<PathBuf>,
        peers: Vec<AgentId>,
    ) -> Self {
        Self {
            endpoint: endpoint.into(),
            shim_path: shim_path.into(),
            peers,
            max_inflight: DEFAULT_MAX_INFLIGHT,
            max_sends_per_min: DEFAULT_MAX_SENDS_PER_MIN,
            max_calls_per_min: DEFAULT_MAX_CALLS_PER_MIN,
        }
    }

    fn validate(&self, own: &AgentId) -> Result<(), WorkerError> {
        if self.endpoint.as_os_str().is_empty() {
            return Err(WorkerError::Config("delegation endpoint is empty"));
        }
        if self.shim_path.as_os_str().is_empty() {
            return Err(WorkerError::Config("delegation shim path is empty"));
        }
        if !(1..=16).contains(&self.max_inflight) {
            return Err(WorkerError::Config("max_inflight must be 1 to 16"));
        }
        if !(1..=600).contains(&self.max_sends_per_min) {
            return Err(WorkerError::Config("max_sends_per_min must be 1 to 600"));
        }
        if !(1..=1200).contains(&self.max_calls_per_min) {
            return Err(WorkerError::Config("max_calls_per_min must be 1 to 1200"));
        }
        for peer in &self.peers {
            if peer == own {
                return Err(WorkerError::Config(
                    "delegation peers must not include the worker itself",
                ));
            }
            if self.peers.iter().filter(|p| *p == peer).count() > 1 {
                return Err(WorkerError::Config("delegation peers must be unique"));
            }
        }
        Ok(())
    }
}

/// Grant table shared with the IPC server. Revocation is visible immediately.
#[derive(Debug, Default)]
pub(crate) struct GrantBook {
    grants: BTreeMap<String, Grant>,
    stopped: bool,
}

#[derive(Debug)]
struct Grant {
    credential: String,
    slot: String,
    live: bool,
}

#[derive(Clone)]
struct Grants(Arc<Mutex<GrantBook>>);

impl GrantTable for Grants {
    fn authenticate(
        &self,
        grant_id: &str,
        credential: &Secret,
        session_slot: &str,
    ) -> Result<(), IpcError> {
        let book = self
            .0
            .lock()
            .map_err(|_| IpcError::new(reason::UNAVAILABLE))?;
        let grant = book.grants.get(grant_id);
        let expected = grant.map_or("", |g| g.credential.as_str());
        let matches = constant_time_eq(expected.as_bytes(), credential.expose().as_bytes());
        let Some(grant) = grant.filter(|_| matches) else {
            return Err(IpcError::new(reason::UNAUTHENTICATED));
        };
        if book.stopped || !grant.live {
            return Err(IpcError::new(reason::GRANT_REVOKED));
        }
        if grant.slot != session_slot {
            return Err(IpcError::new(reason::SLOT_UNBOUND));
        }
        Ok(())
    }
}

/// The per-task binding: one slot, one grant, idempotency keys and rate windows.
#[derive(Debug)]
struct Binding {
    slot: String,
    grant_id: String,
    credential: String,
    /// Idempotency key to the child's local task id (0015-048).
    keys: BTreeMap<String, String>,
    calls: Window,
    sends: Window,
}

/// A call waiting for a child's outcome.
pub(crate) struct Waiter {
    request: ServerRequest,
    call_id: u64,
    deadline: u64,
}

/// A remote child created by a delegating task.
pub(crate) struct Child {
    parent: String,
    target: AgentId,
    remote: TaskId,
    context_id: String,
    chain_length: u32,
    state: TaskState,
    cancel_requested: bool,
    settled: Option<Result<TaskResult, IpcError>>,
    waiters: Vec<Waiter>,
    next_poll: u64,
    deadline: u64,
}

/// Delegation state of a worker with an attached endpoint.
pub(crate) struct Delegation<B: Bus> {
    pub(crate) config: DelegationConfig,
    own: AgentId,
    grants: Arc<Mutex<GrantBook>>,
    /// Keeps the IPC endpoint serving; dropping the handle stops it.
    _server: ServerHandle,
    requests: Receiver<ServerRequest>,
    pub(crate) client: Client<Arc<B>>,
    /// Bindings keyed by parent task id.
    bindings: BTreeMap<String, Binding>,
    /// Slot to parent task id.
    slots: BTreeMap<String, String>,
    /// Children keyed by local child task id.
    children: BTreeMap<String, Child>,
    /// Local child ids whose remote task must be canceled.
    pub(crate) cancels: Vec<String>,
}

impl<B: Bus> Delegation<B> {
    /// Next time the endpoint needs service, in milliseconds.
    pub(crate) fn wakeup(&self, now: u64) -> u64 {
        let mut next = now.saturating_add(IPC_POLL_MS);
        for child in self.children.values().filter(|c| c.settled.is_none()) {
            next = next.min(child.next_poll);
            for waiter in &child.waiters {
                next = next.min(waiter.deadline);
            }
        }
        next
    }
}

impl<B: Bus, D: Backend, S: TaskStore> Worker<B, D, S> {
    /// Attaches the delegation endpoint (RFC-0015). Fails if one is attached.
    pub fn attach_delegation(&mut self, config: DelegationConfig) -> Result<(), WorkerError> {
        if self.delegation.is_some() {
            return Err(WorkerError::Config("delegation already attached"));
        }
        config.validate(&self.config.agent)?;
        let endpoint = config
            .endpoint
            .to_str()
            .ok_or(WorkerError::Config("delegation endpoint is not UTF-8"))?
            .to_owned();
        let grants = Arc::new(Mutex::new(GrantBook::default()));
        let (sender, requests) = mpsc::channel();
        let server = a2a_mcp::ipc_server::spawn(
            &endpoint,
            Arc::new(Grants(Arc::clone(&grants))),
            sender,
        )
        .map_err(|_| WorkerError::Config("delegation endpoint unavailable"))?;
        let client = Client::new(
            Arc::clone(&self.bus),
            Arc::clone(&self.clock),
            ClientConfig::default(),
            self.config.tenant.clone(),
            self.config.agent.clone(),
        )
        .map_err(|_| WorkerError::Config("delegation client unavailable"))?;
        self.delegation = Some(Delegation {
            config,
            own: self.config.agent.clone(),
            grants,
            _server: server,
            requests,
            client,
            bindings: BTreeMap::new(),
            slots: BTreeMap::new(),
            children: BTreeMap::new(),
            cancels: Vec::new(),
        });
        Ok(())
    }

    /// Stops delegation (RFC-0015 0015-074): revokes every grant, answers pending
    /// calls with `GRANT_REVOKED` and closes the endpoint.
    pub fn shutdown_delegation(&mut self) {
        let Some(mut d) = self.delegation.take() else {
            return;
        };
        if let Ok(mut book) = d.grants.lock() {
            book.stopped = true;
            for grant in book.grants.values_mut() {
                grant.live = false;
            }
        }
        while let Ok(request) = d.requests.try_recv() {
            respond_err(request, IpcError::new(reason::GRANT_REVOKED));
        }
        for child in d.children.values_mut() {
            for waiter in child.waiters.drain(..) {
                respond_err(waiter.request, IpcError::new(reason::GRANT_REVOKED));
            }
        }
    }

    /// Binds a task's peer tools, creating its slot and grant on first use.
    /// Returns `None` when no delegation endpoint is attached.
    pub(crate) fn bind_peer_tools(&mut self, task: &str) -> Result<Option<PeerTools>, WorkerError> {
        let Some(d) = self.delegation.as_mut() else {
            return Ok(None);
        };
        if !d.bindings.contains_key(task) {
            let slot = random_token::<SESSION_SLOT_BYTES>()?;
            let grant_id = random_token::<GRANT_ID_BYTES>()?;
            let credential = random_token::<GRANT_CREDENTIAL_BYTES>()?;
            d.grants
                .lock()
                .map_err(|_| WorkerError::Config("grant table unavailable"))?
                .grants
                .insert(
                    grant_id.clone(),
                    Grant {
                        credential: credential.clone(),
                        slot: slot.clone(),
                        live: true,
                    },
                );
            d.slots.insert(slot.clone(), task.to_owned());
            d.bindings.insert(
                task.to_owned(),
                Binding {
                    slot,
                    grant_id,
                    credential,
                    keys: BTreeMap::new(),
                    calls: Window::default(),
                    sends: Window::default(),
                },
            );
        }
        let binding = d
            .bindings
            .get(task)
            .ok_or(WorkerError::Config("peer binding missing"))?;
        Ok(Some(PeerTools {
            slot: binding.slot.clone(),
            grant_id: binding.grant_id.clone(),
            grant_credential: SecretText::new(binding.credential.clone()),
            endpoint: d.config.endpoint.clone(),
            shim_path: d.config.shim_path.clone(),
        }))
    }

    /// Serves delegation requests and child polls. Called from [`Worker::pump`].
    pub(crate) async fn pump_delegation(&mut self, now: u64) -> Result<(), WorkerError> {
        let Some(mut d) = self.delegation.take() else {
            return Ok(());
        };
        let served = self.serve_delegation(&mut d, now).await;
        self.delegation = Some(d);
        let effects = served?;
        self.run(effects, now)
    }

    async fn serve_delegation(
        &mut self,
        d: &mut Delegation<B>,
        now: u64,
    ) -> Result<Vec<Effect>, WorkerError> {
        let mut effects = Vec::new();
        self.teardown_ended(d);
        while let Ok(request) = d.requests.try_recv() {
            self.dispatch(d, request, now, &mut effects).await?;
        }
        flush_cancels(d).await;
        self.poll_children(d, now, &mut effects).await?;
        expire_waiters(d, now);
        Ok(effects)
    }

    /// Ends bindings whose parent task is terminal or gone (0015-073). Pending calls
    /// are answered `UNAVAILABLE`; unsettled children stay tracked until they settle.
    fn teardown_ended(&mut self, d: &mut Delegation<B>) {
        let ended: Vec<String> = d
            .bindings
            .keys()
            .filter(|task| !self.parent_open(task))
            .cloned()
            .collect();
        for task in ended {
            let Some(binding) = d.bindings.remove(&task) else {
                continue;
            };
            d.slots.remove(&binding.slot);
            if let Ok(mut book) = d.grants.lock() {
                if let Some(grant) = book.grants.get_mut(&binding.grant_id) {
                    grant.live = false;
                }
            }
            for child in d.children.values_mut().filter(|c| c.parent == task) {
                for waiter in child.waiters.drain(..) {
                    respond_err(waiter.request, IpcError::new(reason::UNAVAILABLE));
                }
            }
            d.children
                .retain(|_, c| c.parent != task || c.settled.is_none());
        }
    }

    fn parent_open(&self, task: &str) -> bool {
        let Some(owner) = self.tasks.task_owner(task) else {
            return false;
        };
        self.tasks
            .task(&owner.origin, task)
            .is_ok_and(|view| !view.state.is_terminal())
    }

    async fn dispatch(
        &mut self,
        d: &mut Delegation<B>,
        request: ServerRequest,
        now: u64,
        effects: &mut Vec<Effect>,
    ) -> Result<(), WorkerError> {
        let frame = request.frame.clone();
        let Some(task) = d.slots.get(&request.slot).cloned() else {
            respond_err(request, IpcError::new(reason::SLOT_UNBOUND));
            return Ok(());
        };
        if !matches!(frame, ClientFrame::CancelCall { .. }) {
            let max = d.config.max_calls_per_min;
            let admitted = d
                .bindings
                .get_mut(&task)
                .is_some_and(|b| b.calls.try_admit(now, max));
            if !admitted {
                self.refuse(Some(&task), request, IpcError::new(reason::RATE_LIMITED));
                return Ok(());
            }
        }
        match frame {
            ClientFrame::Discover { params, .. } => {
                self.discover(d, request, &task, &params).await;
            }
            ClientFrame::Send { params, .. } => {
                self.send_call(d, request, &task, &params, now, effects)
                    .await?;
            }
            ClientFrame::Get { params, .. } => self.get_call(d, request, &task, &params),
            ClientFrame::Cancel { params, .. } => {
                self.cancel_child(d, request, &task, &params.task_id);
            }
            ClientFrame::CancelCall { call_id, .. } => {
                self.cancel_call(d, request, &task, call_id);
            }
        }
        Ok(())
    }

    /// Records an audit note and answers the call with the refusal (0015-075).
    pub(crate) fn refuse(&mut self, task: Option<&str>, request: ServerRequest, error: IpcError) {
        self.audit(task, format!("DELEGATION_REFUSED {}", error.reason));
        respond_err(request, error);
    }

    async fn poll_children(
        &mut self,
        d: &mut Delegation<B>,
        now: u64,
        effects: &mut Vec<Effect>,
    ) -> Result<(), WorkerError> {
        let due: Vec<String> = d
            .children
            .iter()
            .filter(|(_, c)| c.settled.is_none() && c.next_poll <= now)
            .map(|(id, _)| id.clone())
            .collect();
        for id in due {
            let Some(child) = d.children.get(&id) else {
                continue;
            };
            let (target, remote) = (child.target.clone(), child.remote.clone());
            match d
                .client
                .get_task(&target, &remote, &CallOptions::default())
                .await
            {
                Ok(unary) => self.observe(d, &id, &unary.value, now, effects)?,
                Err(_) => {
                    if let Some(child) = d.children.get_mut(&id) {
                        child.next_poll = now.saturating_add(CHILD_POLL_MS);
                    }
                }
            }
        }
        Ok(())
    }

    /// Applies a remote child's observed state. A terminal state settles the child
    /// in the parent task and answers every waiting call.
    pub(crate) fn observe(
        &mut self,
        d: &mut Delegation<B>,
        child_id: &str,
        task: &pb::Task,
        now: u64,
        effects: &mut Vec<Effect>,
    ) -> Result<(), WorkerError> {
        let status = task.status.as_ref();
        let state = status
            .and_then(|s| TaskState::from_wire(s.state).ok())
            .unwrap_or(TaskState::Unspecified);
        let Some(child) = d.children.get_mut(child_id) else {
            return Ok(());
        };
        child.state = state;
        if !state.is_terminal() {
            child.next_poll = now.saturating_add(CHILD_POLL_MS);
            return Ok(());
        }
        if child.settled.is_some() {
            return Ok(());
        }
        let code = status
            .and_then(|s| s.message.as_ref())
            .and_then(|m| a2a_types::failure::FailureReason::from_message(m).ok().flatten())
            .map(|r| r.code.to_string());
        let parent = child.parent.clone();
        let cancelled_by_parent = child.cancel_requested && state == TaskState::Canceled;
        let settled = result::outcome(child_id, child, task, state, code.as_deref());
        match self.tasks.settle_remote_child(
            &parent,
            child_id,
            ChildEnd {
                state,
                code,
                usage: Usage::default(),
                cancelled_by_parent,
            },
            now,
        ) {
            Ok(new_effects) => effects.extend(new_effects),
            Err(TaskError::NotActive | TaskError::Terminal | TaskError::NotFound) => {}
            Err(err) => return Err(WorkerError::Tasks(err)),
        }
        if let Some(child) = d.children.get_mut(child_id) {
            for waiter in child.waiters.drain(..) {
                respond_outcome(waiter.request, &settled);
            }
            child.settled = Some(settled);
        }
        self.audit(
            Some(&parent),
            format!("DELEGATION_CHILD_SETTLED {}", result::state_name(state)),
        );
        Ok(())
    }

    /// Settles a child whose send failed or whose remote id was unusable.
    pub(crate) fn settle_unsent(
        &mut self,
        task: &str,
        child_id: &str,
        now: u64,
        effects: &mut Vec<Effect>,
    ) -> Result<(), WorkerError> {
        let ended = self.tasks.settle_remote_child(
            task,
            child_id,
            ChildEnd {
                state: TaskState::Canceled,
                code: None,
                usage: Usage::default(),
                cancelled_by_parent: false,
            },
            now,
        );
        match ended {
            Ok(new_effects) => effects.extend(new_effects),
            Err(TaskError::NotActive | TaskError::Terminal | TaskError::NotFound) => {}
            Err(err) => return Err(WorkerError::Tasks(err)),
        }
        Ok(())
    }
}

async fn flush_cancels<B: Bus>(d: &mut Delegation<B>) {
    for child_id in std::mem::take(&mut d.cancels) {
        let Some(child) = d.children.get(&child_id) else {
            continue;
        };
        if child.settled.is_some() {
            continue;
        }
        let (target, remote) = (child.target.clone(), child.remote.clone());
        let _ = d
            .client
            .cancel_task(&target, &remote, &CallOptions::default())
            .await;
    }
}

fn expire_waiters<B: Bus>(d: &mut Delegation<B>, now: u64) {
    for (child_id, child) in &mut d.children {
        let (due, rest): (Vec<Waiter>, Vec<Waiter>) = child
            .waiters
            .drain(..)
            .partition(|w| w.deadline <= now);
        child.waiters = rest;
        for waiter in due {
            let snapshot = result::snapshot(child_id, child);
            respond_outcome(waiter.request, &Ok(snapshot));
        }
    }
}

/// The child's current answer: its settled outcome, or a snapshot while it runs.
pub(crate) fn current(child_id: &str, child: &Child) -> Result<TaskResult, IpcError> {
    match &child.settled {
        Some(settled) => settled.clone(),
        None => Ok(result::snapshot(child_id, child)),
    }
}

pub(crate) fn respond_err(request: ServerRequest, error: IpcError) {
    let id = request.frame.id();
    request.respond(Response::failure(id, error));
}

pub(crate) fn respond_outcome(request: ServerRequest, outcome: &Result<TaskResult, IpcError>) {
    let id = request.frame.id();
    let response = match outcome {
        Ok(value) => Response::success(id, value)
            .unwrap_or_else(|_| Response::failure(id, IpcError::new(reason::UNAVAILABLE))),
        Err(error) => Response::failure(id, error.clone()),
    };
    request.respond(response);
}

pub(crate) fn respond_value<T: serde::Serialize>(request: ServerRequest, value: &T) {
    let id = request.frame.id();
    let response = Response::success(id, value)
        .unwrap_or_else(|_| Response::failure(id, IpcError::new(reason::UNAVAILABLE)));
    request.respond(response);
}

/// Random bytes as unpadded base64url, the encoding the IPC frames carry.
fn random_token<const N: usize>() -> Result<String, WorkerError> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes)
        .map_err(|_| WorkerError::Config("random source unavailable"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}
