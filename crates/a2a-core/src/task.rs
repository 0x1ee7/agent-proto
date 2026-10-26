//! Task lifecycle, admission, sequencing, outbox, dedupe, deadlines and
//! recovery (RFC-0005), wired to the budget and approval modules (RFC-0006).
//!
//! The owner store is the sync seam [`TaskStore`] (0001-004: no I/O in this
//! crate). Every mutation is planned as one [`Delta`], serialized, appended,
//! and only then applied to memory. Effects are returned only after the commit
//! succeeded, so a crash never leaves an effect without its durable record.

use std::collections::{BTreeMap, BTreeSet};

use a2a_types::failure::FailureCode;
use a2a_types::ids::{ContextId, MessageId, TaskId};
use a2a_types::lifecycle::{LifecycleError, check_transition};
use a2a_types::proto::a2a::TaskState;
use a2a_types::proto::a2abus::{ApprovalDecision, BudgetUsage, Hop};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::approval::{
    ApprovalBook, NewRequest, OpenError, Opened, Refusal, RequestState, Settled, Verdict,
};
use crate::budget::{
    Allowance, BreakerStatus, Breakers, BudgetError, BudgetTrip, DelegationRefused, Ledger,
    Metering, chain_has_loop,
};

/// Retention after a terminal state: T_task = 24 h (0005-026, 0005-043).
pub const RETENTION_MS: u64 = 86_400_000;
/// Grace between a backend cancel request and the kill (0005-037).
pub const CANCEL_GRACE_MS: u64 = 15_000;
/// Staleness allowance beyond the submission TTL (0005-027).
pub const STALE_GRACE_MS: u64 = 30_000;
/// Initial outbox republication backoff (0005-033).
pub const OUTBOX_BACKOFF_MIN_MS: u64 = 100;
/// Outbox republication backoff cap (0005-033).
pub const OUTBOX_BACKOFF_MAX_MS: u64 = 30_000;
/// ListTasks default page size (0005-044).
pub const DEFAULT_PAGE_SIZE: usize = 50;
/// ListTasks maximum page size (0005-044).
pub const MAX_PAGE_SIZE: usize = 100;

const BREAKER_DETAIL: &str = r#"{"breaker":"open"}"#;

/// Error from the record store seam.
#[derive(Debug, Error)]
#[error("store failure: {0}")]
pub struct StoreError(pub String);

/// Append-only record store. Records are opaque serialized [`Delta`]s.
pub trait TaskStore {
    /// Durably appends one record. Returns only after the record is durable.
    fn append(&mut self, record: &[u8]) -> Result<(), StoreError>;
    /// Returns every record in append order.
    fn records(&self) -> Result<Vec<Vec<u8>>, StoreError>;
}

/// In-memory [`TaskStore`] with fault injection for the append path.
#[derive(Debug, Default)]
pub struct MemStore {
    records: Vec<Vec<u8>>,
    fail_next: bool,
}

impl MemStore {
    /// Makes the next append fail without writing anything.
    pub fn fail_next_append(&mut self) {
        self.fail_next = true;
    }
}

impl TaskStore for MemStore {
    fn append(&mut self, record: &[u8]) -> Result<(), StoreError> {
        if std::mem::take(&mut self.fail_next) {
            return Err(StoreError("injected append failure".to_owned()));
        }
        self.records.push(record.to_vec());
        Ok(())
    }

    fn records(&self) -> Result<Vec<Vec<u8>>, StoreError> {
        Ok(self.records.clone())
    }
}

/// Errors from task operations.
#[derive(Debug, Error)]
pub enum TaskError {
    /// Unknown task, or a task not visible to the caller's origin (0005-004).
    #[error("task not found")]
    NotFound,
    /// The contextId does not match the task (0005-002).
    #[error("context id does not match the task")]
    ContextMismatch,
    /// The task is terminal; no transition or charge is possible (0005-007).
    #[error("task is terminal")]
    Terminal,
    /// The task is terminal and cannot be canceled (0005-038).
    #[error("task is not cancelable")]
    NotCancelable,
    /// Subscribing to a terminal task is unsupported (0005-020).
    #[error("subscribe is not supported for a terminal task")]
    NotSubscribable,
    /// The Worker is draining; no task is created (0005-050).
    #[error("service is draining")]
    Draining,
    /// The task is not in a state that accepts this operation.
    #[error("task is not active")]
    NotActive,
    /// The backend start was refused: not SUBMITTED, or already started.
    #[error("backend start refused")]
    NotStartable,
    /// The request is malformed.
    #[error("invalid request: {0}")]
    Invalid(&'static str),
    /// The ListTasks page token is malformed.
    #[error("invalid page token")]
    BadPageToken,
    /// A state transition is refused by Table 4.2.
    #[error(transparent)]
    Lifecycle(#[from] LifecycleError),
    /// A budget grant is invalid.
    #[error(transparent)]
    Budget(#[from] BudgetError),
    /// A delegation is refused by the parent's budget (0006-049).
    #[error("delegation refused")]
    Delegation(DelegationRefused),
    /// An approval decision is refused (0006-013, 0006-015).
    #[error("approval refused: {0}")]
    Approval(Refusal),
    /// An approval request is refused.
    #[error("approval request refused: {0}")]
    Open(OpenError),
    /// The record store failed; nothing was committed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A stored record could not be decoded.
    #[error("corrupt record: {0}")]
    Corrupt(String),
}

/// Stop reasons reported by a backend session (0005-012).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The turn completed normally.
    EndTurn,
    /// The backend stopped after a cancel request.
    Cancelled,
    /// The backend refused the turn.
    Refusal,
    /// The backend hit its token limit.
    MaxTokens,
    /// The backend hit its turn-request limit.
    MaxTurnRequests,
    /// The backend exited or crashed.
    Crashed,
    /// Any other stop reason.
    Unknown,
}

/// Answer sent to a backend for an open approval request (0006-027).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalAnswer {
    /// The selected offered option id.
    Selected(&'static str),
    /// No decision: the request was closed by expiry or cancel.
    Cancelled,
}

/// Side effects for the Worker. Returned only after the commit succeeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Invoke the backend for this task (0005-025), after [`Tasks::start_backend`].
    StartBackend { task_id: String },
    /// Ask the backend to stop; kill after `grace_ms` (0 = immediately).
    CancelBackend { task_id: String, grace_ms: u64 },
    /// Kill the backend now.
    KillBackend { task_id: String },
    /// Answer an approval request held by the backend.
    AnswerApproval {
        task_id: String,
        request_id: String,
        answer: ApprovalAnswer,
    },
    /// Cancel a child task (0005-039, 0006-050).
    CancelChild { task_id: String, child_id: String },
    /// Audit record for a refused or dropped event.
    Audit {
        task_id: Option<String>,
        reason: String,
    },
}

/// Admission outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    /// A task record was created (state may be SUBMITTED, REJECTED or FAILED).
    Created { task_id: String },
    /// The submission was already admitted (0005-023).
    Existing { task_id: String },
    /// The submission is stale and was dropped (0005-027).
    Stale,
}

/// Failure detail stored on FAILED and REJECTED states (0005-010).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    code: String,
    /// JSON text detail, when present.
    pub detail: Option<String>,
}

impl Failure {
    fn new(code: FailureCode, detail: Option<String>) -> Self {
        Self {
            code: code.as_str().to_owned(),
            detail,
        }
    }

    /// The failure code.
    #[must_use]
    pub fn code(&self) -> Option<FailureCode> {
        FailureCode::parse(&self.code).ok()
    }
}

/// Body of a committed event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventBody {
    /// A committed state transition (0005-013).
    Status {
        state: i32,
        failure: Option<Failure>,
        approval: Option<String>,
    },
    /// An artifact update (0005-022).
    Artifact {
        artifact_id: String,
        last_chunk: bool,
        payload: Vec<u8>,
    },
}

/// One event in a task's sequence (0005-014).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// Owning task.
    pub task_id: String,
    /// Contiguous sequence number, starting at 1.
    pub seq: u64,
    /// A2AB-Msg-Id, generated once at commit (0005-016).
    pub msg_id: String,
    /// Commit time.
    pub at_ms: u64,
    /// Event payload.
    pub body: EventBody,
}

impl Event {
    /// True when the event must carry A2AB-Stream-End (0005-018).
    #[must_use]
    pub fn stream_ends(&self) -> bool {
        match &self.body {
            EventBody::Status { state, .. } => {
                let state = state_of(*state);
                state.is_interrupted() || state.is_terminal()
            }
            EventBody::Artifact { .. } => false,
        }
    }
}

/// Snapshot of a task for reads (0005-021).
#[derive(Debug, Clone)]
pub struct TaskView {
    /// Task id.
    pub task_id: String,
    /// Context id.
    pub context_id: String,
    /// Current state.
    pub state: TaskState,
    /// Failure detail, on FAILED and REJECTED.
    pub failure: Option<Failure>,
    /// Last committed seq (the sequence metadata of 0005-021).
    pub seq: u64,
    /// Admission time.
    pub created_ms: u64,
    /// Last committed transition time.
    pub updated_ms: u64,
    /// Deadline (0005-040).
    pub deadline_ms: u64,
    /// Budget usage snapshot.
    pub usage: BudgetUsage,
}

/// ListTasks query (0005-044).
#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    /// Requested page size; defaults to 50 and clamps to 100.
    pub page_size: Option<usize>,
    /// Token from a previous page.
    pub page_token: Option<String>,
    /// Include tasks whose status timestamp is at or after this value.
    pub status_after_ms: Option<u64>,
}

/// One ListTasks page (0005-044).
#[derive(Debug, Clone)]
pub struct ListPage {
    /// Tasks on this page.
    pub tasks: Vec<TaskView>,
    /// Token for the next page; `None` on the last page.
    pub next_page_token: Option<String>,
    /// Count of all visible tasks matching the query.
    pub total_size: usize,
}

/// Child delegation to a registered child task.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Child {
    task_id: String,
    grant: Allowance,
    settled: bool,
}

/// Stored task state. Serialized into every delta that touches the task.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TaskRecord {
    task_id: String,
    context_id: String,
    origin: String,
    state: i32,
    seq: u64,
    failure: Option<Failure>,
    created_ms: u64,
    updated_ms: u64,
    terminal_ms: Option<u64>,
    deadline_ms: u64,
    ledger: Ledger,
    backend_started: bool,
    session_alive: bool,
    cancel_requested: bool,
    cancel_due_ms: Option<u64>,
    denied: bool,
    children: Vec<Child>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Admit {
    msg_id: String,
    origin: String,
    message_id: Option<String>,
    task_id: String,
}

/// One committed record: the complete effect of a mutation on durable state.
#[derive(Default, Serialize, Deserialize)]
struct Delta {
    upsert: BTreeMap<String, TaskRecord>,
    purged: Vec<String>,
    events: Vec<Event>,
    outbox_add: Vec<(String, u64)>,
    outbox_ack: Vec<(String, u64)>,
    admit: Vec<Admit>,
    breakers: Option<Breakers>,
    approvals: Option<ApprovalBook>,
}

