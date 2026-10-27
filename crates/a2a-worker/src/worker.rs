//! Worker runtime: backend dispatch, approvals, outbox and request serving.
//!
//! The runtime is agnostic to the async executor. It awaits only bus futures.
//! The host drives time by calling [`Worker::pump`] (or [`Worker::serve_one`],
//! which pumps after each request) no later than [`Worker::next_wakeup`].

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::{Future, poll_fn};
use std::pin::pin;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;

use a2a_core::approval::{ALLOW_ONCE_ID, NewRequest, Opened, Tier};
use a2a_core::envelope::{self, Clock, EventParts, Outgoing, Rejection};
use a2a_core::task::{
    ApprovalAnswer, ArtifactChunk, Effect, Event, EventBody, StopReason, TaskError, TaskStore,
    Tasks,
};
use a2a_types::failure::FailureCode;
use a2a_types::ids::{MessageId, TaskId};
use a2a_types::proto::a2a as pb;
use a2a_types::proto::a2a::TaskState;
use bus_api::bus::{Bus, RequestStream};
use bus_api::error::BusError;
use bus_api::name::SubjectPattern;

use crate::audit::AuditRecorder;
use crate::backend::{
    Backend, BackendError, BackendEvent, ChannelSink, FaultKind, PermissionAnswer,
    PermissionOption, PermissionRequest, RequestId, StartTurn, TurnRef,
};
use crate::config::{ApprovalMode, WorkerConfig};
use crate::error::WorkerError;
use crate::sanitize::{frame_inputs, redact_outbound};
use crate::wire;
use crate::workspace::WorkspaceHook;

/// What woke [`Worker::await_peer`]: the outbound call finished, or a bus request arrived.
enum Turn<T> {
    Done(T),
    Request(Result<bus_api::message::Request, BusError>),
}

/// A note the host should log or persist. Drained with [`Worker::take_audit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditNote {
    /// Task the note concerns, when there is one.
    pub task_id: Option<String>,
    /// Stable reason token followed by optional detail.
    pub reason: String,
}

/// Input held for a SUBMITTED task until its backend turn starts. Not persisted.
#[derive(Debug)]
pub(crate) struct PendingTurn {
    pub(crate) context_id: String,
    pub(crate) input: Vec<crate::backend::InputPart>,
    pub(crate) deadline_ms: u64,
    pub(crate) origin: String,
}

/// A backend permission request awaiting an approval decision.
#[derive(Debug)]
pub(crate) struct Offered {
    pub(crate) backend_id: String,
    pub(crate) options: Vec<PermissionOption>,
}

/// One agent's worker: serves its RPC subjects and runs its tasks through a backend.
pub struct Worker<B: Bus, D: Backend, S: TaskStore> {
    pub(crate) config: WorkerConfig,
    pub(crate) bus: Arc<B>,
    pub(crate) requests: B::Requests,
    pub(crate) backend: D,
    pub(crate) events: Receiver<BackendEvent>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) tasks: Tasks<S>,
    pub(crate) pending: BTreeMap<String, PendingTurn>,
    /// Tasks admitted to the backend but waiting for their context to free (FIFO, 0005-054).
    pub(crate) queue: VecDeque<String>,
    /// Open turns: task id to the last `event_no` accepted for the turn.
    pub(crate) open: BTreeMap<String, u64>,
    /// Kill deadlines for turns that were asked to cancel.
    pub(crate) cancel_deadlines: BTreeMap<String, u64>,
    /// Completed turns whose end waits for open delegated children (0015-067).
    pub(crate) deferred_ends: BTreeMap<String, StopReason>,
    /// Blocking sends awaiting their reply (0002-050).
    pub(crate) held: Vec<crate::rpc::Held>,
    /// Backend permission requests keyed by the approval request id that answers them.
    pub(crate) permissions: BTreeMap<String, Vec<Offered>>,
    pub(crate) restarts_seen: u32,
    /// Whether the backend was live at the last health check (a start is not a loss).
    pub(crate) live_seen: bool,
    /// `(task, artifact)` pairs already published; the next chunk is an append.
    pub(crate) artifacts: BTreeSet<(String, String)>,
    pub(crate) audit: Vec<AuditNote>,
    /// Sink for mapped audit records; `None` until the host attaches one.
    pub(crate) audit_sink: Option<Arc<Mutex<dyn AuditRecorder>>>,
    /// Records lost to build or emit failures ([0013-015]).
    pub(crate) audit_failures: u64,
    /// Workspace lease hook; `None` when no workspace binding is configured.
    pub(crate) workspace: Option<WorkspaceHook>,
    /// Card and presence publisher; `None` when no publication is configured (RFC-0004).
    pub(crate) presence: Option<crate::presence::Publisher>,
    /// Peer-call delegation (RFC-0015); `None` when no delegation table is attached.
    pub(crate) delegation: Option<crate::delegation::Delegation<B>>,
    /// Peer-call state (RFC-0015) kept on the worker itself, so bus requests served while
    /// a delegation pump awaits a peer can create bindings and queue child cancels.
    pub(crate) peers: crate::delegation::Peers,
    /// Nonces of verified operator decisions, refused on replay ([0007-041]). Used only when
    /// signing is enabled ([0010-079]).
    pub(crate) replay: a2a_security::replay::ReplayCache,
}

