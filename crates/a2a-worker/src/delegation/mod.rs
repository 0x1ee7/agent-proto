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

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};

use a2a_client::{CallOptions, Client, Config as ClientConfig, Outbound};
use a2a_core::budget::Usage;
use a2a_core::envelope::{Clock, SystemClock};
use a2a_core::task::{ChildEnd, Effect, TaskError, TaskStore};
use a2a_mcp::ipc::{
    ClientFrame, GRANT_CREDENTIAL_BYTES, GRANT_ID_BYTES, IpcError, Response, SESSION_SLOT_BYTES,
    Secret, TaskResult, constant_time_eq, reason,
};
use a2a_mcp::ipc_server::{GrantTable, ServerHandle, ServerRequest};
use a2a_types::ids::{AgentId, MessageId, TaskId};
use a2a_types::proto::a2a::{self as pb, TaskState};
use a2a_types::proto::a2abus::Hop;
use a2a_types::vocab::ext;
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

/// Idempotency ledger keyed by (parent task id, idempotency key) (0015-048).
pub(crate) type Ledger = BTreeMap<(String, String), String>;

/// Reads a string field of a journal record; a missing field means the journal is corrupt.
fn journal_str<'a>(record: &'a serde_json::Value, name: &str) -> Result<&'a str, WorkerError> {
    record
        .get(name)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(corrupt_journal)
}

fn corrupt_journal() -> WorkerError {
    WorkerError::Config("delegation journal is corrupt")
}

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

/// Failed authentications within [`LOCKOUT_WINDOW_MS`] that start a lockout (0015-043).
const LOCKOUT_FAILURES: usize = 5;
/// Length of the failure window and of the lockout, in milliseconds (0015-043).
const LOCKOUT_WINDOW_MS: u64 = 60_000;
/// Owner of the delegation journal records in the task store (0015-062, 0015-063).
pub(crate) const JOURNAL_OWNER: &str = "a2a-worker.delegation";

/// Grant table shared with the IPC server. Revocation is visible immediately. Failed
/// authentications are counted across connections; after [`LOCKOUT_FAILURES`] within a
/// window, every authentication is refused until the lockout ends (0015-043).
pub(crate) struct GrantBook {
    grants: BTreeMap<String, Grant>,
    stopped: bool,
    clock: Arc<dyn Clock>,
    /// Times of recent failed authentications inside the window.
    failures: VecDeque<u64>,
    /// Authentication is refused while `now` is before this time, in milliseconds.
    locked_until: u64,
}

impl Default for GrantBook {
    fn default() -> Self {
        Self::new(Arc::new(SystemClock))
    }
}

impl GrantBook {
    pub(crate) fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            grants: BTreeMap::new(),
            stopped: false,
            clock,
            failures: VecDeque::new(),
            locked_until: 0,
        }
    }

    /// Checks a grant id, credential and slot without counting failures.
    fn verify(
        &self,
        grant_id: &str,
        credential: &Secret,
        session_slot: &str,
    ) -> Result<(), IpcError> {
        let grant = self.grants.get(grant_id);
        let expected = grant.map_or("", |g| g.credential.as_str());
        let matches = constant_time_eq(expected.as_bytes(), credential.expose().as_bytes());
        let Some(grant) = grant.filter(|_| matches) else {
            return Err(IpcError::new(reason::UNAUTHENTICATED));
        };
        if self.stopped || !grant.live {
            return Err(IpcError::new(reason::GRANT_REVOKED));
        }
        if grant.slot != session_slot {
            return Err(IpcError::new(reason::SLOT_UNBOUND));
        }
        Ok(())
    }

    /// Records a failed authentication at `now`. The fifth failure within the window starts
    /// a lockout and clears the count.
    fn record_failure(&mut self, now: u64) {
        while self
            .failures
            .front()
            .is_some_and(|t| now.saturating_sub(*t) >= LOCKOUT_WINDOW_MS)
        {
            self.failures.pop_front();
        }
        self.failures.push_back(now);
        if self.failures.len() >= LOCKOUT_FAILURES {
            self.failures.clear();
            self.locked_until = now.saturating_add(LOCKOUT_WINDOW_MS);
        }
    }
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
        let mut book = self
            .0
            .lock()
            .map_err(|_| IpcError::new(reason::UNAVAILABLE))?;
        let now = book
            .clock
            .now_ms()
            .ok_or_else(|| IpcError::new(reason::UNAVAILABLE))?;
        if now < book.locked_until {
            return Err(IpcError::new(reason::UNAUTHENTICATED));
        }
        let outcome = book.verify(grant_id, credential, session_slot);
        if outcome.is_err() {
            book.record_failure(now);
        }
        outcome
    }
}