#[derive(Debug, Clone, Copy)]
struct Backoff {
    attempts: u32,
    next_ms: u64,
}

/// Configuration of the owner.
#[derive(Debug, Clone)]
pub struct Config {
    /// The owning agent id; also the origin of local submissions.
    pub owner: String,
    /// Agent default deadline (0005-040), default 3600 s.
    pub agent_deadline_ms: u64,
    /// Root budget applied when a submission carries none.
    pub default_budget: Allowance,
    /// Metering mode for budget dimensions.
    pub metering: Metering,
}

/// A submission to admit (0005-023, 0005-024).
#[derive(Debug, Clone)]
pub struct Submission {
    /// A2AB-Msg-Id of the request.
    pub msg_id: String,
    /// Effective origin of the request.
    pub origin: String,
    /// Message.messageId, when supplied.
    pub message_id: Option<String>,
    /// Requested context id; a fresh one is generated when absent.
    pub context_id: Option<String>,
    /// A2AB-Sent time.
    pub sent_ms: u64,
    /// A2AB-Ttl.
    pub ttl_ms: u64,
    /// Fingerprint of the message (0006-044), for loop detection.
    pub fingerprint: String,
    /// Requested root budget, when supplied (0006-032).
    pub budget: Option<Allowance>,
    /// Set when the submission is a delegated child of a local task.
    pub incoming: Option<Incoming>,
    /// Overload signal from the transport (0005-028).
    pub pressure: Option<FailureCode>,
}

/// Delegated child submission carried through the parent's grant.
#[derive(Debug, Clone)]
pub struct Incoming {
    /// The child task id returned by [`Tasks::delegate`].
    pub task_id: String,
    /// The parent task id.
    pub parent_task_id: String,
    /// The grant reserved by the parent.
    pub grant: Allowance,
    /// The chain carried by the message (0006-043).
    pub chain: Vec<Hop>,
}

/// Mutation plan for one task. Commits as a single [`Delta`].
struct Plan {
    rec: TaskRecord,
    delta: Delta,
    effects: Vec<Effect>,
    book: ApprovalBook,
    book_touched: bool,
    breakers: Breakers,
    breakers_touched: bool,
    open_children: Vec<String>,
}

enum Ending {
    Completed,
    Canceled,
    Failed(FailureCode),
}

/// Owner-side task state. Each method commits before it returns.
#[derive(Debug)]
pub struct Tasks<S: TaskStore> {
    store: S,
    config: Config,
    draining: bool,
    tasks: BTreeMap<String, TaskRecord>,
    events: BTreeMap<String, Vec<Event>>,
    outbox: BTreeSet<(String, u64)>,
    backoff: BTreeMap<(String, u64), Backoff>,
    msg_ids: BTreeMap<String, String>,
    messages: BTreeMap<String, BTreeMap<String, String>>,
    breakers: Breakers,
    approvals: ApprovalBook,
}

fn state_of(raw: i32) -> TaskState {
    TaskState::from_wire(raw).unwrap_or(TaskState::Unspecified)
}

fn earliest(acc: Option<u64>, at: u64) -> Option<u64> {
    Some(acc.map_or(at, |a| a.min(at)))
}

fn active_error(state: TaskState) -> TaskError {
    if state.is_terminal() {
        TaskError::Terminal
    } else {
        TaskError::NotActive
    }
}

fn trip_detail(trip: &BudgetTrip) -> String {
    trip.detail().to_string()
}

impl Plan {
    fn state(&self) -> TaskState {
        state_of(self.rec.state)
    }

    fn book_mut(&mut self) -> &mut ApprovalBook {
        self.book_touched = true;
        &mut self.book
    }

    fn breakers_mut(&mut self) -> &mut Breakers {
        self.breakers_touched = true;
        &mut self.breakers
    }

    fn emit(&mut self, body: EventBody, now: u64) {
        let seq = self.rec.seq + 1;
        self.rec.seq = seq;
        self.delta.outbox_add.push((self.rec.task_id.clone(), seq));
        self.delta.events.push(Event {
            task_id: self.rec.task_id.clone(),
            seq,
            msg_id: MessageId::generate().to_string(),
            at_ms: now,
            body,
        });
    }

    /// Moves the task along Table 4.2 and emits one event.
    fn move_to(
        &mut self,
        from: Option<TaskState>,
        to: TaskState,
        failure: Option<Failure>,
        approval: Option<String>,
        now: u64,
    ) -> Result<(), TaskError> {
        if from.is_some_and(|state| state.is_terminal()) {
            return Err(TaskError::Terminal);
        }
        let code = failure.as_ref().and_then(Failure::code);
        check_transition(from, to, code)?;
        self.rec.state = to as i32;
        self.rec.updated_ms = now;
        self.rec.failure = failure.clone();
        if to.is_terminal() {
            self.rec.terminal_ms = Some(now);
            self.terminate(to, code, now);
        }
        self.emit(
            EventBody::Status {
                state: to as i32,
                failure,
                approval,
            },
            now,
        );
        Ok(())
    }

    /// Moves to the state of `code` (FAILED or REJECTED) with its detail.
    fn fail(
        &mut self,
        from: Option<TaskState>,
        code: FailureCode,
        detail: Option<String>,
        now: u64,
    ) -> Result<(), TaskError> {
        self.move_to(
            from,
            code.final_state(),
            Some(Failure::new(code, detail)),
            None,
            now,
        )
    }

    /// Side effects of entering a terminal state.
    fn terminate(&mut self, to: TaskState, code: Option<FailureCode>, now: u64) {
        let task_id = self.rec.task_id.clone();
        let origin = self.rec.origin.clone();
        let alive = std::mem::replace(&mut self.rec.session_alive, false);
        self.rec.cancel_due_ms = None;
        if alive {
            self.effects.push(Effect::CancelBackend {
                task_id: task_id.clone(),
                grace_ms: 0,
            });
        }
        if self.book.open_for_task(&task_id) > 0 {
            let ended = self.book_mut().close_task(&task_id);
            if alive {
                for request in ended {
                    self.effects.push(Effect::AnswerApproval {
                        task_id: task_id.clone(),
                        request_id: request.request_id,
                        answer: ApprovalAnswer::Cancelled,
                    });
                }
            }
        }
        for child_id in std::mem::take(&mut self.open_children) {
            self.effects.push(Effect::CancelChild {
                task_id: task_id.clone(),
                child_id,
            });
        }
        match (to, code) {
            (TaskState::Completed, _) => self.breakers_mut().record_success(&origin),
            (TaskState::Failed, Some(code)) => {
                self.breakers_mut().record_failure(&origin, code, now);
            }
            _ => {}
        }
    }
}

impl<S: TaskStore> Tasks<S> {
    /// Opens the owner store, replays every record, and recovers non-terminal
    /// tasks (0005-031). Returns the effects the Worker must perform.
    pub fn open(store: S, config: Config, now_ms: u64) -> Result<(Self, Vec<Effect>), TaskError> {
        let mut core = Self {
            store,
            config,
            draining: false,
            tasks: BTreeMap::new(),
            events: BTreeMap::new(),
            outbox: BTreeSet::new(),
            backoff: BTreeMap::new(),
            msg_ids: BTreeMap::new(),
            messages: BTreeMap::new(),
            breakers: Breakers::default(),
            approvals: ApprovalBook::default(),
        };
        for record in core.store.records()? {
            let delta: Delta =
                serde_json::from_slice(&record).map_err(|e| TaskError::Corrupt(e.to_string()))?;
            core.apply(delta);
        }
        let mut effects = Vec::new();
        let open: Vec<String> = core
            .tasks
            .values()
            .filter(|t| !state_of(t.state).is_terminal())
            .map(|t| t.task_id.clone())
            .collect();
        for task_id in open {
            effects.extend(core.recover(&task_id, now_ms)?);
        }
        effects.extend(core.expire_due(now_ms)?);
        Ok((core, effects))
    }

    /// Releases the store, for tests and restart simulations.
    pub fn into_store(self) -> S {
        self.store
    }

    /// Sets the DRAINING presence state (0005-050). In-memory only.
    pub fn set_draining(&mut self, draining: bool) {
        self.draining = draining;
    }

    fn recover(&mut self, task_id: &str, now: u64) -> Result<Vec<Effect>, TaskError> {
        let rec = self
            .tasks
            .get(task_id)
            .cloned()
            .ok_or(TaskError::NotFound)?;
        let state = state_of(rec.state);
        let mut plan = self.plan_with(rec);
        match state {
            TaskState::Submitted if !plan.rec.backend_started => {
                return Ok(vec![Effect::StartBackend {
                    task_id: task_id.to_owned(),
                }]);
            }
            TaskState::Working => {
                plan.rec.session_alive = false;
                plan.fail(Some(state), FailureCode::BackendCrashed, None, now)?;
            }
            TaskState::InputRequired => {
                plan.rec.session_alive = false;
                if plan.book.open_for_task(task_id) == 0 {
                    plan.fail(Some(state), FailureCode::ApprovalTimeout, None, now)?;
                }
            }
            _ => return Ok(Vec::new()),
        }
        self.commit(plan)
    }

    fn plan_with(&self, rec: TaskRecord) -> Plan {
        let open_children = rec
            .children
            .iter()
            .filter(|c| !c.settled)
            .filter(|c| {
                self.tasks
                    .get(&c.task_id)
                    .is_some_and(|t| !state_of(t.state).is_terminal())
            })
            .map(|c| c.task_id.clone())
            .collect();
        Plan {
            rec,
            delta: Delta::default(),
            effects: Vec::new(),
            book: self.approvals.clone(),
            book_touched: false,
            breakers: self.breakers.clone(),
            breakers_touched: false,
            open_children,
        }
    }

    fn plan(&self, task_id: &str) -> Result<Plan, TaskError> {
        let rec = self
            .tasks
            .get(task_id)
            .cloned()
            .ok_or(TaskError::NotFound)?;
        Ok(self.plan_with(rec))
    }

    fn visible(&self, origin: &str, task_id: &str) -> Result<&TaskRecord, TaskError> {
        self.tasks
            .get(task_id)
            .filter(|r| r.origin == origin)
            .ok_or(TaskError::NotFound)
    }

    fn visible_plan(&self, origin: &str, task_id: &str) -> Result<Plan, TaskError> {
        self.visible(origin, task_id)?;
        self.plan(task_id)
    }

    fn commit(&mut self, plan: Plan) -> Result<Vec<Effect>, TaskError> {
        let Plan {
            rec,
            mut delta,
            effects,
            book,
            book_touched,
            breakers,
            breakers_touched,
            ..
        } = plan;
        if book_touched {
            delta.approvals = Some(book);
        }
        if breakers_touched {
            delta.breakers = Some(breakers);
        }
        delta.upsert.insert(rec.task_id.clone(), rec);
        self.write(delta)?;
        Ok(effects)
    }