impl<B: Bus, D: Backend, S: TaskStore> std::fmt::Debug for Worker<B, D, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Worker")
            .field("agent", &self.config.agent)
            .field("tenant", &self.config.tenant)
            .field("open_turns", &self.open.len())
            .field("pending", &self.pending.len())
            .finish_non_exhaustive()
    }
}

impl<B: Bus, D: Backend, S: TaskStore> Worker<B, D, S> {
    /// Opens the worker: attaches the backend sink, serves the RPC subjects,
    /// and recovers the task store. Recovery effects are applied before return.
    pub async fn open(
        config: WorkerConfig,
        bus: B,
        mut backend: D,
        store: S,
        clock: Box<dyn Clock>,
    ) -> Result<Self, WorkerError> {
        if config.max_active == 0 {
            return Err(WorkerError::Config("max_active must be at least 1"));
        }
        if config.max_waiting_per_context == 0 {
            return Err(WorkerError::Config(
                "max_waiting_per_context must be at least 1",
            ));
        }
        let now = clock.now_ms().ok_or(WorkerError::Clock)?;
        let workspace = WorkspaceHook::for_config(&config)?;
        if config.working_directory.as_os_str().is_empty() {
            return Err(WorkerError::Config("working directory not configured"));
        }
        let (sink, events) = ChannelSink::channel();
        backend.attach(Box::new(sink));
        let pattern = rpc_pattern(&config)?;
        let requests = bus.serve(&config.principal, &pattern).await?;
        let (tasks, effects) = Tasks::open(store, config.tasks.clone(), now)?;
        let initial = backend.health();
        let presence = config
            .publication
            .as_ref()
            .map(|p| crate::presence::Publisher::new(&config.tenant, &config.agent, p, now))
            .transpose()?;
        let mut worker = Self {
            config,
            bus: Arc::new(bus),
            requests,
            backend,
            events,
            clock: Arc::from(clock),
            tasks,
            pending: BTreeMap::new(),
            queue: VecDeque::new(),
            held: Vec::new(),
            open: BTreeMap::new(),
            deferred_ends: BTreeMap::new(),
            cancel_deadlines: BTreeMap::new(),
            permissions: BTreeMap::new(),
            restarts_seen: initial.restarts,
            live_seen: initial.live,
            artifacts: BTreeSet::new(),
            audit: Vec::new(),
            audit_sink: None,
            audit_failures: 0,
            workspace,
            presence,
            delegation: None,
            peers: crate::delegation::Peers::default(),
            replay: a2a_security::replay::ReplayCache::new(crate::signing::REPLAY_CAPACITY),
        };
        worker.run(effects, now)?;
        worker.publish_presence(now, true).await?;
        Ok(worker)
    }

    /// Awaits one inbound request, answers it, then pumps.
    pub async fn serve_one(&mut self) -> Result<(), WorkerError> {
        let request = self.requests.next().await?;
        self.serve_request(request).await?;
        self.pump().await
    }

    /// Answers one inbound bus request.
    async fn serve_request(
        &mut self,
        request: bus_api::message::Request,
    ) -> Result<(), WorkerError> {
        let signed = self
            .config
            .signing
            .is_some()
            .then(|| crate::signing::Received::of(&request));
        match envelope::receive(&self.config.wire, self.clock.as_ref(), request) {
            Err(Rejection::Dropped) => Ok(()),
            Err(Rejection::Answer(answer)) => Ok(answer.send()?),
            Ok(inbound) => {
                let now = self.now()?;
                self.handle(inbound, now, signed).await
            }
        }
    }

    /// Awaits an outbound peer call while still serving bus requests (RFC-0015). A worker
    /// waiting on a peer keeps answering that peer, so two workers calling each other
    /// cannot deadlock.
    pub(crate) async fn await_peer<T>(
        &mut self,
        call: impl Future<Output = T>,
    ) -> Result<T, WorkerError> {
        let mut call = pin!(call);
        loop {
            let turn = poll_fn(|cx| {
                if let Poll::Ready(value) = call.as_mut().poll(cx) {
                    return Poll::Ready(Turn::Done(value));
                }
                match pin!(self.requests.next()).poll(cx) {
                    Poll::Ready(request) => Poll::Ready(Turn::Request(request)),
                    Poll::Pending => Poll::Pending,
                }
            })
            .await;
            match turn {
                Turn::Done(value) => return Ok(value),
                Turn::Request(request) => self.serve_request(request?).await?,
            }
        }
    }