/// The per-task binding: one slot, one grant, idempotency keys and rate windows.
#[derive(Debug)]
pub(crate) struct Binding {
    slot: String,
    grant_id: String,
    credential: String,
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

/// Delegation state of a worker with an attached endpoint. It is taken out of the worker
/// while served (see [`Worker::pump_delegation`]); state that bound tasks need lives in
/// [`Peers`] instead.
pub(crate) struct Delegation<B: Bus> {
    pub(crate) config: DelegationConfig,
    own: AgentId,
    /// Keeps the IPC endpoint serving; dropping the handle stops it.
    _server: ServerHandle,
    requests: Receiver<ServerRequest>,
    pub(crate) client: Client<Arc<B>>,
    /// Children keyed by local child task id.
    children: BTreeMap<String, Child>,
    /// Idempotency ledger: (parent task, idempotency key) to the child's local id (0015-048).
    ledger: Ledger,
    /// Journaled sends not yet confirmed as `sent`; resent by the next pass (0015-062).
    pending: Vec<Pending>,
    /// Test crash injection: park after the remote accepts a child, before `sent` (0015-062).
    #[cfg(test)]
    pub(crate) park_after_accept: bool,
}

/// A child journaled as `prepared` whose send is not yet journaled as `sent` (0015-062). The
/// recorded message and transport identifier let the send be repeated unchanged.
pub(crate) struct Pending {
    child: String,
    parent: String,
    key: String,
    target: AgentId,
    context: String,
    chain_length: u32,
    deadline: u64,
    message: Outbound,
}

impl Pending {
    /// Reads a `prepared` record. Every field is required; a record without its message or
    /// transport identifier is corrupt.
    fn from_record(record: &serde_json::Value) -> Result<Self, WorkerError> {
        let wire = record.get("message").cloned().ok_or_else(corrupt_journal)?;
        let wire: pb::Message = serde_json::from_value(wire).map_err(|_| corrupt_journal())?;
        let immediate = record
            .get("return_immediately")
            .and_then(serde_json::Value::as_bool)
            .ok_or_else(corrupt_journal)?;
        let transport =
            MessageId::parse(journal_str(record, "transport")?).map_err(|_| corrupt_journal())?;
        Ok(Self {
            child: journal_str(record, "child")?.to_owned(),
            parent: journal_str(record, "parent")?.to_owned(),
            key: journal_str(record, "key")?.to_owned(),
            target: AgentId::parse(journal_str(record, "target")?)
                .map_err(|_| corrupt_journal())?,
            context: journal_str(record, "context")?.to_owned(),
            chain_length: record
                .get("hop")
                .and_then(serde_json::Value::as_u64)
                .and_then(|hop| u32::try_from(hop).ok())
                .ok_or_else(corrupt_journal)?,
            deadline: record
                .get("deadline")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(corrupt_journal)?,
            message: Outbound::from_message(wire, transport, immediate)
                .ok_or_else(corrupt_journal)?,
        })
    }
}

/// Journal state rebuilt by `replay_journal`.
struct Replayed {
    ledger: Ledger,
    children: BTreeMap<String, Child>,
    pending: Vec<Pending>,
}

/// Peer-call state kept on the worker (RFC-0015).
#[derive(Default)]
pub(crate) struct Peers {
    /// Grants authenticating the IPC endpoint.
    pub(crate) grants: Arc<Mutex<GrantBook>>,
    /// Bindings keyed by parent task id.
    pub(crate) bindings: BTreeMap<String, Binding>,
    /// Slot to parent task id.
    pub(crate) slots: BTreeMap<String, String>,
    /// Local child ids whose remote task must be canceled.
    pub(crate) cancels: Vec<String>,
    /// Endpoint paths handed to bound tasks; `None` when no endpoint is attached.
    pub(crate) endpoint: Option<PeerEndpoint>,
    /// Delegation chain of each admitted task: the inbound chain, or this agent's root hop
    /// for a budgetless root (0006-043). Outbound delegation extends it.
    pub(crate) chains: BTreeMap<String, Vec<Hop>>,
}

/// Paths a bound task's shim needs.
#[derive(Clone)]
pub(crate) struct PeerEndpoint {
    pub(crate) endpoint: PathBuf,
    pub(crate) shim_path: PathBuf,
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
        let peers = Peers {
            grants: Arc::new(Mutex::new(GrantBook::new(Arc::clone(&self.clock)))),
            endpoint: Some(PeerEndpoint {
                endpoint: config.endpoint.clone(),
                shim_path: config.shim_path.clone(),
            }),
            ..Peers::default()
        };
        let (sender, requests) = mpsc::channel();
        let server = a2a_mcp::ipc_server::spawn(
            &endpoint,
            Arc::new(Grants(Arc::clone(&peers.grants))),
            sender,
        )
        .map_err(|_| WorkerError::Config("delegation endpoint unavailable"))?;
        let client = Client::new(
            Arc::clone(&self.bus),
            Arc::clone(&self.clock),
            ClientConfig {
                extensions: vec![ext::BUDGET.to_owned(), ext::PROVENANCE.to_owned()],
                ..ClientConfig::default()
            },
            self.config.tenant.clone(),
            self.config.agent.clone(),
        )
        .map_err(|_| WorkerError::Config("delegation client unavailable"))?;
        let Replayed {
            ledger,
            children,
            pending,
        } = self.replay_journal()?;
        // Cancels queued by recovery (0015-063) survive the attach and flush on the next pump.
        let cancels = std::mem::take(&mut self.peers.cancels);
        self.peers = peers;
        self.peers.cancels = cancels;
        self.delegation = Some(Delegation {
            config,
            own: self.config.agent.clone(),
            _server: server,
            requests,
            client,
            ledger,
            children,
            pending,
            #[cfg(test)]
            park_after_accept: false,
        });
        Ok(())
    }