    fn write(&mut self, delta: Delta) -> Result<(), TaskError> {
        let bytes = serde_json::to_vec(&delta).map_err(|e| TaskError::Corrupt(e.to_string()))?;
        self.store.append(&bytes)?;
        self.apply(delta);
        Ok(())
    }

    /// Applies a committed delta to memory. Shared by commit and replay.
    fn apply(&mut self, delta: Delta) {
        let Delta {
            upsert,
            purged,
            events,
            outbox_add,
            outbox_ack,
            admit,
            breakers,
            approvals,
        } = delta;
        for (task_id, rec) in upsert {
            self.tasks.insert(task_id, rec);
        }
        for event in events {
            self.events
                .entry(event.task_id.clone())
                .or_default()
                .push(event);
        }
        self.outbox.extend(outbox_add);
        for key in outbox_ack {
            self.outbox.remove(&key);
            self.backoff.remove(&key);
        }
        for admission in admit {
            if let Some(message_id) = admission.message_id {
                self.messages
                    .entry(admission.origin)
                    .or_default()
                    .insert(message_id, admission.task_id.clone());
            }
            self.msg_ids.insert(admission.msg_id, admission.task_id);
        }
        if let Some(breakers) = breakers {
            self.breakers = breakers;
        }
        if let Some(book) = approvals {
            self.approvals = book;
        }
        if !purged.is_empty() {
            let gone: BTreeSet<&str> = purged.iter().map(String::as_str).collect();
            self.tasks.retain(|id, _| !gone.contains(id.as_str()));
            self.events.retain(|id, _| !gone.contains(id.as_str()));
            self.outbox.retain(|(id, _)| !gone.contains(id.as_str()));
            self.backoff
                .retain(|(id, _), _| !gone.contains(id.as_str()));
            self.msg_ids.retain(|_, id| !gone.contains(id.as_str()));
            for by_id in self.messages.values_mut() {
                by_id.retain(|_, id| !gone.contains(id.as_str()));
            }
        }
    }

    /// Admits a submission (0005-023, 0005-024, 0005-027, 0005-028, 0005-050).
    pub fn submit(
        &mut self,
        s: Submission,
        now: u64,
    ) -> Result<(Admission, Vec<Effect>), TaskError> {
        if let Some(task_id) = self.msg_ids.get(&s.msg_id) {
            return Ok((
                Admission::Existing {
                    task_id: task_id.clone(),
                },
                Vec::new(),
            ));
        }
        if let Some(message_id) = &s.message_id {
            if let Some(task_id) = self
                .messages
                .get(&s.origin)
                .and_then(|by_id| by_id.get(message_id))
            {
                return Ok((
                    Admission::Existing {
                        task_id: task_id.clone(),
                    },
                    Vec::new(),
                ));
            }
        }
        if now
            > s.sent_ms
                .saturating_add(s.ttl_ms)
                .saturating_add(STALE_GRACE_MS)
        {
            let audit = Effect::Audit {
                task_id: None,
                reason: "STALE_SUBMISSION".to_owned(),
            };
            return Ok((Admission::Stale, vec![audit]));
        }
        if self.draining {
            return Err(TaskError::Draining);
        }

        let task_id = match &s.incoming {
            Some(incoming) => incoming.task_id.clone(),
            None => TaskId::generate().to_string(),
        };
        TaskId::parse(&task_id).map_err(|_| TaskError::Invalid("task id"))?;
        if self.tasks.contains_key(&task_id) {
            return Err(TaskError::Invalid("task id already in use"));
        }

        let owner = self.config.owner.clone();
        let (grant, ledger) = match &s.incoming {
            None => {
                let grant = Allowance::root(&self.config.default_budget, s.budget.as_ref());
                (grant, Ledger::root(grant, self.config.metering)?)
            }
            Some(incoming) => {
                let parent = self
                    .tasks
                    .get(&incoming.parent_task_id)
                    .ok_or(TaskError::NotFound)?;
                let depth = parent.ledger.depth().saturating_add(1);
                (
                    incoming.grant,
                    Ledger::child(incoming.grant, depth, self.config.metering)?,
                )
            }
        };

        let looped = s
            .incoming
            .as_ref()
            .is_some_and(|incoming| chain_has_loop(&incoming.chain, &owner, &s.fingerprint));
        let refusal: Option<(FailureCode, Option<String>)> = if let Some(code) = s.pressure {
            Some((code, None))
        } else if matches!(
            self.breakers.status(&s.origin, now),
            BreakerStatus::Open { .. }
        ) {
            Some((FailureCode::RateLimited, Some(BREAKER_DETAIL.to_owned())))
        } else if looped {
            Some((FailureCode::LoopDetected, None))
        } else {
            None
        };

        let rec = TaskRecord {
            task_id: task_id.clone(),
            context_id: s
                .context_id
                .clone()
                .unwrap_or_else(|| ContextId::generate().to_string()),
            origin: s.origin.clone(),
            state: TaskState::Unspecified as i32,
            seq: 0,
            failure: None,
            created_ms: now,
            updated_ms: now,
            terminal_ms: None,
            deadline_ms: now.saturating_add(self.config.agent_deadline_ms.min(grant.max_wall_ms)),
            ledger,
            backend_started: false,
            session_alive: false,
            cancel_requested: false,
            cancel_due_ms: None,
            denied: false,
            children: Vec::new(),
        };
        let mut plan = self.plan_with(rec);
        match refusal {
            Some((code, detail)) => plan.fail(None, code, detail, now)?,
            None => {
                plan.move_to(None, TaskState::Submitted, None, None, now)?;
                plan.effects.push(Effect::StartBackend {
                    task_id: task_id.clone(),
                });
            }
        }
        plan.delta.admit.push(Admit {
            msg_id: s.msg_id,
            origin: s.origin,
            message_id: s.message_id,
            task_id: task_id.clone(),
        });
        let effects = self.commit(plan)?;
        Ok((Admission::Created { task_id }, effects))
    }

    /// Starts the backend turn: SUBMITTED to WORKING, committed before the
    /// Worker invokes the backend (0005-025). Returns the effects of the commit.
    pub fn start_backend(&mut self, task_id: &str, now: u64) -> Result<Vec<Effect>, TaskError> {
        let mut plan = self.plan(task_id)?;
        let state = plan.state();
        if state != TaskState::Submitted || plan.rec.backend_started {
            return Err(TaskError::NotStartable);
        }
        if let Err(trip) = plan.rec.ledger.start_turn() {
            plan.move_to(
                Some(TaskState::Submitted),
                TaskState::Working,
                None,
                None,
                now,
            )?;
            plan.fail(
                Some(TaskState::Working),
                trip.code(),
                Some(trip_detail(&trip)),
                now,
            )?;
            return self.commit(plan);
        }
        plan.rec.backend_started = true;
        plan.rec.session_alive = true;
        plan.move_to(
            Some(TaskState::Submitted),
            TaskState::Working,
            None,
            None,
            now,
        )?;
        self.commit(plan)
    }

    /// Records the backend stop reason and commits the mapped end state
    /// (0005-012, 0006-019, 0006-056).
    pub fn backend_stopped(
        &mut self,
        task_id: &str,
        reason: StopReason,
        now: u64,
    ) -> Result<Vec<Effect>, TaskError> {
        let mut plan = self.plan(task_id)?;
        let state = plan.state();
        if state.is_terminal() {
            plan.effects.push(Effect::Audit {
                task_id: Some(task_id.to_owned()),
                reason: "LATE_BACKEND_OUTPUT".to_owned(),
            });
            return Ok(plan.effects);
        }
        plan.rec.session_alive = false;
        let ending = if state == TaskState::InputRequired {
            Ending::Failed(FailureCode::BackendCrashed)
        } else if plan.rec.denied && reason != StopReason::EndTurn {
            Ending::Failed(FailureCode::ApprovalDenied)
        } else {
            match reason {
                StopReason::EndTurn => Ending::Completed,
                StopReason::Cancelled if plan.rec.cancel_requested => Ending::Canceled,
                StopReason::Cancelled | StopReason::Crashed => {
                    Ending::Failed(FailureCode::BackendCrashed)
                }
                StopReason::Refusal => Ending::Failed(FailureCode::AgentRefused),
                StopReason::MaxTokens | StopReason::MaxTurnRequests => {
                    Ending::Failed(FailureCode::AgentLimit)
                }
                StopReason::Unknown => Ending::Failed(FailureCode::BackendError),
            }
        };
        match ending {
            Ending::Completed => {
                plan.move_to(Some(state), TaskState::Completed, None, None, now)?
            }
            Ending::Canceled => plan.move_to(Some(state), TaskState::Canceled, None, None, now)?,
            Ending::Failed(code) => plan.fail(Some(state), code, None, now)?,
        }
        self.commit(plan)
    }

    /// Appends an artifact update as one event (0005-022). No state change.
    pub fn append_artifact(
        &mut self,
        task_id: &str,
        artifact_id: String,
        last_chunk: bool,
        payload: Vec<u8>,
        now: u64,
    ) -> Result<(), TaskError> {
        let mut plan = self.plan(task_id)?;
        if plan.state() != TaskState::Working {
            return Err(active_error(plan.state()));
        }
        plan.emit(
            EventBody::Artifact {
                artifact_id,
                last_chunk,
                payload,
            },
            now,
        );
        self.commit(plan).map(|_| ())
    }

    /// Charges the usage reported by the backend and enforces the budget
    /// (RFC-0006, 0006-029..039).
    pub fn charge(
        &mut self,
        task_id: &str,
        tokens: u64,
        cumulative_cost: Option<u64>,
        now: u64,
    ) -> Result<Vec<Effect>, TaskError> {
        let mut plan = self.plan(task_id)?;
        let state = plan.state();
        if state != TaskState::Working {
            return Err(active_error(state));
        }
        plan.rec.ledger.charge_tokens(tokens);
        if let Some(cost) = cumulative_cost {
            plan.rec.ledger.observe_cost(cost);
        }
        if let Some(trip) = plan.rec.ledger.check_after_usage() {
            plan.fail(Some(state), trip.code(), Some(trip_detail(&trip)), now)?;
        }
        self.commit(plan)
    }

    /// Opens an approval request (0006-005..007). The task moves to
    /// INPUT_REQUIRED in the same commit as the request (0005-008).
    pub fn request_approval(
        &mut self,
        task_id: &str,
        request: NewRequest,
        now: u64,
    ) -> Result<(Opened, Vec<Effect>), TaskError> {
        if request.task_id != task_id {
            return Err(TaskError::Invalid("approval task id"));
        }
        let request_id = request.request_id.clone();
        let mut plan = self.plan(task_id)?;
        let state = plan.state();
        if !matches!(state, TaskState::Working | TaskState::InputRequired) {
            return Err(active_error(state));
        }
        let opened = plan
            .book_mut()
            .open(request, now)
            .map_err(TaskError::Open)?;
        match opened {
            Opened::Created { .. } => {
                if state == TaskState::Working {
                    plan.move_to(
                        Some(state),
                        TaskState::InputRequired,
                        None,
                        Some(request_id),
                        now,
                    )?;
                } else {
                    plan.emit(
                        EventBody::Status {
                            state: TaskState::InputRequired as i32,
                            failure: None,
                            approval: Some(request_id),
                        },
                        now,
                    );
                }
                let effects = self.commit(plan)?;
                Ok((opened, effects))
            }
            Opened::Joined { .. } => Ok((opened, Vec::new())),
            Opened::Flood => {
                plan.effects.push(Effect::Audit {
                    task_id: Some(task_id.to_owned()),
                    reason: "APPROVAL_FLOOD".to_owned(),
                });
                Ok((opened, plan.effects))
            }
        }
    }