    /// Advances the worker at the current time: backend events, backend health,
    /// cancel kills, deadlines and approval expiry, then the outbox.
    pub async fn pump(&mut self) -> Result<(), WorkerError> {
        let now = self.now()?;
        // Expiry is authoritative at its instant: it runs before backend output of the same
        // instant, so a driver-side timeout or crash delivered with it cannot win (0006-017).
        let effects = self.tasks.expire_due(now)?;
        let effects = self.expire(effects, now)?;
        self.run(effects, now)?;
        while let Ok(event) = self.events.try_recv() {
            let effects = self.on_backend_event(event, now)?;
            self.run(effects, now)?;
        }
        let effects = self.check_health(now)?;
        self.run(effects, now)?;
        self.workspace_maintenance(now);
        let due: Vec<String> = self
            .cancel_deadlines
            .iter()
            .filter(|(_, at)| **at <= now)
            .map(|(task, _)| task.clone())
            .collect();
        for task in due {
            let effects = self.kill(&task, StopReason::Cancelled, None, now)?;
            self.run(effects, now)?;
        }
        self.release_held(now).await?;
        self.pump_delegation(now).await?;
        self.flush_outbox(now).await?;
        self.publish_presence(now, false).await
    }

    /// Publishes the agent card and presence (RFC-0004). Strict callers (`open`) propagate bus
    /// failures; `pump` records them and retries on a later pump.
    async fn publish_presence(&mut self, now: u64, strict: bool) -> Result<(), WorkerError> {
        let Some(mut publisher) = self.presence.take() else {
            return Ok(());
        };
        let healthy = self.backend.health().live;
        let active = self.tasks.active_count();
        let result = publisher
            .tick(
                &self.bus,
                &self.config.principal,
                now,
                healthy,
                active,
                strict,
            )
            .await;
        self.presence = Some(publisher);
        for note in result? {
            self.audit(None, note);
        }
        Ok(())
    }