    /// Test crash injection (0015-062): the next child this worker sends is parked after the
    /// remote accepts it and before `sent` is journaled, as if the process stopped there.
    #[cfg(test)]
    pub(crate) fn park_after_accept(&mut self) {
        if let Some(d) = self.delegation.as_mut() {
            d.park_after_accept = true;
        }
    }

    /// Rebuilds the idempotency ledger, the child table and the sends still to make from the
    /// delegation journal (0015-062, 0015-063). A child is restored once its send is journaled as
    /// `sent`. A `prepared` record closed by neither `sent` nor `abandoned` is returned as pending
    /// so its message can be resent with the recorded transport identifier. Restored children are
    /// polled and canceled through the normal delegation path.
    fn replay_journal(&self) -> Result<Replayed, WorkerError> {
        let now = self.clock.now_ms().ok_or(WorkerError::Clock)?;
        let records: Vec<serde_json::Value> = self
            .tasks
            .extensions(JOURNAL_OWNER)
            .into_iter()
            .cloned()
            .collect();
        let mut prepared: BTreeMap<String, Pending> = BTreeMap::new();
        let mut ledger = Ledger::new();
        let mut children = BTreeMap::new();
        for record in records {
            let child = journal_str(&record, "child")?.to_owned();
            match journal_str(&record, "phase")? {
                "prepared" => {
                    prepared.insert(child, Pending::from_record(&record)?);
                }
                "abandoned" => {
                    prepared.remove(&child).ok_or_else(corrupt_journal)?;
                }
                "sent" => {
                    let entry = prepared.remove(&child).ok_or_else(corrupt_journal)?;
                    let remote = TaskId::parse(journal_str(&record, "remote")?)
                        .map_err(|_| corrupt_journal())?;
                    ledger.insert((entry.parent.clone(), entry.key), child.clone());
                    children.insert(
                        child,
                        Child {
                            parent: entry.parent,
                            target: entry.target,
                            remote,
                            context_id: entry.context,
                            chain_length: entry.chain_length,
                            state: TaskState::Submitted,
                            cancel_requested: false,
                            settled: None,
                            waiters: Vec::new(),
                            next_poll: now.saturating_add(CHILD_POLL_MS),
                            deadline: entry.deadline,
                        },
                    );
                }
                _ => return Err(corrupt_journal()),
            }
        }
        Ok(Replayed {
            ledger,
            children,
            pending: prepared.into_values().collect(),
        })
    }