    /// Applies an operator decision (0006-012..020). Refusals commit nothing,
    /// except that a decision on a dead session closes the request and fails
    /// the task (0006-056).
    pub fn decide(
        &mut self,
        origin: &str,
        decision: &ApprovalDecision,
        verified_operator: Option<&str>,
        tenant: &str,
        now: u64,
    ) -> Result<(Settled, Vec<Effect>), TaskError> {
        let mut plan = self.visible_plan(origin, &decision.task_id)?;
        let settled = plan
            .book_mut()
            .decide(decision, verified_operator, tenant, now)
            .map_err(TaskError::Approval)?;
        if settled.replayed {
            return Ok((settled, Vec::new()));
        }
        let state = plan.state();
        let task_id = plan.rec.task_id.clone();
        if !plan.rec.session_alive {
            plan.book_mut().close_task(&task_id);
            plan.fail(Some(state), FailureCode::BackendCrashed, None, now)?;
            self.commit(plan)?;
            return Err(TaskError::Approval(Refusal::NotApplicable));
        }
        if matches!(settled.verdict, Verdict::Denied) {
            plan.rec.denied = true;
        }
        plan.effects.push(Effect::AnswerApproval {
            task_id: task_id.clone(),
            request_id: decision.request_id.clone(),
            answer: ApprovalAnswer::Selected(settled.verdict.option_id()),
        });
        if state == TaskState::InputRequired && plan.book.open_for_task(&task_id) == 0 {
            plan.move_to(Some(state), TaskState::Working, None, None, now)?;
        }
        let effects = self.commit(plan)?;
        Ok((settled, effects))
    }

    /// Reserves a child grant on a WORKING parent (0006-049). Returns the new
    /// child task id and its grant; the child is then admitted with
    /// [`Submission::incoming`].
    pub fn delegate(&mut self, parent_id: &str) -> Result<(String, Allowance), TaskError> {
        let mut plan = self.plan(parent_id)?;
        let state = plan.state();
        if state != TaskState::Working {
            return Err(active_error(state));
        }
        let grant = plan.rec.ledger.delegate().map_err(TaskError::Delegation)?;
        let child_id = TaskId::generate().to_string();
        plan.rec.children.push(Child {
            task_id: child_id.clone(),
            grant,
            settled: false,
        });
        self.commit(plan)?;
        Ok((child_id, grant))
    }

    /// Settles a terminal child against its parent (0006-047, 0006-049). A
    /// child that ended FAILED, REJECTED or CANCELED fails the parent with
    /// CHILD_FAILED while the parent is WORKING.
    pub fn settle_child(
        &mut self,
        parent_id: &str,
        child_id: &str,
        now: u64,
    ) -> Result<Vec<Effect>, TaskError> {
        let child = self.tasks.get(child_id).ok_or(TaskError::NotFound)?.clone();
        let child_state = state_of(child.state);
        if !child_state.is_terminal() {
            return Err(TaskError::Invalid("child task is not terminal"));
        }
        let mut plan = self.plan(parent_id)?;
        let entry = plan
            .rec
            .children
            .iter_mut()
            .find(|c| c.task_id == child_id)
            .ok_or(TaskError::NotFound)?;
        if entry.settled {
            return Ok(Vec::new());
        }
        entry.settled = true;
        let grant = entry.grant;
        plan.rec.ledger.settle_child(&grant, child.ledger.usage());
        let parent_state = plan.state();
        if child_state != TaskState::Completed && !parent_state.is_terminal() {
            let code = child
                .failure
                .as_ref()
                .map_or_else(|| "CANCELED".to_owned(), |f| f.code.clone());
            let detail =
                serde_json::json!({"child_task_id": child_id, "child_code": code}).to_string();
            plan.fail(
                Some(parent_state),
                FailureCode::ChildFailed,
                Some(detail),
                now,
            )?;
        }
        self.commit(plan)
    }

    /// Expires approvals and deadlines, and kills backends past their cancel
    /// grace (0005-037, 0005-041, 0006-017).
    pub fn expire_due(&mut self, now: u64) -> Result<Vec<Effect>, TaskError> {
        let mut book = self.approvals.clone();
        let ended = book.expire_due(now);
        if !ended.is_empty() {
            self.write(Delta {
                approvals: Some(book),
                ..Delta::default()
            })?;
        }
        let mut expired: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for request in ended {
            if matches!(request.state, RequestState::Expired) {
                expired
                    .entry(request.task_id)
                    .or_default()
                    .push(request.request_id);
            }
        }
        let mut effects = Vec::new();
        for (task_id, request_ids) in expired {
            let Some(rec) = self.tasks.get(&task_id).cloned() else {
                continue;
            };
            let state = state_of(rec.state);
            if state != TaskState::InputRequired {
                continue;
            }
            let mut plan = self.plan_with(rec);
            if plan.rec.session_alive {
                for request_id in request_ids {
                    plan.effects.push(Effect::AnswerApproval {
                        task_id: task_id.clone(),
                        request_id,
                        answer: ApprovalAnswer::Cancelled,
                    });
                }
            }
            plan.fail(Some(state), FailureCode::ApprovalTimeout, None, now)?;
            effects.extend(self.commit(plan)?);
        }

        let due: Vec<String> = self
            .tasks
            .values()
            .filter(|t| !state_of(t.state).is_terminal() && t.deadline_ms <= now)
            .map(|t| t.task_id.clone())
            .collect();
        for task_id in due {
            let mut plan = self.plan(&task_id)?;
            let state = plan.state();
            plan.fail(Some(state), FailureCode::DeadlineExceeded, None, now)?;
            effects.extend(self.commit(plan)?);
        }

        let kill: Vec<String> = self
            .tasks
            .values()
            .filter(|t| {
                state_of(t.state) == TaskState::Working
                    && t.cancel_due_ms.is_some_and(|due| due <= now)
            })
            .map(|t| t.task_id.clone())
            .collect();
        for task_id in kill {
            let mut plan = self.plan(&task_id)?;
            if std::mem::replace(&mut plan.rec.session_alive, false) {
                plan.effects.push(Effect::KillBackend {
                    task_id: task_id.clone(),
                });
            }
            plan.move_to(
                Some(TaskState::Working),
                TaskState::Canceled,
                None,
                None,
                now,
            )?;
            effects.extend(self.commit(plan)?);
        }
        Ok(effects)
    }

    /// Cancels a task for its origin (0005-036..038, 0005-051).
    pub fn cancel(
        &mut self,
        origin: &str,
        task_id: &str,
        now: u64,
    ) -> Result<Vec<Effect>, TaskError> {
        let mut plan = self.visible_plan(origin, task_id)?;
        let state = plan.state();
        match state {
            TaskState::Canceled => Ok(Vec::new()),
            TaskState::Completed | TaskState::Failed | TaskState::Rejected => {
                Err(TaskError::NotCancelable)
            }
            TaskState::Submitted | TaskState::InputRequired => {
                plan.move_to(Some(state), TaskState::Canceled, None, None, now)?;
                self.commit(plan)
            }
            TaskState::Working => {
                if plan.rec.cancel_requested {
                    return Ok(Vec::new());
                }
                plan.rec.cancel_requested = true;
                plan.rec.cancel_due_ms = Some(now.saturating_add(CANCEL_GRACE_MS));
                if plan.rec.session_alive {
                    plan.effects.push(Effect::CancelBackend {
                        task_id: task_id.to_owned(),
                        grace_ms: CANCEL_GRACE_MS,
                    });
                }
                self.commit(plan)
            }
            _ => Err(TaskError::Invalid("unknown task state")),
        }
    }

    /// Checks the contextId of a follow-up against the task (0005-002).
    pub fn check_context(
        &self,
        origin: &str,
        task_id: &str,
        context_id: &str,
    ) -> Result<(), TaskError> {
        let rec = self.visible(origin, task_id)?;
        if rec.context_id == context_id {
            Ok(())
        } else {
            Err(TaskError::ContextMismatch)
        }
    }

    /// Reads one task for its origin (0005-004, 0005-021).
    pub fn task(&self, origin: &str, task_id: &str) -> Result<TaskView, TaskError> {
        Ok(view(self.visible(origin, task_id)?))
    }

    /// Events after `after_seq` for a live task (0005-019). Terminal tasks
    /// are refused (0005-020).
    pub fn subscribe(
        &self,
        origin: &str,
        task_id: &str,
        after_seq: u64,
    ) -> Result<Vec<Event>, TaskError> {
        let rec = self.visible(origin, task_id)?;
        if state_of(rec.state).is_terminal() {
            return Err(TaskError::NotSubscribable);
        }
        Ok(self.events.get(task_id).map_or_else(Vec::new, |events| {
            events
                .iter()
                .filter(|e| e.seq > after_seq)
                .cloned()
                .collect()
        }))
    }

    /// Lists the visible tasks of an origin (0005-044).
    pub fn list(&self, origin: &str, query: &ListQuery) -> Result<ListPage, TaskError> {
        let mut items: Vec<&TaskRecord> = self
            .tasks
            .values()
            .filter(|t| {
                t.origin == origin
                    && query
                        .status_after_ms
                        .is_none_or(|after| t.updated_ms >= after)
            })
            .collect();
        items.sort_by(|a, b| {
            (b.updated_ms, b.task_id.as_str()).cmp(&(a.updated_ms, a.task_id.as_str()))
        });
        let total_size = items.len();
        let start = match &query.page_token {
            None => 0,
            Some(token) => {
                let (updated, id) = token.split_once(':').ok_or(TaskError::BadPageToken)?;
                let updated = updated
                    .parse::<u64>()
                    .map_err(|_| TaskError::BadPageToken)?;
                let key = (updated, id);
                items.partition_point(|t| (t.updated_ms, t.task_id.as_str()) >= key)
            }
        };
        let size = match query.page_size {
            Some(n) if n > 0 => n.min(MAX_PAGE_SIZE),
            _ => DEFAULT_PAGE_SIZE,
        };
        let start = start.min(total_size);
        let end = start.saturating_add(size).min(total_size);
        let next_page_token = if end < total_size && end > start {
            let last = items[end - 1];
            Some(format!("{}:{}", last.updated_ms, last.task_id))
        } else {
            None
        };
        let tasks = items[start..end].iter().map(|t| view(t)).collect();
        Ok(ListPage {
            tasks,
            next_page_token,
            total_size,
        })
    }