    /// Earliest absolute time (ms) at which [`Worker::pump`] must run again, if any.
    #[must_use]
    pub fn next_wakeup(&self, now: u64) -> Option<u64> {
        let cancel = self.cancel_deadlines.values().min().copied();
        let presence = self
            .presence
            .as_ref()
            .and_then(|p| p.next_wakeup(now, self.backend.health().live));
        let delegation = self.delegation.as_ref().map(|d| d.wakeup(now));
        let held = self.held.iter().map(|entry| entry.wait.until).min();
        [
            self.tasks.next_wakeup(now),
            cancel,
            presence,
            delegation,
            held,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Sets draining. New submissions answer DRAINING; in-flight tasks continue (0005-050).
    pub fn set_draining(&mut self, draining: bool) {
        self.tasks.set_draining(draining);
        if let Some(p) = self.presence.as_mut() {
            p.set_draining(draining);
        }
        self.set_workspace_draining(draining);
    }

    /// Returns and clears the pending audit notes.
    pub fn take_audit(&mut self) -> Vec<AuditNote> {
        std::mem::take(&mut self.audit)
    }

    /// Returns the configuration in effect.
    #[must_use]
    pub fn config(&self) -> &WorkerConfig {
        &self.config
    }

    pub(crate) fn now(&self) -> Result<u64, WorkerError> {
        self.clock.now_ms().ok_or(WorkerError::Clock)
    }

    pub(crate) fn audit(&mut self, task_id: Option<&str>, reason: impl Into<String>) {
        let note = AuditNote {
            task_id: task_id.map(str::to_owned),
            reason: reason.into(),
        };
        self.audit.push(note.clone());
        self.record_or_count(&note);
    }

    pub(crate) async fn publish(&self, outgoing: Outgoing) -> Result<(), BusError> {
        let Outgoing {
            subject,
            headers,
            payload,
        } = outgoing;
        self.bus
            .publish(&self.config.principal, &subject, &headers, payload)
            .await
            .map(|_| ())
    }

    /// Encodes one task event for the ev subject. Failure means the event cannot be
    /// represented on the wire (for example an artifact above `max_inline`).
    pub(crate) fn encode(
        &self,
        task_id: &str,
        msg_id: &str,
        stream_end: bool,
        event: pb::StreamResponse,
    ) -> Result<Outgoing, WorkerError> {
        let task = TaskId::parse(task_id).map_err(|_| WorkerError::Invalid("task id"))?;
        let msg_id = MessageId::parse(msg_id).map_err(|_| WorkerError::Invalid("message id"))?;
        envelope::encode_event(
            EventParts {
                tenant: self.config.tenant.clone(),
                agent: self.config.agent.clone(),
                task,
                msg_id,
                traceparent: None,
                tracestate: None,
                stream_end,
                json: false,
                event,
            },
            self.config.max_inline,
        )
        .map_err(|_| WorkerError::Invalid("event not encodable"))
    }

    /// Applies effects, including the effects that application produces.
    pub(crate) fn run(&mut self, effects: Vec<Effect>, now: u64) -> Result<(), WorkerError> {
        let mut work = VecDeque::from(effects);
        while let Some(effect) = work.pop_front() {
            let more = self.apply(effect, now)?;
            work.extend(more);
        }
        Ok(())
    }

    fn apply(&mut self, effect: Effect, now: u64) -> Result<Vec<Effect>, WorkerError> {
        match effect {
            Effect::StartBackend { task_id } => {
                self.queue.push_back(task_id);
                self.dispatch_next(now)
            }
            Effect::CancelBackend { task_id, grace_ms } => {
                self.cancel_turn(&task_id, grace_ms, now)
            }
            Effect::KillBackend { task_id } => {
                self.kill(&task_id, StopReason::Cancelled, None, now)
            }
            Effect::AnswerApproval {
                request_id, answer, ..
            } => {
                self.answer_permissions(&request_id, answer);
                Ok(Vec::new())
            }
            Effect::CancelChild { child_id, .. } => {
                self.peers.cancels.push(child_id);
                Ok(Vec::new())
            }
            Effect::Audit { task_id, reason } => {
                self.audit(task_id.as_deref(), reason);
                Ok(Vec::new())
            }
        }
    }

    /// Tasks of (origin, context_id) waiting in the queue for an open turn. Admission uses
    /// this count for the QUEUE_FULL quota (0005-028).
    pub(crate) fn waiting_in_context(&self, origin: &str, context_id: &str) -> usize {
        self.queue
            .iter()
            .filter(|task| {
                self.tasks
                    .task_owner(task)
                    .is_some_and(|owner| owner.origin == origin && owner.context_id == context_id)
            })
            .count()
    }

    /// Starts queued tasks whose context has no open turn (0005-054). Turns of one
    /// (origin, context_id) run one at a time, in admission order; turns of different contexts
    /// run concurrently (0009-020).
    pub(crate) fn dispatch_next(&mut self, now: u64) -> Result<Vec<Effect>, WorkerError> {
        let mut out = Vec::new();
        while self.open.len() < self.config.max_active {
            let Some(pos) = self.queue.iter().position(|task| !self.context_held(task)) else {
                break;
            };
            let Some(task) = self.queue.remove(pos) else {
                break;
            };
            out.extend(self.start_turn(&task, now)?);
        }
        Ok(out)
    }

    /// Whether an open turn holds the (origin, context_id) of `task`. A task with no record
    /// holds nothing and is left to `start_turn`, which ends it.
    fn context_held(&self, task: &str) -> bool {
        let Some(lane) = self.tasks.task_owner(task) else {
            return false;
        };
        self.open.keys().any(|holder| {
            self.tasks.task_owner(holder).is_some_and(|owner| {
                owner.origin == lane.origin && owner.context_id == lane.context_id
            })
        })
    }

    fn start_turn(&mut self, task: &str, now: u64) -> Result<Vec<Effect>, WorkerError> {
        if let Some(context) = self.pending.get(task).map(|turn| turn.context_id.clone()) {
            if let Err(token) = self.lease_for_turn(task, &context) {
                self.pending.remove(task);
                return self.fail_workspace(task, token, now);
            }
        }
        let mut out = match self.tasks.start_backend(task, now) {
            Ok(effects) => effects,
            Err(TaskError::NotStartable | TaskError::NotActive | TaskError::Terminal) => {
                // The task ended while queued (cancel or deadline); nothing to start.
                self.release_task(task);
                self.pending.remove(task);
                return Ok(Vec::new());
            }
            Err(err) => {
                self.release_task(task);
                return Err(err.into());
            }
        };
        let Some(pending) = self.pending.remove(task) else {
            // Recovered SUBMITTED task whose input was never persisted (see AGENTS.md).
            let detail = cause("RECOVERED", "task recovered without its persisted input");
            out.extend(self.tasks.backend_stopped_with_detail(
                task,
                StopReason::Crashed,
                Some(detail),
                now,
            )?);
            return Ok(out);
        };
        let peer_tools = self.bind_peer_tools(task)?;
        let input = match frame_inputs(pending.input, &pending.origin) {
            Ok(input) => input,
            Err(err) => {
                self.audit(Some(task), format!("UNTRUSTED_INPUT_REJECTED: {err}"));
                let detail = cause("UNTRUSTED_INPUT_REJECTED", &err.to_string());
                out.extend(self.commit_turn_end_with_detail(
                    task,
                    StopReason::Unknown,
                    Some(detail),
                    now,
                )?);
                return Ok(out);
            }
        };
        let cmd = StartTurn {
            turn: TurnRef::new(task),
            context_id: pending.context_id,
            input,
            deadline_ms: pending.deadline_ms,
            session: None,
            origin: pending.origin,
            peer_tools,
        };
        match self.backend.start(cmd) {
            Ok(()) => {
                self.open.insert(task.to_owned(), 0);
            }
            Err(err) => {
                self.audit(Some(task), format!("BACKEND_START_FAILED: {err}"));
                let detail = cause("BACKEND_START_FAILED", &err.to_string());
                out.extend(self.commit_turn_end_with_detail(
                    task,
                    start_failure(&err),
                    Some(detail),
                    now,
                )?);
            }
        }
        Ok(out)
    }

    /// Ends an open turn with `reason` and `detail`, then starts the next queued task.
    fn close_turn_with_detail(
        &mut self,
        task: &str,
        reason: StopReason,
        detail: Option<serde_json::Value>,
        now: u64,
    ) -> Result<Vec<Effect>, WorkerError> {
        self.open.remove(task);
        self.cancel_deadlines.remove(task);
        let mut out = self.commit_turn_end_with_detail(task, reason, detail, now)?;
        out.extend(self.dispatch_next(now)?);
        Ok(out)
    }

    fn cancel_turn(
        &mut self,
        task: &str,
        grace_ms: u64,
        now: u64,
    ) -> Result<Vec<Effect>, WorkerError> {
        if !self.open.contains_key(task) {
            return Ok(Vec::new());
        }
        if grace_ms == 0 {
            return self.kill(task, StopReason::Cancelled, None, now);
        }
        match self.backend.cancel(&TurnRef::new(task)) {
            Ok(()) => {
                self.cancel_deadlines
                    .entry(task.to_owned())
                    .or_insert(now.saturating_add(grace_ms));
                Ok(Vec::new())
            }
            Err(BackendError::Closed) => {
                let detail = cause("BACKEND_CLOSED", "backend closed during cancel");
                self.close_turn_with_detail(task, StopReason::Crashed, Some(detail), now)
            }
            Err(err) => {
                // The turn is still open; the kill deadline bounds how long it may run.
                self.audit(Some(task), format!("CANCEL_FAILED: {err}"));
                self.cancel_deadlines
                    .entry(task.to_owned())
                    .or_insert(now.saturating_add(grace_ms));
                Ok(Vec::new())
            }
        }
    }

    /// Kills the backend and ends the open turn with `reason`. The next start on this
    /// backend spawns the agent again (0009-036).
    fn kill(
        &mut self,
        task: &str,
        reason: StopReason,
        detail: Option<serde_json::Value>,
        now: u64,
    ) -> Result<Vec<Effect>, WorkerError> {
        if !self.open.contains_key(task) {
            return Ok(Vec::new());
        }
        self.stop_session(task);
        self.close_turn_with_detail(task, reason, detail, now)
    }

    /// Stops the open session for `task` without recording an end state. The caller commits it.
    fn stop_session(&mut self, task: &str) {
        if self.open.remove(task).is_none() {
            return;
        }
        self.cancel_deadlines.remove(task);
        // The worker stopped this backend itself. Clearing `live_seen` keeps the next health
        // check from reading the stop as a live backend dropping under a later turn (0006-017).
        self.live_seen = false;
        if let Err(err) = self.backend.shutdown(Duration::ZERO) {
            self.audit(Some(task), format!("SHUTDOWN_FAILED: {err}"));
        }
    }

    /// Answers every open permission request with `cancelled` before the session is killed, so
    /// the backend never waits on an answer the worker will not deliver (0006-017).
    fn cancel_pending_permissions(&mut self) {
        let pending = std::mem::take(&mut self.permissions);
        for (approval_id, offers) in pending {
            for offered in offers {
                if let Err(err) = self.backend.resolve_permission(
                    &RequestId::new(&offered.backend_id),
                    PermissionAnswer::Cancelled,
                ) {
                    self.audit(
                        None,
                        format!("RESOLVE_PERMISSION_FAILED {approval_id}: {err}"),
                    );
                }
            }
        }
    }

    /// Applies `expire_due` output. `KillBackend` is kill-then-commit (0005-037, 0005-041): the
    /// session stops first, then `commit_expired` records the end state. Queued work then starts.
    fn expire(&mut self, effects: Vec<Effect>, now: u64) -> Result<Vec<Effect>, WorkerError> {
        let mut out = Vec::with_capacity(effects.len());
        let mut killed = false;
        for effect in effects {
            match effect {
                Effect::KillBackend { task_id } => {
                    self.cancel_pending_permissions();
                    self.stop_session(&task_id);
                    self.publish_before_kill(&task_id, now)?;
                    out.extend(self.tasks.commit_expired(&task_id, now)?);
                    self.release_task(&task_id);
                    killed = true;
                }
                other => out.push(other),
            }
        }
        if killed {
            out.extend(self.dispatch_next(now)?);
        }
        Ok(out)
    }

    fn check_health(&mut self, now: u64) -> Result<Vec<Effect>, WorkerError> {
        let health = self.backend.health();
        let restarted = health.restarts != self.restarts_seen;
        let was_live = std::mem::replace(&mut self.live_seen, health.live);
        self.restarts_seen = health.restarts;
        // A backend that is still starting has no turn to lose: its turns wait for it. Only a
        // live backend that stops, or a restart, drops the open turns.
        if health.live && !restarted {
            return Ok(Vec::new());
        }
        if !health.live && !was_live && !restarted {
            return Ok(Vec::new());
        }
        let text = match (health.live, health.reason.as_deref()) {
            (false, Some(reason)) => format!("backend not live: {reason}"),
            (false, None) => "backend not live".to_owned(),
            (true, _) => "backend restarted; the open turn was lost".to_owned(),
        };
        let detail = cause("BACKEND_NOT_LIVE", &text);
        let open: Vec<String> = self.open.keys().cloned().collect();
        let mut out = Vec::new();
        for task in open {
            out.extend(self.close_turn_with_detail(
                &task,
                StopReason::Crashed,
                Some(detail.clone()),
                now,
            )?);
        }
        Ok(out)
    }

    fn on_backend_event(
        &mut self,
        event: BackendEvent,
        now: u64,
    ) -> Result<Vec<Effect>, WorkerError> {
        match event {
            BackendEvent::Fault { kind, detail } => self.on_fault(kind, &detail, now),
            turn_event => self.on_turn_event(turn_event, now),
        }
    }

    fn on_fault(
        &mut self,
        kind: FaultKind,
        detail: &str,
        now: u64,
    ) -> Result<Vec<Effect>, WorkerError> {
        self.audit(None, format!("BACKEND_FAULT {kind:?}: {detail}"));
        let reason = match kind {
            FaultKind::StreamEnded => StopReason::Crashed,
            FaultKind::ConfigRefused | FaultKind::UnsupportedProtocolVersion => {
                StopReason::Backend(FailureCode::BackendUnavailable)
            }
            FaultKind::Internal => StopReason::Unknown,
        };
        let open: Vec<String> = self.open.keys().cloned().collect();
        let carried = cause(&format!("{kind:?}"), detail);
        let mut out = Vec::new();
        for task in open {
            out.extend(self.close_turn_with_detail(&task, reason, Some(carried.clone()), now)?);
        }
        Ok(out)
    }

    fn on_turn_event(&mut self, event: BackendEvent, now: u64) -> Result<Vec<Effect>, WorkerError> {
        let Some(turn) = event.turn().map(|turn| turn.as_str().to_owned()) else {
            return Ok(Vec::new());
        };
        let event_no = event.event_no().unwrap_or(0);
        let Some(prev) = self.open.get(&turn).copied() else {
            self.audit(Some(&turn), "LATE_BACKEND_OUTPUT: turn is not open");
            return Ok(Vec::new());
        };
        if event_no <= prev {
            self.audit(Some(&turn), "DUPLICATE_BACKEND_EVENT");
            return Ok(Vec::new());
        }
        if event_no != prev + 1 {
            self.audit(Some(&turn), "BACKEND_EVENT_GAP: turn killed");
            let detail = cause("BACKEND_EVENT_GAP", "backend event sequence has a gap");
            return self.kill(&turn, StopReason::Unknown, Some(detail), now);
        }
        self.open.insert(turn.clone(), event_no);
        match event {
            // Progress does not change task state; it is not published (see AGENTS.md).
            BackendEvent::Status { .. } => Ok(Vec::new()),
            BackendEvent::ArtifactDelta {
                artifact_key,
                text,
                append,
                last,
                ..
            } => match self.tasks.append_artifact(
                &turn,
                ArtifactChunk {
                    artifact_id: artifact_key.clone(),
                    name: artifact_key,
                    media_type: "text/plain".to_owned(),
                    append,
                    last_chunk: last,
                    payload: redact_outbound(&text)?.into_bytes(),
                },
                now,
            ) {
                Ok(()) => Ok(Vec::new()),
                Err(TaskError::Terminal | TaskError::NotActive) => {
                    self.audit(Some(&turn), "LATE_ARTIFACT_DELTA");
                    Ok(Vec::new())
                }
                Err(err) => Err(err.into()),
            },
            BackendEvent::Usage { sample, .. } => {
                let tokens = sample
                    .tokens_in
                    .unwrap_or(0)
                    .saturating_add(sample.tokens_out.unwrap_or(0));
                let cost = sample.cost.map(|cost| cost.units);
                match self.tasks.charge(&turn, tokens, cost, now) {
                    Ok(effects) => Ok(effects),
                    Err(TaskError::Terminal | TaskError::NotActive) => {
                        self.audit(Some(&turn), "LATE_USAGE_SAMPLE");
                        Ok(Vec::new())
                    }
                    Err(err) => Err(err.into()),
                }
            }
            BackendEvent::PermissionRequest { request, .. } => {
                self.on_permission(&turn, request, now)
            }
            BackendEvent::InputRequired { .. } => {
                // The peer interrupted the turn and needs input; this is not an approval (0009-047).
                // The turn ends here, so the session is not shut down (0005-008, 0002-044).
                self.open.remove(&turn);
                self.cancel_deadlines.remove(&turn);
                let mut out = self.tasks.backend_input_required(&turn, now)?;
                out.extend(self.dispatch_next(now)?);
                Ok(out)
            }
            BackendEvent::Terminal { outcome, .. } => {
                let reason = wire::stop_reason(&outcome);
                // 0015-067: a completed parent waits for its open children before COMPLETED.
                if reason == StopReason::EndTurn && self.has_open_children(&turn) {
                    self.open.remove(&turn);
                    self.cancel_deadlines.remove(&turn);
                    self.deferred_ends.insert(turn, reason);
                    return Ok(Vec::new());
                }
                let detail = wire::outcome_detail(&outcome);
                self.close_turn_with_detail(&turn, reason, detail, now)
            }
            BackendEvent::Fault { .. } => Ok(Vec::new()),
        }
    }

    fn on_permission(
        &mut self,
        turn: &str,
        request: PermissionRequest,
        now: u64,
    ) -> Result<Vec<Effect>, WorkerError> {
        let tier = tier_of(&request.tool_kind);
        let backend_id = request.request_id.as_str().to_owned();
        if self.config.approval == ApprovalMode::AskExec && matches!(tier, Tier::Read | Tier::Write)
        {
            self.resolve_backend(&backend_id, &request.options, Some(ALLOW_ONCE_ID));
            return Ok(Vec::new());
        }
        let title = redact_outbound(&request.tool_title)?;
        let (working_directory, workspace_mode) = self.working_context(turn);
        let new = NewRequest {
            request_id: backend_id.clone(),
            task_id: turn.to_owned(),
            tool: title.clone(),
            tier,
            summary: title,
            requester: self.config.agent.as_str().to_owned(),
            chain: vec![self.config.agent.as_str().to_owned()],
            nonce: MessageId::generate().as_str().as_bytes().to_vec(),
            expiry_ms: None,
            working_directory,
            workspace_mode,
        };
        let (opened, effects) = match self.tasks.request_approval(turn, new, now) {
            Ok(result) => result,
            Err(TaskError::NotActive | TaskError::Terminal) => {
                self.resolve_backend(&backend_id, &request.options, None);
                return Ok(Vec::new());
            }
            Err(err) => return Err(err.into()),
        };
        match opened {
            Opened::Created { .. } => self.park(backend_id.clone(), backend_id, request.options),
            Opened::Joined { request_id } => self.park(request_id, backend_id, request.options),
            Opened::Flood => self.resolve_backend(&backend_id, &request.options, None),
        }
        Ok(effects)
    }

    fn park(&mut self, approval_id: String, backend_id: String, options: Vec<PermissionOption>) {
        self.permissions
            .entry(approval_id)
            .or_default()
            .push(Offered {
                backend_id,
                options,
            });
    }

    fn answer_permissions(&mut self, approval_id: &str, answer: ApprovalAnswer) {
        let Some(offered) = self.permissions.remove(approval_id) else {
            return;
        };
        let kind = match answer {
            ApprovalAnswer::Selected(id) => Some(id),
            ApprovalAnswer::Cancelled => None,
        };
        for entry in offered {
            self.resolve_backend(&entry.backend_id, &entry.options, kind);
        }
    }

    /// Answers one backend permission request with the offered option of `kind`
    /// (`allow_once` or `reject_once`), or cancels it when `kind` is `None`.
    fn resolve_backend(
        &mut self,
        backend_id: &str,
        options: &[PermissionOption],
        kind: Option<&str>,
    ) {
        let answer = match kind.and_then(|kind| options.iter().find(|option| option.kind == kind)) {
            Some(option) => PermissionAnswer::Selected {
                option_id: option.option_id.clone(),
            },
            None => PermissionAnswer::Cancelled,
        };
        if let Err(err) = self
            .backend
            .resolve_permission(&RequestId::new(backend_id), answer)
        {
            self.audit(None, format!("RESOLVE_PERMISSION_FAILED: {err}"));
        }
    }

    /// Renders one committed event for the wire. A Status event carrying an approval id is
    /// rendered with the full approval request (RFC-0006 0006-006). A missing request or a body
    /// that cannot be encoded is returned as the audit reason; the caller skips the event.
    pub(crate) fn render(
        &self,
        event: &Event,
        context_id: &str,
        first: bool,
    ) -> Result<pb::StreamResponse, String> {
        let body = match &event.body {
            EventBody::Status {
                approval: Some(request_id),
                ..
            } => {
                let request = self
                    .tasks
                    .approval_request(request_id)
                    .ok_or_else(|| format!("APPROVAL_NOT_FOUND: {request_id}"))?;
                Some(
                    wire::approval_metadata(&request)
                        .map_err(|err| format!("APPROVAL_NOT_ENCODABLE: {err}"))?,
                )
            }
            _ => None,
        };
        let usage = match &event.body {
            EventBody::Status { state, .. }
                if TaskState::from_wire(*state).is_ok_and(|state| state.is_terminal()) =>
            {
                self.tasks
                    .budget_view(&event.task_id)
                    .map(|(_, usage)| usage)
            }
            _ => None,
        };
        Ok(wire::stream_response(
            event,
            context_id,
            first,
            body,
            usage.as_ref(),
        ))
    }

    async fn flush_outbox(&mut self, now: u64) -> Result<(), WorkerError> {
        for event in self.tasks.due_events(now) {
            let Some(owner) = self.tasks.task_owner(&event.task_id) else {
                self.tasks.ack(&event.task_id, event.seq)?;
                continue;
            };
            let artifact = artifact_key(&event);
            let first = artifact
                .as_ref()
                .is_some_and(|key| !self.artifacts.contains(key));
            let response = match self.render(&event, &owner.context_id, first) {
                Ok(response) => response,
                Err(reason) => {
                    // Not representable on the wire; dropping it keeps the outbox moving.
                    self.audit(Some(&event.task_id), reason);
                    self.tasks.ack(&event.task_id, event.seq)?;
                    continue;
                }
            };
            let outgoing =
                match self.encode(&event.task_id, &event.msg_id, event.stream_ends(), response) {
                    Ok(outgoing) => outgoing,
                    Err(err) => {
                        // Not representable on the wire; dropping it keeps the outbox moving.
                        self.audit(Some(&event.task_id), format!("EVENT_NOT_ENCODABLE: {err}"));
                        self.tasks.ack(&event.task_id, event.seq)?;
                        continue;
                    }
                };
            match self.publish(outgoing).await {
                Ok(()) => {
                    if let Some(key) = artifact {
                        self.artifacts.insert(key);
                    }
                    self.tasks.ack(&event.task_id, event.seq)?;
                }
                Err(_) => self.tasks.publish_failed(&event.task_id, event.seq, now),
            }
        }
        Ok(())
    }
}

fn rpc_pattern(config: &WorkerConfig) -> Result<SubjectPattern, WorkerError> {
    SubjectPattern::parse(&format!(
        "a2a.v1.{}.rpc.{}.>",
        config.tenant.as_str(),
        config.agent.as_str()
    ))
    .map_err(|_| WorkerError::Config("rpc subject pattern"))
}

/// The artifact a delta belongs to, or `None` for non-artifact events.
pub(crate) fn artifact_key(event: &Event) -> Option<(String, String)> {
    match &event.body {
        EventBody::Artifact { artifact_id, .. } => {
            Some((event.task_id.clone(), artifact_id.clone()))
        }
        EventBody::Status { .. } => None,
    }
}

/// Maps a backend tool kind to an approval tier. Unknown kinds are exec (fail closed).
fn tier_of(tool_kind: &str) -> Tier {
    match tool_kind {
        "read" | "search" | "fetch" | "think" => Tier::Read,
        "edit" => Tier::Write,
        "delete" | "move" => Tier::DeleteOrMove,
        _ => Tier::Exec,
    }
}

/// The cause a FAILED record carries (RFC-0005 0005-049): `{"kind", "detail"}` with the
/// text redacted by a2a-security. A redaction error is replaced by a fixed marker, never by
/// the raw text.
pub(crate) fn cause(kind: &str, text: &str) -> serde_json::Value {
    let detail = a2a_security::redact::redact(text).unwrap_or_else(|_| "[redacted]".to_owned());
    serde_json::Value::String(format!("{kind}: {detail}"))
}

fn start_failure(err: &BackendError) -> StopReason {
    match err {
        BackendError::Closed => StopReason::Crashed,
        BackendError::Unsupported(_) => StopReason::Backend(FailureCode::BackendUnavailable),
        BackendError::Busy | BackendError::NoSink => StopReason::Unknown,
    }
}