    /// Stops delegation (RFC-0015 0015-074): revokes every grant, answers pending
    /// calls with `GRANT_REVOKED` and closes the endpoint.
    pub fn shutdown_delegation(&mut self) {
        let Some(mut d) = self.delegation.take() else {
            return;
        };
        self.peers.endpoint = None;
        if let Ok(mut book) = self.peers.grants.lock() {
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
        let Some(endpoint) = self.peers.endpoint.clone() else {
            return Ok(None);
        };
        if !self.peers.bindings.contains_key(task) {
            let slot = random_token::<SESSION_SLOT_BYTES>()?;
            let grant_id = random_token::<GRANT_ID_BYTES>()?;
            let credential = random_token::<GRANT_CREDENTIAL_BYTES>()?;
            self.peers
                .grants
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
            self.peers.slots.insert(slot.clone(), task.to_owned());
            self.peers.bindings.insert(
                task.to_owned(),
                Binding {
                    slot,
                    grant_id,
                    credential,
                    calls: Window::default(),
                    sends: Window::default(),
                },
            );
        }
        let binding = self
            .peers
            .bindings
            .get(task)
            .ok_or(WorkerError::Config("peer binding missing"))?;
        Ok(Some(PeerTools {
            slot: binding.slot.clone(),
            grant_id: binding.grant_id.clone(),
            grant_credential: SecretText::new(binding.credential.clone()),
            endpoint: endpoint.endpoint,
            shim_path: endpoint.shim_path,
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
        self.resume_sends(d, now, &mut effects).await?;
        self.flush_cancels(d).await?;
        self.poll_children(d, now, &mut effects).await?;
        expire_waiters(d, now);
        Ok(effects)
    }

    /// Ends bindings whose parent task is terminal or gone (0015-073). Pending calls
    /// are answered `UNAVAILABLE`; unsettled children stay tracked until they settle.
    fn teardown_ended(&mut self, d: &mut Delegation<B>) {
        let ended_chains: Vec<String> = self
            .peers
            .chains
            .keys()
            .filter(|task| !self.parent_open(task))
            .cloned()
            .collect();
        for task in ended_chains {
            self.peers.chains.remove(&task);
        }
        let ended: Vec<String> = self
            .peers
            .bindings
            .keys()
            .filter(|task| !self.parent_open(task))
            .cloned()
            .collect();
        for task in ended {
            let Some(binding) = self.peers.bindings.remove(&task) else {
                continue;
            };
            self.peers.slots.remove(&binding.slot);
            if let Ok(mut book) = self.peers.grants.lock() {
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
        let Some(task) = self.peers.slots.get(&request.slot).cloned() else {
            respond_err(request, IpcError::new(reason::SLOT_UNBOUND));
            return Ok(());
        };
        if !matches!(frame, ClientFrame::CancelCall { .. }) {
            let max = d.config.max_calls_per_min;
            let admitted = self
                .peers
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
                self.discover(d, request, &task, &params).await?;
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

    /// Sends queued cancels of children whose parents asked for them (0015-069).
    async fn flush_cancels(&mut self, d: &mut Delegation<B>) -> Result<(), WorkerError> {
        for child_id in std::mem::take(&mut self.peers.cancels) {
            let Some(child) = d.children.get(&child_id) else {
                continue;
            };
            if child.settled.is_some() {
                continue;
            }
            let (target, remote) = (child.target.clone(), child.remote.clone());
            let _ = self
                .await_peer(
                    d.client
                        .cancel_task(&target, &remote, &CallOptions::default()),
                )
                .await?;
        }
        Ok(())
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
            let polled = self
                .await_peer(d.client.get_task(&target, &remote, &CallOptions::default()))
                .await?;
            match polled {
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

    /// Whether `parent` has a delegated child that has not settled (0015-067).
    pub(crate) fn has_open_children(&self, parent: &str) -> bool {
        self.delegation.as_ref().is_some_and(|d| {
            d.children
                .values()
                .any(|c| c.parent == parent && c.settled.is_none())
        })
    }

    /// Commits a deferred COMPLETED once `parent` has no open child left (0015-067). A parent that
    /// already ended, for example through a child failure (0015-070), drops the deferred end.
    pub(crate) fn finish_deferred(
        &mut self,
        d: &Delegation<B>,
        parent: &str,
        now: u64,
        effects: &mut Vec<Effect>,
    ) -> Result<(), WorkerError> {
        let open = d
            .children
            .values()
            .any(|c| c.parent == parent && c.settled.is_none());
        if open || !self.deferred_ends.contains_key(parent) {
            return Ok(());
        }
        let Some(reason) = self.deferred_ends.remove(parent) else {
            return Ok(());
        };
        match self.commit_turn_end(parent, reason, now) {
            Ok(new_effects) => effects.extend(new_effects),
            Err(WorkerError::Tasks(
                TaskError::NotActive | TaskError::Terminal | TaskError::NotFound,
            )) => {}
            Err(err) => return Err(err),
        }
        effects.extend(self.dispatch_next(now)?);
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
            .and_then(|m| {
                a2a_types::failure::FailureReason::from_message(m)
                    .ok()
                    .flatten()
            })
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
                usage: result::usage(task).unwrap_or_default(),
                cancelled_by_parent,
            },
            now,
        ) {
            Ok(new_effects) => effects.extend(new_effects),
            Err(TaskError::NotActive | TaskError::Terminal | TaskError::NotFound) => {}
            Err(err) => return Err(WorkerError::Tasks(err)),
        }
        let chain_length = d.children.get(child_id).map_or(0, |c| c.chain_length);
        if let Some(child) = d.children.get_mut(child_id) {
            for waiter in child.waiters.drain(..) {
                respond_outcome(waiter.request, &settled);
            }
            child.settled = Some(settled);
        }
        self.finish_deferred(d, &parent, now, effects)?;
        self.audit(
            Some(&parent),
            format!(
                "DELEGATION_CHILD_SETTLED {} child={child_id} chain_length={chain_length}",
                result::state_name(state)
            ),
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

fn expire_waiters<B: Bus>(d: &mut Delegation<B>, now: u64) {
    for (child_id, child) in &mut d.children {
        let (due, rest): (Vec<Waiter>, Vec<Waiter>) =
            child.waiters.drain(..).partition(|w| w.deadline <= now);
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
    getrandom::fill(&mut bytes).map_err(|_| WorkerError::Config("random source unavailable"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    /// Clock the test moves by hand.
    struct ManualClock(AtomicU64);

    impl Clock for ManualClock {
        fn now_ms(&self) -> Option<u64> {
            Some(self.0.load(Ordering::SeqCst))
        }
    }

    const ID: &str = "grant-1";
    const CREDENTIAL: &str = "correct-credential";
    const SLOT: &str = "slot-1";

    fn table(clock: Arc<ManualClock>) -> Grants {
        let mut book = GrantBook::new(clock);
        book.grants.insert(
            ID.to_owned(),
            Grant {
                credential: CREDENTIAL.to_owned(),
                slot: SLOT.to_owned(),
                live: true,
            },
        );
        Grants(Arc::new(Mutex::new(book)))
    }

    fn auth(grants: &Grants, credential: &str) -> Result<(), IpcError> {
        grants.authenticate(ID, &Secret::new(credential), SLOT)
    }

    fn code(result: Result<(), IpcError>) -> String {
        match result {
            Ok(()) => "ok".to_owned(),
            Err(err) => err.reason,
        }
    }

    /// RFC-0015 0015-043: five failed authentications, across connections, lock out every
    /// authentication for 60 s; the window of failures is measured on the injected clock.
    #[test]
    fn req_0015_043_five_failures_lock_out_for_sixty_seconds() {
        let clock = Arc::new(ManualClock(AtomicU64::new(1_000)));
        let grants = table(Arc::clone(&clock));
        for _ in 0..5 {
            assert_eq!(code(auth(&grants, "wrong")), reason::UNAUTHENTICATED);
        }
        clock.0.store(30_000, Ordering::SeqCst);
        assert_eq!(
            code(auth(&grants, CREDENTIAL)),
            reason::UNAUTHENTICATED,
            "correct credential is refused during the lockout"
        );
        clock.0.store(61_000 - 1, Ordering::SeqCst);
        assert_eq!(code(auth(&grants, CREDENTIAL)), reason::UNAUTHENTICATED);
        clock.0.store(61_000, Ordering::SeqCst);
        assert_eq!(code(auth(&grants, CREDENTIAL)), "ok");
    }

    /// RFC-0015 0015-043: failures older than the window do not count toward a lockout.
    #[test]
    fn req_0015_043_old_failures_do_not_lock_out() {
        let clock = Arc::new(ManualClock(AtomicU64::new(0)));
        let grants = table(Arc::clone(&clock));
        for _ in 0..4 {
            assert_eq!(code(auth(&grants, "wrong")), reason::UNAUTHENTICATED);
        }
        clock.0.store(60_000, Ordering::SeqCst);
        assert_eq!(code(auth(&grants, "wrong")), reason::UNAUTHENTICATED);
        assert_eq!(code(auth(&grants, CREDENTIAL)), "ok");
    }
}