    /// Removes terminal tasks past retention (0005-026, 0005-043).
    pub fn purge_expired(&mut self, now: u64) -> Result<Vec<String>, TaskError> {
        let purged: Vec<String> = self
            .tasks
            .values()
            .filter(|t| {
                t.terminal_ms
                    .is_some_and(|at| at.saturating_add(RETENTION_MS) <= now)
            })
            .map(|t| t.task_id.clone())
            .collect();
        if !purged.is_empty() {
            self.write(Delta {
                purged: purged.clone(),
                ..Delta::default()
            })?;
        }
        Ok(purged)
    }

    /// Earliest time the Worker must call [`Tasks::expire_due`] or publish the
    /// outbox again. `None` when nothing is pending.
    #[must_use]
    pub fn next_wakeup(&self, now: u64) -> Option<u64> {
        let mut next = None;
        for task in self
            .tasks
            .values()
            .filter(|t| !state_of(t.state).is_terminal())
        {
            next = earliest(next, task.deadline_ms);
            if let Some(due) = task.cancel_due_ms {
                next = earliest(next, due);
            }
        }
        if let Some(expiry) = self.approvals.next_expiry() {
            next = earliest(next, expiry);
        }
        for key in &self.outbox {
            next = earliest(next, self.backoff.get(key).map_or(now, |b| b.next_ms));
        }
        next
    }

    fn event(&self, task_id: &str, seq: u64) -> Option<&Event> {
        let index = usize::try_from(seq).ok()?.checked_sub(1)?;
        self.events.get(task_id)?.get(index)
    }

    /// Outbox entries due for publication. Within a task, a deferred entry
    /// holds back every later seq of that task (0005-017).
    #[must_use]
    pub fn due_events(&self, now: u64) -> Vec<Event> {
        let mut out = Vec::new();
        let mut blocked: BTreeSet<&str> = BTreeSet::new();
        for (task_id, seq) in &self.outbox {
            if blocked.contains(task_id.as_str()) {
                continue;
            }
            let key = (task_id.clone(), *seq);
            if self.backoff.get(&key).is_some_and(|b| b.next_ms > now) {
                blocked.insert(task_id.as_str());
                continue;
            }
            if let Some(event) = self.event(task_id, *seq) {
                out.push(event.clone());
            }
        }
        out
    }

    /// Marks an outbox entry published (0005-015). Durable.
    pub fn ack(&mut self, task_id: &str, seq: u64) -> Result<(), TaskError> {
        let key = (task_id.to_owned(), seq);
        if !self.outbox.contains(&key) {
            return Ok(());
        }
        self.write(Delta {
            outbox_ack: vec![key],
            ..Delta::default()
        })
    }

    /// Records a failed publication and raises the backoff (0005-033). The
    /// backoff is in memory only.
    pub fn publish_failed(&mut self, task_id: &str, seq: u64, now: u64) {
        let key = (task_id.to_owned(), seq);
        let attempts = self
            .backoff
            .get(&key)
            .map_or(0, |b| b.attempts.saturating_add(1));
        let delay = (OUTBOX_BACKOFF_MIN_MS << attempts.min(9)).min(OUTBOX_BACKOFF_MAX_MS);
        self.backoff.insert(
            key,
            Backoff {
                attempts,
                next_ms: now.saturating_add(delay),
            },
        );
    }
}

fn view(rec: &TaskRecord) -> TaskView {
    TaskView {
        task_id: rec.task_id.clone(),
        context_id: rec.context_id.clone(),
        state: state_of(rec.state),
        failure: rec.failure.clone(),
        seq: rec.seq,
        created_ms: rec.created_ms,
        updated_ms: rec.updated_ms,
        deadline_ms: rec.deadline_ms,
        usage: rec.ledger.usage_proto(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::approval::{
        ALLOW_ONCE_ID, DEFAULT_EXPIRY_MS, NONCE_MIN_BYTES, NewRequest, Opened, REJECT_ONCE_ID,
        Refusal, RequestState, Tier,
    };
    use crate::budget::BREAKER_OPEN_MS;
    use a2a_types::ids::TaskId;

    const ORIGIN: &str = "claude";
    const OTHER: &str = "gateway";
    const OPERATOR: &str = "operator:alice";
    const TENANT: &str = "tenant-a";
    const T0: u64 = 1_000_000;
    const DEADLINE: u64 = 3_600_000;

    fn budget() -> Allowance {
        Allowance {
            max_tokens: 10_000,
            max_cost_units: 10_000,
            max_wall_ms: DEADLINE,
            max_hops: 8,
            max_depth: 3,
            max_children: 8,
        }
    }

    fn config() -> Config {
        Config {
            owner: "omp".to_owned(),
            agent_deadline_ms: DEADLINE,
            default_budget: budget(),
            metering: Metering {
                tokens: true,
                cost: true,
            },
        }
    }

    fn fresh() -> Tasks<MemStore> {
        Tasks::open(MemStore::default(), config(), T0)
            .expect("open")
            .0
    }

    fn reopen(tasks: Tasks<MemStore>, now: u64) -> (Tasks<MemStore>, Vec<Effect>) {
        Tasks::open(tasks.into_store(), config(), now).expect("reopen")
    }

    fn sub(msg: &str, now: u64) -> Submission {
        Submission {
            msg_id: msg.to_owned(),
            origin: ORIGIN.to_owned(),
            message_id: None,
            context_id: None,
            sent_ms: now,
            ttl_ms: 60_000,
            fingerprint: format!("fp-{msg}"),
            budget: None,
            incoming: None,
            pressure: None,
        }
    }

    fn id_of(admission: Admission) -> String {
        match admission {
            Admission::Created { task_id } | Admission::Existing { task_id } => task_id,
            Admission::Stale => String::new(),
        }
    }

    fn start(tasks: &mut Tasks<MemStore>, msg: &str, now: u64) -> String {
        let (admission, effects) = tasks.submit(sub(msg, now), now).expect("submit");
        let id = id_of(admission);
        assert!(effects.contains(&Effect::StartBackend {
            task_id: id.clone()
        }));
        tasks.start_backend(&id, now).expect("start");
        id
    }

    fn state(tasks: &Tasks<MemStore>, id: &str) -> TaskState {
        tasks.task(ORIGIN, id).expect("visible").state
    }

    fn nonce_hex(request_id: &str) -> String {
        request_id
            .bytes()
            .cycle()
            .take(NONCE_MIN_BYTES)
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn ask(
        tasks: &mut Tasks<MemStore>,
        id: &str,
        request_id: &str,
        tool: &str,
        now: u64,
    ) -> (Opened, Vec<Effect>) {
        let request = NewRequest {
            request_id: request_id.to_owned(),
            task_id: id.to_owned(),
            tool: tool.to_owned(),
            tier: Tier::Exec,
            summary: format!("run {tool}"),
            requester: "claude".to_owned(),
            chain: vec!["omp".to_owned(), "claude".to_owned()],
            nonce: request_id.bytes().cycle().take(NONCE_MIN_BYTES).collect(),
            expiry_ms: None,
        };
        tasks
            .request_approval(id, request, now)
            .expect("request approval")
    }

    fn digest_of(opened: &Opened) -> String {
        match opened {
            Opened::Created { digest, .. } => digest.clone(),
            Opened::Joined { .. } | Opened::Flood => String::new(),
        }
    }

    fn decision(id: &str, request_id: &str, option: &str, digest: &str) -> ApprovalDecision {
        ApprovalDecision {
            request_id: request_id.to_owned(),
            task_id: id.to_owned(),
            option_id: option.to_owned(),
            nonce: nonce_hex(request_id),
            decided_by: String::new(),
            decided_at: None,
            request_sha256: digest.to_owned(),
        }
    }

    fn delegate_child(tasks: &mut Tasks<MemStore>, parent: &str, msg: &str, now: u64) -> String {
        let (child, _grant) = tasks.delegate(parent).expect("delegate");
        let mut s = sub(msg, now);
        s.incoming = Some(Incoming {
            task_id: child.clone(),
            parent_task_id: parent.to_owned(),
            grant: budget(),
            chain: vec![Hop {
                agent_id: "omp".to_owned(),
                fingerprint: String::new(),
            }],
        });
        let (admission, _) = tasks.submit(s, now).expect("child submit");
        assert_eq!(id_of(admission), child);
        tasks.start_backend(&child, now).expect("child start");
        child
    }

    fn failure_code(tasks: &Tasks<MemStore>, id: &str) -> Option<FailureCode> {
        tasks
            .task(ORIGIN, id)
            .expect("visible")
            .failure
            .as_ref()
            .and_then(Failure::code)
    }

    #[test]
    fn req_0005_001_task_ids_are_uuidv7_and_unknown_ids_are_not_found() {
        let mut tasks = fresh();
        let id = start(&mut tasks, "m1", T0);
        assert!(TaskId::parse(&id).is_ok());
        assert_eq!(id.as_bytes()[14], b'7');
        assert!(matches!(
            tasks.task(ORIGIN, "no-such-task"),
            Err(TaskError::NotFound)
        ));
        assert_eq!(state(&tasks, &id), TaskState::Working);
    }

    #[test]
    fn req_0005_002_context_must_match_the_task() {
        let mut tasks = fresh();
        let mut s = sub("m1", T0);
        s.context_id = Some("ctx-1".to_owned());
        let id = id_of(tasks.submit(s, T0).expect("submit").0);
        assert!(tasks.check_context(ORIGIN, &id, "ctx-1").is_ok());
        assert!(matches!(
            tasks.check_context(ORIGIN, &id, "ctx-2"),
            Err(TaskError::ContextMismatch)
        ));
        assert_eq!(tasks.task(ORIGIN, &id).expect("view").context_id, "ctx-1");
    }

    #[test]
    fn req_0005_004_other_origin_sees_not_found() {
        let mut tasks = fresh();
        let id = start(&mut tasks, "m1", T0);
        assert!(matches!(tasks.task(OTHER, &id), Err(TaskError::NotFound)));
        assert!(matches!(
            tasks.subscribe(OTHER, &id, 0),
            Err(TaskError::NotFound)
        ));
        assert!(matches!(
            tasks.cancel(OTHER, &id, T0),
            Err(TaskError::NotFound)
        ));
        assert!(
            tasks
                .list(OTHER, &ListQuery::default())
                .expect("list")
                .tasks
                .is_empty()
        );
    }

    #[test]
    fn req_0005_006_transitions_follow_table_4_2() {
        let mut tasks = fresh();
        let id = id_of(tasks.submit(sub("m1", T0), T0).expect("submit").0);
        let mut plan = tasks.plan(&id).expect("plan");
        assert!(matches!(
            plan.move_to(
                Some(TaskState::Submitted),
                TaskState::Completed,
                None,
                None,
                T0
            ),
            Err(TaskError::Lifecycle(_))
        ));
        assert!(
            plan.move_to(
                Some(TaskState::Submitted),
                TaskState::Working,
                None,
                None,
                T0
            )
            .is_ok()
        );
        plan.rec.state = TaskState::Completed as i32;
        assert!(matches!(
            plan.move_to(
                Some(TaskState::Completed),
                TaskState::Working,
                None,
                None,
                T0
            ),
            Err(TaskError::Terminal)
        ));
    }

    #[test]
    fn req_0005_007_terminal_states_are_immutable() {
        let mut tasks = fresh();
        let id = start(&mut tasks, "m1", T0);
        tasks
            .backend_stopped(&id, StopReason::EndTurn, T0 + 1)
            .expect("complete");
        assert_eq!(state(&tasks, &id), TaskState::Completed);
        assert!(matches!(
            tasks.start_backend(&id, T0 + 2),
            Err(TaskError::NotStartable)
        ));
        assert!(matches!(
            tasks.charge(&id, 1, None, T0 + 2),
            Err(TaskError::Terminal)
        ));
        assert!(matches!(
            tasks.cancel(ORIGIN, &id, T0 + 2),
            Err(TaskError::NotCancelable)
        ));
        assert_eq!(state(&tasks, &id), TaskState::Completed);
    }

    #[test]
    fn req_0005_008_decision_resumes_same_task_and_context_once() {
        let mut tasks = fresh();
        let mut s = sub("m1", T0);
        s.context_id = Some("ctx-1".to_owned());
        let id = id_of(tasks.submit(s, T0).expect("submit").0);
        tasks.start_backend(&id, T0).expect("start");
        let (opened, _) = ask(&mut tasks, &id, "r1", "bash", T0);
        let digest = digest_of(&opened);
        assert_eq!(state(&tasks, &id), TaskState::InputRequired);
        let allow = decision(&id, "r1", ALLOW_ONCE_ID, &digest);
        assert!(matches!(
            tasks.decide(ORIGIN, &allow, None, TENANT, T0 + 1),
            Err(TaskError::Approval(Refusal::NotOperator))
        ));
        assert_eq!(state(&tasks, &id), TaskState::InputRequired);
        let (settled, effects) = tasks
            .decide(ORIGIN, &allow, Some(OPERATOR), TENANT, T0 + 1)
            .expect("decide");
        assert!(!settled.replayed);
        assert_eq!(
            effects,
            vec![Effect::AnswerApproval {
                task_id: id.clone(),
                request_id: "r1".to_owned(),
                answer: ApprovalAnswer::Selected(ALLOW_ONCE_ID)
            }]
        );
        let view = tasks.task(ORIGIN, &id).expect("view");
        assert_eq!(view.state, TaskState::Working);
        assert_eq!(view.context_id, "ctx-1");
        let (replay, effects) = tasks
            .decide(ORIGIN, &allow, Some(OPERATOR), TENANT, T0 + 2)
            .expect("replay");
        assert!(replay.replayed);
        assert!(effects.is_empty());
        let other = decision(&id, "r1", REJECT_ONCE_ID, &digest);
        assert!(matches!(
            tasks.decide(ORIGIN, &other, Some(OPERATOR), TENANT, T0 + 3),
            Err(TaskError::Approval(Refusal::AlreadyResolved))
        ));
    }

    #[test]
    fn req_0005_012_stop_reasons_map_to_failure_codes() {
        let cases = [
            (StopReason::EndTurn, TaskState::Completed, None),
            (
                StopReason::Refusal,
                TaskState::Failed,
                Some(FailureCode::AgentRefused),
            ),
            (
                StopReason::MaxTokens,
                TaskState::Failed,
                Some(FailureCode::AgentLimit),
            ),
            (
                StopReason::MaxTurnRequests,
                TaskState::Failed,
                Some(FailureCode::AgentLimit),
            ),
            (
                StopReason::Crashed,
                TaskState::Failed,
                Some(FailureCode::BackendCrashed),
            ),
            (
                StopReason::Unknown,
                TaskState::Failed,
                Some(FailureCode::BackendError),
            ),
        ];
        for (i, (reason, want_state, want_code)) in cases.into_iter().enumerate() {
            let mut tasks = fresh();
            let id = start(&mut tasks, &format!("m{i}"), T0);
            tasks.backend_stopped(&id, reason, T0 + 1).expect("stop");
            assert_eq!(state(&tasks, &id), want_state, "{reason:?}");
            assert_eq!(failure_code(&tasks, &id), want_code, "{reason:?}");
        }
        let mut tasks = fresh();
        let id = start(&mut tasks, "c", T0);
        tasks.cancel(ORIGIN, &id, T0 + 1).expect("cancel");
        tasks
            .backend_stopped(&id, StopReason::Cancelled, T0 + 2)
            .expect("stop");
        assert_eq!(state(&tasks, &id), TaskState::Canceled);
        let mut tasks = fresh();
        let id = start(&mut tasks, "u", T0);
        tasks
            .backend_stopped(&id, StopReason::Cancelled, T0 + 1)
            .expect("stop");
        assert_eq!(failure_code(&tasks, &id), Some(FailureCode::BackendCrashed));
    }

    #[test]
    fn req_0005_013_014_one_event_per_transition_with_contiguous_seq() {
        let mut tasks = fresh();
        let id = start(&mut tasks, "m1", T0);
        tasks
            .append_artifact(&id, "a1".to_owned(), true, b"x".to_vec(), T0 + 1)
            .expect("artifact");
        tasks
            .backend_stopped(&id, StopReason::EndTurn, T0 + 2)
            .expect("stop");
        let seqs: Vec<u64> = tasks.events[&id].iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![1, 2, 3, 4]);
        assert_eq!(tasks.task(ORIGIN, &id).expect("view").seq, 4);
    }

    #[test]
    fn req_0005_015_016_outbox_ack_is_durable_and_message_id_is_stable() {
        let mut tasks = fresh();
        let id = start(&mut tasks, "m1", T0);
        let first = tasks.due_events(T0);
        assert_eq!(first.len(), 2);
        let msg_id = first[0].msg_id.clone();
        tasks.ack(&id, 1).expect("ack");
        tasks.ack(&id, 2).expect("ack");
        assert!(tasks.due_events(T0).is_empty());
        let (tasks, _) = reopen(tasks, T0 + 1);
        let pending: Vec<u64> = tasks.due_events(T0 + 1).iter().map(|e| e.seq).collect();
        assert_eq!(pending, vec![3], "acked seqs 1 and 2 are not republished");
        assert_eq!(tasks.events[&id][0].msg_id, msg_id);
    }

    #[test]
    fn req_0005_018_stream_end_only_on_interrupt_and_terminal_states() {
        let event = |state: TaskState| Event {
            task_id: "t".to_owned(),
            seq: 1,
            msg_id: "m".to_owned(),
            at_ms: T0,
            body: EventBody::Status {
                state: state as i32,
                failure: None,
                approval: None,
            },
        };
        assert!(event(TaskState::InputRequired).stream_ends());
        assert!(event(TaskState::Completed).stream_ends());
        assert!(event(TaskState::Failed).stream_ends());
        assert!(event(TaskState::Canceled).stream_ends());
        assert!(event(TaskState::Rejected).stream_ends());
        assert!(!event(TaskState::Submitted).stream_ends());
        assert!(!event(TaskState::Working).stream_ends());
    }

    #[test]
    fn req_0005_019_020_subscribe_replays_after_seq_and_refuses_terminal() {
        let mut tasks = fresh();
        let id = start(&mut tasks, "m1", T0);
        tasks
            .append_artifact(&id, "a1".to_owned(), true, Vec::new(), T0 + 1)
            .expect("artifact");
        let replay = tasks.subscribe(ORIGIN, &id, 1).expect("subscribe");
        assert_eq!(replay.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![2, 3]);
        tasks
            .backend_stopped(&id, StopReason::EndTurn, T0 + 2)
            .expect("stop");
        assert!(matches!(
            tasks.subscribe(ORIGIN, &id, 0),
            Err(TaskError::NotSubscribable)
        ));
    }

    #[test]
    fn req_0005_022_artifact_updates_do_not_change_state() {
        let mut tasks = fresh();
        let id = start(&mut tasks, "m1", T0);
        tasks
            .append_artifact(&id, "a1".to_owned(), true, b"data".to_vec(), T0 + 1)
            .expect("artifact");
        assert_eq!(state(&tasks, &id), TaskState::Working);
        let last = tasks.events[&id].last().expect("event").clone();
        assert!(matches!(&last.body, EventBody::Artifact { .. }));
        assert!(!last.stream_ends());
    }

    #[test]
    fn req_0005_023_duplicate_message_returns_existing_task() {
        let mut tasks = fresh();
        let (first, _) = tasks.submit(sub("m1", T0), T0).expect("first");
        let (second, effects) = tasks.submit(sub("m1", T0 + 1), T0 + 1).expect("second");
        assert_eq!(id_of(first.clone()), id_of(second.clone()));
        assert!(matches!(second, Admission::Existing { .. }));
        assert!(effects.is_empty());
    }

    #[test]
    fn req_0005_024_admission_commit_is_atomic() {
        let mut tasks = fresh();
        tasks.store.fail_next_append();
        assert!(matches!(
            tasks.submit(sub("m1", T0), T0),
            Err(TaskError::Store(_))
        ));
        assert!(
            tasks
                .list(ORIGIN, &ListQuery::default())
                .expect("list")
                .tasks
                .is_empty()
        );
        let (admission, _) = tasks.submit(sub("m1", T0), T0).expect("retry");
        assert!(matches!(admission, Admission::Created { .. }));
    }

    #[test]
    fn req_0005_025_session_start_is_durable_before_the_backend_is_invoked() {
        let mut tasks = fresh();
        let id = id_of(tasks.submit(sub("m1", T0), T0).expect("submit").0);
        tasks.store.fail_next_append();
        assert!(tasks.start_backend(&id, T0).is_err());
        assert_eq!(state(&tasks, &id), TaskState::Submitted);
        let (mut tasks, effects) = reopen(tasks, T0 + 1);
        assert!(effects.contains(&Effect::StartBackend {
            task_id: id.clone()
        }));
        tasks.start_backend(&id, T0 + 2).expect("start");
        let (tasks, effects) = reopen(tasks, T0 + 3);
        assert!(effects.is_empty());
        assert_eq!(state(&tasks, &id), TaskState::Failed);
        assert_eq!(failure_code(&tasks, &id), Some(FailureCode::BackendCrashed));
    }

    #[test]
    fn req_0005_026_043_retention_purges_only_terminal_tasks_past_window() {
        let mut tasks = fresh();
        let live = start(&mut tasks, "live", T0);
        let done = start(&mut tasks, "done", T0);
        tasks
            .backend_stopped(&done, StopReason::EndTurn, T0 + 1)
            .expect("stop");
        assert!(
            tasks
                .purge_expired(T0 + 1 + RETENTION_MS - 1)
                .expect("early")
                .is_empty()
        );
        let purged = tasks.purge_expired(T0 + 1 + RETENTION_MS).expect("purge");
        assert_eq!(purged, vec![done.clone()]);
        assert!(matches!(
            tasks.task(ORIGIN, &done),
            Err(TaskError::NotFound)
        ));
        assert_eq!(state(&tasks, &live), TaskState::Working);
    }

    #[test]
    fn req_0005_027_stale_submission_creates_no_record() {
        let mut tasks = fresh();
        let mut s = sub("m1", T0);
        s.sent_ms = T0 - 120_000;
        s.ttl_ms = 60_000;
        let (admission, effects) = tasks.submit(s, T0).expect("submit");
        assert_eq!(admission, Admission::Stale);
        assert_eq!(
            effects,
            vec![Effect::Audit {
                task_id: None,
                reason: "STALE_SUBMISSION".to_owned()
            }]
        );
        assert!(
            tasks
                .list(ORIGIN, &ListQuery::default())
                .expect("list")
                .tasks
                .is_empty()
        );
    }

    #[test]
    fn req_0005_028_overload_rejects_with_seq_one_and_stream_end() {
        let mut tasks = fresh();
        let mut s = sub("m1", T0);
        s.pressure = Some(FailureCode::QueueFull);
        let (admission, effects) = tasks.submit(s, T0).expect("submit");
        let id = id_of(admission);
        assert!(effects.is_empty());
        assert_eq!(state(&tasks, &id), TaskState::Rejected);
        assert_eq!(failure_code(&tasks, &id), Some(FailureCode::QueueFull));
        let events = &tasks.events[&id];
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].seq, 1);
        assert!(events[0].stream_ends());
    }

    #[test]
    fn req_0005_029_030_redelivery_never_reruns_a_terminal_task() {
        let mut tasks = fresh();
        let id = start(&mut tasks, "m1", T0);
        tasks
            .backend_stopped(&id, StopReason::EndTurn, T0 + 1)
            .expect("stop");
        let (again, effects) = tasks.submit(sub("m1", T0 + 2), T0 + 2).expect("redeliver");
        assert_eq!(id_of(again), id);
        assert!(effects.is_empty());
        assert_eq!(state(&tasks, &id), TaskState::Completed);
    }

    #[test]
    fn req_0005_031_restart_recovery_matrix() {
        let mut tasks = fresh();
        let submitted = id_of(tasks.submit(sub("a", T0), T0).expect("a").0);
        let working = start(&mut tasks, "b", T0);
        let waiting = start(&mut tasks, "c", T0);
        ask(&mut tasks, &waiting, "r1", "bash", T0);
        let (tasks, effects) = reopen(tasks, T0 + 1);
        assert_eq!(
            effects,
            vec![Effect::StartBackend {
                task_id: submitted.clone()
            }]
        );
        assert_eq!(state(&tasks, &working), TaskState::Failed);
        assert_eq!(
            failure_code(&tasks, &working),
            Some(FailureCode::BackendCrashed)
        );
        assert_eq!(state(&tasks, &waiting), TaskState::InputRequired);
        assert_eq!(tasks.approvals.state("r1"), Some(RequestState::Open));
    }

    #[test]
    fn req_0005_032_backend_crash_fails_the_task_immediately() {
        let mut tasks = fresh();
        let id = start(&mut tasks, "m1", T0);
        tasks
            .backend_stopped(&id, StopReason::Crashed, T0 + 5)
            .expect("stop");
        let view = tasks.task(ORIGIN, &id).expect("view");
        assert_eq!(view.state, TaskState::Failed);
        assert_eq!(view.updated_ms, T0 + 5);
        assert_eq!(failure_code(&tasks, &id), Some(FailureCode::BackendCrashed));
    }

    #[test]
    fn req_0005_033_outbox_backoff_defers_republication() {
        let mut tasks = fresh();
        let id = start(&mut tasks, "m1", T0);
        tasks.publish_failed(&id, 1, T0);
        assert!(tasks.due_events(T0 + 99).is_empty());
        assert_eq!(
            tasks
                .due_events(T0 + 100)
                .iter()
                .map(|e| e.seq)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        for _ in 0..20 {
            tasks.publish_failed(&id, 1, T0);
        }
        let next = tasks.backoff[&(id.clone(), 1)].next_ms;
        assert_eq!(next, T0 + OUTBOX_BACKOFF_MAX_MS);
    }

    #[test]
    fn req_0005_035_expiry_wins_over_a_late_decision() {
        let mut tasks = fresh();
        let id = start(&mut tasks, "m1", T0);
        let (opened, _) = ask(&mut tasks, &id, "r1", "bash", T0);
        let digest = digest_of(&opened);
        tasks.expire_due(T0 + DEFAULT_EXPIRY_MS).expect("expire");
        let late = decision(&id, "r1", ALLOW_ONCE_ID, &digest);
        assert!(
            tasks
                .decide(
                    ORIGIN,
                    &late,
                    Some(OPERATOR),
                    TENANT,
                    T0 + DEFAULT_EXPIRY_MS + 1
                )
                .is_err()
        );
        assert_eq!(state(&tasks, &id), TaskState::Failed);
        assert_eq!(
            failure_code(&tasks, &id),
            Some(FailureCode::ApprovalTimeout)
        );
    }

    #[test]
    fn req_0005_036_cancel_before_start_is_immediate() {
        let mut tasks = fresh();
        let id = id_of(tasks.submit(sub("m1", T0), T0).expect("submit").0);
        assert!(
            tasks
                .cancel(ORIGIN, &id, T0 + 1)
                .expect("cancel")
                .is_empty()
        );
        assert_eq!(state(&tasks, &id), TaskState::Canceled);
        assert!(matches!(
            tasks.start_backend(&id, T0 + 2),
            Err(TaskError::NotStartable)
        ));
    }

    #[test]
    fn req_0005_037_cancel_of_working_task_gives_grace_then_kills() {
        let mut tasks = fresh();
        let id = start(&mut tasks, "m1", T0);
        let effects = tasks.cancel(ORIGIN, &id, T0 + 1).expect("cancel");
        assert_eq!(
            effects,
            vec![Effect::CancelBackend {
                task_id: id.clone(),
                grace_ms: CANCEL_GRACE_MS
            }]
        );
        assert_eq!(state(&tasks, &id), TaskState::Working);
        assert!(
            tasks
                .expire_due(T0 + 1 + CANCEL_GRACE_MS - 1)
                .expect("early")
                .is_empty()
        );
        let effects = tasks.expire_due(T0 + 1 + CANCEL_GRACE_MS).expect("kill");
        assert!(effects.contains(&Effect::KillBackend {
            task_id: id.clone()
        }));
        assert_eq!(state(&tasks, &id), TaskState::Canceled);
    }

    #[test]
    fn req_0005_038_cancel_is_idempotent_and_refuses_terminal_tasks() {
        let mut tasks = fresh();
        let id = id_of(tasks.submit(sub("m1", T0), T0).expect("submit").0);
        tasks.cancel(ORIGIN, &id, T0).expect("first");
        assert!(
            tasks
                .cancel(ORIGIN, &id, T0 + 1)
                .expect("second")
                .is_empty()
        );
        let done = start(&mut tasks, "m2", T0);
        tasks
            .backend_stopped(&done, StopReason::EndTurn, T0 + 1)
            .expect("complete");
        assert!(matches!(
            tasks.cancel(ORIGIN, &done, T0 + 2),
            Err(TaskError::NotCancelable)
        ));
    }

    #[test]
    fn req_0005_039_cancel_cascades_to_open_children() {
        let mut tasks = fresh();
        let parent = start(&mut tasks, "p", T0);
        let child = delegate_child(&mut tasks, &parent, "c1", T0);
        tasks.cancel(ORIGIN, &parent, T0 + 1).expect("cancel");
        let effects = tasks.expire_due(T0 + 1 + CANCEL_GRACE_MS).expect("kill");
        assert!(effects.contains(&Effect::CancelChild {
            task_id: parent.clone(),
            child_id: child.clone()
        }));
        assert_eq!(state(&tasks, &parent), TaskState::Canceled);
    }

    #[test]
    fn req_0005_040_deadline_is_admission_plus_the_smaller_of_agent_and_grant() {
        let mut tasks = fresh();
        let default = id_of(tasks.submit(sub("a", T0), T0).expect("a").0);
        assert_eq!(
            tasks.task(ORIGIN, &default).expect("view").deadline_ms,
            T0 + DEADLINE
        );
        let mut s = sub("b", T0);
        s.budget = Some(Allowance {
            max_wall_ms: 5_000,
            ..budget()
        });
        let short = id_of(tasks.submit(s, T0).expect("b").0);
        assert_eq!(
            tasks.task(ORIGIN, &short).expect("view").deadline_ms,
            T0 + 5_000
        );
    }

    #[test]
    fn req_0005_041_deadline_fails_without_grace_period() {
        let mut tasks = fresh();
        let id = start(&mut tasks, "m1", T0);
        assert!(
            tasks
                .expire_due(T0 + DEADLINE - 1)
                .expect("early")
                .is_empty()
        );
        let effects = tasks.expire_due(T0 + DEADLINE).expect("deadline");
        assert!(effects.contains(&Effect::CancelBackend {
            task_id: id.clone(),
            grace_ms: 0
        }));
        assert_eq!(state(&tasks, &id), TaskState::Failed);
        assert_eq!(
            failure_code(&tasks, &id),
            Some(FailureCode::DeadlineExceeded)
        );
    }

    #[test]
    fn req_0005_044_list_is_newest_first_and_paged() {
        let mut tasks = fresh();
        let a = start(&mut tasks, "a", T0);
        let b = start(&mut tasks, "b", T0 + 1);
        let c = start(&mut tasks, "c", T0 + 2);
        let page = tasks
            .list(
                ORIGIN,
                &ListQuery {
                    page_size: Some(2),
                    ..ListQuery::default()
                },
            )
            .expect("page 1");
        assert_eq!(
            page.tasks
                .iter()
                .map(|t| t.task_id.clone())
                .collect::<Vec<_>>(),
            vec![c.clone(), b.clone()]
        );
        assert!(page.next_page_token.is_some());
        let q = ListQuery {
            page_size: Some(2),
            page_token: page.next_page_token.clone(),
            ..ListQuery::default()
        };
        let rest = tasks.list(ORIGIN, &q).expect("page 2");
        assert_eq!(
            rest.tasks
                .iter()
                .map(|t| t.task_id.clone())
                .collect::<Vec<_>>(),
            vec![a]
        );
        assert!(rest.next_page_token.is_none());
        assert_eq!(rest.total_size, 3);
    }

    #[test]
    fn req_0005_047_tasks_may_share_a_context() {
        let mut tasks = fresh();
        let mut a = sub("m1", T0);
        a.context_id = Some("ctx".to_owned());
        let mut b = sub("m2", T0);
        b.context_id = Some("ctx".to_owned());
        let first = id_of(tasks.submit(a, T0).expect("a").0);
        let second = id_of(tasks.submit(b, T0).expect("b").0);
        assert_ne!(first, second);
        assert!(tasks.check_context(ORIGIN, &first, "ctx").is_ok());
        assert!(tasks.check_context(ORIGIN, &second, "ctx").is_ok());
    }

    #[test]
    fn req_0005_050_draining_refuses_new_tasks_without_creating_one() {
        let mut tasks = fresh();
        tasks.set_draining(true);
        assert!(matches!(
            tasks.submit(sub("m1", T0), T0),
            Err(TaskError::Draining)
        ));
        assert!(
            tasks
                .list(ORIGIN, &ListQuery::default())
                .expect("list")
                .tasks
                .is_empty()
        );
        tasks.set_draining(false);
        assert!(tasks.submit(sub("m1", T0), T0).is_ok());
    }

    #[test]
    fn req_0005_051_cross_origin_cancel_is_not_found_even_when_terminal() {
        let mut tasks = fresh();
        let id = start(&mut tasks, "m1", T0);
        tasks
            .backend_stopped(&id, StopReason::EndTurn, T0 + 1)
            .expect("complete");
        assert!(matches!(
            tasks.cancel(OTHER, &id, T0 + 2),
            Err(TaskError::NotFound)
        ));
    }

    #[test]
    fn req_0006_005_006_007_request_parks_task_and_expires_at_default_window() {
        let mut tasks = fresh();
        let id = start(&mut tasks, "m1", T0);
        let (opened, _) = ask(&mut tasks, &id, "r1", "bash", T0);
        assert!(matches!(opened, Opened::Created { .. }));
        assert_eq!(state(&tasks, &id), TaskState::InputRequired);
        let last = tasks.events[&id].last().expect("event").clone();
        assert!(last.stream_ends());
        assert!(
            matches!(&last.body, EventBody::Status { approval: Some(rid), .. } if rid.as_str() == "r1")
        );
        assert!(
            tasks
                .expire_due(T0 + DEFAULT_EXPIRY_MS - 1)
                .expect("early")
                .is_empty()
        );
        assert_eq!(state(&tasks, &id), TaskState::InputRequired);
        let effects = tasks.expire_due(T0 + DEFAULT_EXPIRY_MS).expect("expiry");
        assert!(effects.contains(&Effect::AnswerApproval {
            task_id: id.clone(),
            request_id: "r1".to_owned(),
            answer: ApprovalAnswer::Cancelled
        }));
        assert_eq!(state(&tasks, &id), TaskState::Failed);
        assert_eq!(
            failure_code(&tasks, &id),
            Some(FailureCode::ApprovalTimeout)
        );
    }

    #[test]
    fn req_0006_018_cancel_closes_open_requests_and_answers_cancelled() {
        let mut tasks = fresh();
        let id = start(&mut tasks, "m1", T0);
        ask(&mut tasks, &id, "r1", "bash", T0);
        let effects = tasks.cancel(ORIGIN, &id, T0 + 1).expect("cancel");
        assert!(effects.contains(&Effect::AnswerApproval {
            task_id: id.clone(),
            request_id: "r1".to_owned(),
            answer: ApprovalAnswer::Cancelled
        }));
        assert_eq!(state(&tasks, &id), TaskState::Canceled);
        assert_eq!(tasks.approvals.state("r1"), Some(RequestState::Closed));
    }

    #[test]
    fn req_0006_019_denied_then_backend_failure_is_approval_denied() {
        let mut tasks = fresh();
        let id = start(&mut tasks, "m1", T0);
        let (opened, _) = ask(&mut tasks, &id, "r1", "bash", T0);
        let deny = decision(&id, "r1", REJECT_ONCE_ID, &digest_of(&opened));
        let (_, effects) = tasks
            .decide(ORIGIN, &deny, Some(OPERATOR), TENANT, T0 + 1)
            .expect("decide");
        assert_eq!(
            effects,
            vec![Effect::AnswerApproval {
                task_id: id.clone(),
                request_id: "r1".to_owned(),
                answer: ApprovalAnswer::Selected(REJECT_ONCE_ID)
            }]
        );
        assert_eq!(state(&tasks, &id), TaskState::Working);
        tasks
            .backend_stopped(&id, StopReason::Refusal, T0 + 2)
            .expect("stop");
        assert_eq!(state(&tasks, &id), TaskState::Failed);
        assert_eq!(failure_code(&tasks, &id), Some(FailureCode::ApprovalDenied));
    }

    #[test]
    fn req_0006_022_fourth_open_request_is_flooded_and_audited() {
        let mut tasks = fresh();
        let id = start(&mut tasks, "m1", T0);
        ask(&mut tasks, &id, "r1", "t1", T0);
        ask(&mut tasks, &id, "r2", "t2", T0);
        ask(&mut tasks, &id, "r3", "t3", T0);
        let before = tasks.events[&id].len();
        let (opened, effects) = ask(&mut tasks, &id, "r4", "t4", T0);
        assert!(matches!(opened, Opened::Flood));
        assert!(effects.iter().any(|e| matches!(e, Effect::Audit { .. })));
        assert_eq!(tasks.events[&id].len(), before);
        assert_eq!(tasks.approvals.state("r4"), None);
    }

    #[test]
    fn req_0006_035_expiry_closes_request_and_audits_late_decision() {
        let mut tasks = fresh();
        let id = start(&mut tasks, "m1", T0);
        let (opened, _) = ask(&mut tasks, &id, "r1", "bash", T0);
        let digest = digest_of(&opened);
        tasks.expire_due(T0 + DEFAULT_EXPIRY_MS).expect("expire");
        assert_eq!(tasks.approvals.state("r1"), Some(RequestState::Expired));
        let late = decision(&id, "r1", ALLOW_ONCE_ID, &digest);
        assert!(
            tasks
                .decide(
                    ORIGIN,
                    &late,
                    Some(OPERATOR),
                    TENANT,
                    T0 + DEFAULT_EXPIRY_MS + 1
                )
                .is_err()
        );
        assert_eq!(state(&tasks, &id), TaskState::Failed);
    }

    #[test]
    fn req_0006_048_breaker_opens_after_three_origin_failures() {
        let mut tasks = fresh();
        let tight = Allowance {
            max_tokens: 10,
            ..budget()
        };
        for i in 0..crate::budget::BREAKER_THRESHOLD {
            let mut s = sub(&format!("b{i}"), T0);
            s.budget = Some(tight);
            let id = id_of(tasks.submit(s, T0).expect("submit").0);
            tasks.start_backend(&id, T0).expect("start");
            tasks.charge(&id, 100, None, T0 + 1).expect("charge");
            assert_eq!(state(&tasks, &id), TaskState::Failed);
        }
        let refused = id_of(
            tasks
                .submit(sub("refused", T0 + 2), T0 + 2)
                .expect("refused")
                .0,
        );
        let view = tasks.task(ORIGIN, &refused).expect("view");
        assert_eq!(view.state, TaskState::Rejected);
        assert_eq!(
            failure_code(&tasks, &refused),
            Some(FailureCode::RateLimited)
        );
        assert_eq!(
            view.failure.and_then(|f| f.detail).as_deref(),
            Some(r#"{"breaker":"open"}"#)
        );
        let mut other = sub("other", T0 + 2);
        other.origin = OTHER.to_owned();
        assert!(matches!(
            tasks.submit(other, T0 + 2).expect("other").0,
            Admission::Created { .. }
        ));
        let later = T0 + 2 + BREAKER_OPEN_MS;
        let (adm, _) = tasks.submit(sub("later", later), later).expect("later");
        assert_eq!(state(&tasks, &id_of(adm)), TaskState::Submitted);
    }

    #[test]
    fn req_0006_049_child_failure_fails_parent_with_child_failed() {
        let mut tasks = fresh();
        let parent = start(&mut tasks, "p", T0);
        let child = delegate_child(&mut tasks, &parent, "c1", T0);
        tasks
            .backend_stopped(&child, StopReason::Refusal, T0 + 1)
            .expect("child stop");
        tasks.settle_child(&parent, &child, T0 + 2).expect("settle");
        assert_eq!(state(&tasks, &parent), TaskState::Failed);
        assert_eq!(
            failure_code(&tasks, &parent),
            Some(FailureCode::ChildFailed)
        );
        let detail = tasks
            .task(ORIGIN, &parent)
            .expect("view")
            .failure
            .and_then(|f| f.detail)
            .unwrap_or_default();
        assert!(detail.contains(&child));
    }

    #[test]
    fn req_0006_050_budget_trip_cancels_open_children() {
        let mut tasks = fresh();
        let parent = start(&mut tasks, "p", T0);
        let child = delegate_child(&mut tasks, &parent, "c1", T0);
        let effects = tasks.charge(&parent, 20_000, None, T0 + 1).expect("charge");
        assert!(effects.contains(&Effect::CancelChild {
            task_id: parent.clone(),
            child_id: child.clone()
        }));
        assert_eq!(state(&tasks, &parent), TaskState::Failed);
        assert_eq!(
            failure_code(&tasks, &parent),
            Some(FailureCode::BudgetExhausted)
        );
    }

    #[test]
    fn req_0006_056_backend_exit_closes_open_approvals_and_refuses_later_decisions() {
        let mut tasks = fresh();
        let id = start(&mut tasks, "m1", T0);
        let (opened, _) = ask(&mut tasks, &id, "r1", "bash", T0);
        let digest = digest_of(&opened);
        let effects = tasks
            .backend_stopped(&id, StopReason::Crashed, T0 + 1)
            .expect("stop");
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::AnswerApproval { .. }))
        );
        assert_eq!(tasks.approvals.state("r1"), Some(RequestState::Closed));
        assert_eq!(state(&tasks, &id), TaskState::Failed);
        assert_eq!(failure_code(&tasks, &id), Some(FailureCode::BackendCrashed));
        let late = decision(&id, "r1", ALLOW_ONCE_ID, &digest);
        assert!(
            tasks
                .decide(ORIGIN, &late, Some(OPERATOR), TENANT, T0 + 2)
                .is_err()
        );
    }

    #[test]
    fn req_0006_056_recovered_session_never_inherits_an_open_approval() {
        let mut tasks = fresh();
        let id = start(&mut tasks, "m1", T0);
        let (opened, _) = ask(&mut tasks, &id, "r1", "bash", T0);
        let digest = digest_of(&opened);
        let (mut tasks, effects) = reopen(tasks, T0 + 1);
        assert!(effects.is_empty());
        assert_eq!(state(&tasks, &id), TaskState::InputRequired);
        let late = decision(&id, "r1", ALLOW_ONCE_ID, &digest);
        assert!(matches!(
            tasks.decide(ORIGIN, &late, Some(OPERATOR), TENANT, T0 + 2),
            Err(TaskError::Approval(Refusal::NotApplicable))
        ));
        assert_eq!(state(&tasks, &id), TaskState::Failed);
        assert_eq!(failure_code(&tasks, &id), Some(FailureCode::BackendCrashed));
    }
}
