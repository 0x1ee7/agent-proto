//! `Backend` implementation for A2A v1.0 HTTP+JSON-RPC peers (RFC-0009 4.6, 4.10, 4.11, 4.12).
//!
//! The `Backend` trait is synchronous. [`HttpBackend`] owns a multi-thread tokio
//! runtime; each turn runs as a task on it and reports through the attached sink.
//! Every started turn ends with exactly one `Terminal` event (0009-006).

use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use a2a_types::ids::AgentId;
use a2a_worker::backend::{
    Backend, BackendError, BackendEvent, BackendSink, Capabilities, FaultKind, Health, HealthClass,
    InputPart, Outcome, PermissionAnswer, Protocol, ReasonToken, RequestId, StartTurn,
    StatusPayload, TurnRef,
};
use serde_json::Value;
use tokio::runtime::Runtime;
use tokio::sync::{Mutex as AsyncMutex, watch};
use tokio::task::AbortHandle;
use tokio::time::Instant;

use crate::card;
use crate::client::{CallError, EventStream, Transport, build_http};
use crate::config::{CARD_FETCH_CAP, CARD_REFRESH, PeerSettings, Secret, bearer_allowed};
use crate::namespace;
use crate::ownership::{TaskMapError, TaskOwnership};
use crate::redact::{redact, scrub};
use crate::rpc::{
    self, CANCEL_TASK, Envelope, FailClass, GET_TASK, Malformed, PeerState, SEND_MESSAGE,
    SEND_STREAMING, SUBSCRIBE_TASK, Update, classify_code, classify_status, task_params,
};

/// Text the peer uses when its reply deadline elapsed (docs/interop/hermes.md 6.4).
const REPLY_TIMEOUT_MARKER: &str = "[agent did not reply in time]";
/// Reply artifact key used when the peer names none.
const REPLY_KEY: &str = "reply";
/// First poll interval; doubles up to [`POLL_MAX`] (0009-049).
const POLL_MIN: Duration = Duration::from_secs(2);
/// Poll interval cap (0009-049).
const POLL_MAX: Duration = Duration::from_secs(10);
/// Consecutive unreachable polls before the turn fails as BACKEND_CRASHED (0008-078).
const DOWN_LIMIT: u32 = 3;
const SHUTDOWN_POLL: Duration = Duration::from_millis(10);
/// Bounded wait for the best-effort CancelTask (0009-047).
const CANCEL_BOUND: Duration = Duration::from_secs(5);
/// Bounded connection retry delays for SendMessage (0009-045).
const RETRY_DELAYS: [Duration; 2] = [Duration::from_millis(500), Duration::from_millis(1000)];

/// Failure to construct an [`HttpBackend`].
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// The HTTP client could not be built.
    #[error("cannot build the HTTP client")]
    Http(#[source] reqwest::Error),
    /// The async runtime could not be started.
    #[error("cannot start the async runtime")]
    Runtime(#[source] std::io::Error),
    /// The persisted task ownership map cannot be loaded (0009-077).
    #[error("cannot load the task ownership map")]
    TaskMap(#[source] TaskMapError),
}

/// Bus-facing card for a peer, built only from the bus agent id (0008-073).
pub fn bus_card(agent_id: &AgentId) -> Value {
    card::sanitize(agent_id.as_str())
}

/// A2A HTTP+JSON-RPC backend for one configured peer.
pub struct HttpBackend {
    shared: Arc<Shared>,
    runtime: Option<Runtime>,
}

impl fmt::Debug for HttpBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpBackend")
            .field("agent_id", &self.shared.settings.agent_id.as_str())
            .finish_non_exhaustive()
    }
}

impl HttpBackend {
    /// Creates the backend and its runtime. The peer card is fetched lazily on the first turn.
    ///
    /// # Errors
    /// Returns [`ConnectError`] when the HTTP client or runtime cannot be created.
    pub fn connect(settings: PeerSettings) -> Result<Self, ConnectError> {
        let http = build_http(settings.timeout).map_err(ConnectError::Http)?;
        let ownership =
            TaskOwnership::load(&settings.task_map_path).map_err(ConnectError::TaskMap)?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("a2a-http")
            .build()
            .map_err(ConnectError::Runtime)?;
        Ok(Self {
            shared: Arc::new(Shared {
                settings,
                http,
                sink: Mutex::new(None),
                open: Mutex::new(None),
                peer: AsyncMutex::new(None),
                ownership: Mutex::new(ownership),
                live: AtomicBool::new(true),
            }),
            runtime: Some(runtime),
        })
    }
}

impl Backend for HttpBackend {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            protocol: Protocol::A2aHttp,
            resume: false,
            cancel: true,
            usage_sources: Vec::new(),
            approval_modes: Vec::new(),
            streaming_text: self.shared.settings.streaming,
        }
    }

    fn health(&self) -> Health {
        Health {
            live: self.shared.live.load(Ordering::SeqCst),
            restarts: 0,
            reason: None,
        }
    }

    fn attach(&mut self, sink: Box<dyn BackendSink>) {
        *lock(&self.shared.sink) = Some(sink);
    }

    fn start(&mut self, start: StartTurn) -> Result<(), BackendError> {
        let runtime = self.runtime.as_ref().ok_or(BackendError::Closed)?;
        if lock(&self.shared.sink).is_none() {
            return Err(BackendError::NoSink);
        }
        let (cancel_tx, cancel_rx) = watch::channel(false);
        {
            let mut open = lock(&self.shared.open);
            if open.is_some() {
                return Err(BackendError::Busy);
            }
            *open = Some(ActiveTurn {
                turn: start.turn.clone(),
                cancel: cancel_tx,
                aborts: Vec::new(),
            });
        }
        let turn = start.turn.clone();
        let opened = start.turn.clone();
        let shared = self.shared.clone();
        let emitter = Emitter {
            shared: shared.clone(),
            turn: turn.clone(),
            seq: Arc::new(AtomicU64::new(0)),
        };
        let inner = runtime.spawn(drive(shared.clone(), emitter.clone(), start, cancel_rx));
        let inner_abort = inner.abort_handle();
        let outer = runtime.spawn(async move {
            let outcome = match inner.await {
                Ok(outcome) => outcome,
                Err(_) => failed(ReasonToken::BackendError, "driver task aborted"),
            };
            // Clear the slot before the terminal event so a Start right after Terminal is accepted.
            shared.clear_open(&turn);
            emitter.terminal(outcome);
        });
        if let Some(active) = lock(&self.shared.open).as_mut()
            && active.turn == opened
        {
            active.aborts = vec![inner_abort, outer.abort_handle()];
        }
        Ok(())
    }

    fn resolve_permission(
        &mut self,
        _request: &RequestId,
        _answer: PermissionAnswer,
    ) -> Result<(), BackendError> {
        Err(BackendError::Unsupported(
            "the A2A HTTP peer has no permission flow",
        ))
    }

    fn cancel(&mut self, turn: &TurnRef) -> Result<(), BackendError> {
        if let Some(active) = lock(&self.shared.open).as_ref()
            && active.turn == *turn
        {
            let _ = active.cancel.send(true);
        }
        Ok(())
    }

    /// Kills the open turn; the backend stays startable (the Worker's `kill` expects a respawn).
    fn shutdown(&mut self, grace: Duration) -> Result<(), BackendError> {
        self.kill(grace);
        Ok(())
    }
}

impl Drop for HttpBackend {
    /// The only terminal close: the runtime goes away with the backend.
    fn drop(&mut self) {
        self.kill(Duration::ZERO);
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

impl HttpBackend {
    /// Ends the open turn: cancels it, waits up to `grace` for its own Terminal, then stops its
    /// tasks and closes the slot with a failed Terminal. The runtime and the backend stay usable,
    /// so the next `start` works (RFC-0009 [0009-036]).
    fn kill(&self, grace: Duration) {
        if let Some(active) = lock(&self.shared.open).as_ref() {
            let _ = active.cancel.send(true);
        }
        let deadline = Instant::now() + grace;
        while lock(&self.shared.open).is_some() && Instant::now() < deadline {
            std::thread::sleep(SHUTDOWN_POLL);
        }
        // Tasks still running after the grace are stopped; they never emit their Terminal.
        let leftover = lock(&self.shared.open).take();
        if let Some(active) = leftover {
            for handle in &active.aborts {
                handle.abort();
            }
            let emitter = Emitter {
                shared: self.shared.clone(),
                turn: active.turn,
                seq: Arc::new(AtomicU64::new(0)),
            };
            emitter.terminal(failed(
                ReasonToken::BackendError,
                "driver shut down before the turn finished",
            ));
        }
    }
}

/// State shared by the backend handle and its turn tasks.
struct Shared {
    settings: PeerSettings,
    http: reqwest::Client,
    sink: Mutex<Option<Box<dyn BackendSink>>>,
    open: Mutex<Option<ActiveTurn>>,
    peer: AsyncMutex<Option<(Endpoint, Instant)>>,
    ownership: Mutex<TaskOwnership>,
    live: AtomicBool,
}

impl Shared {
    fn secret_text(&self) -> Option<&str> {
        self.settings.bearer.as_ref().map(Secret::expose)
    }

    fn send(&self, event: BackendEvent) {
        if let Some(sink) = lock(&self.sink).as_mut() {
            sink.emit(event);
        }
    }

    fn clear_open(&self, turn: &TurnRef) {
        let mut open = lock(&self.open);
        if open.as_ref().is_some_and(|active| active.turn == *turn) {
            *open = None;
        }
    }

    /// Returns the current endpoint, refreshing the card when older than [`CARD_REFRESH`].
    async fn endpoint(&self) -> Endpoint {
        let mut guard = self.peer.lock().await;
        if let Some((endpoint, fetched)) = guard.as_ref()
            && fetched.elapsed() < CARD_REFRESH
        {
            return endpoint.clone();
        }
        let endpoint = self.discover().await;
        *guard = Some((endpoint.clone(), Instant::now()));
        endpoint
    }

    /// Reads the card; a missing or failed card falls back to the configured base URL (0009-041).
    async fn discover(&self) -> Endpoint {
        let base = self.settings.base_url.clone();
        let timeout = self.settings.timeout.min(CARD_FETCH_CAP);
        let (rpc_url, streaming, version_mismatch) =
            match card::fetch(&self.http, &base, timeout).await {
                Ok(document) => {
                    let selection = card::select(&document, &base);
                    let mut url = selection.rpc_url;
                    // The bearer never goes to a card-chosen URL the policy does not allow (0008-069).
                    if self.settings.bearer.is_some() && !bearer_allowed(&url) {
                        url = base.clone();
                    }
                    (
                        url,
                        selection.streaming && self.settings.streaming,
                        selection.version_mismatch,
                    )
                }
                Err(_) => (base.clone(), false, false),
            };
        Endpoint {
            transport: Arc::new(Transport::new(self.http.clone(), rpc_url, &self.settings)),
            streaming,
            version_mismatch,
        }
    }
}

/// Resolved peer endpoint for turns.
#[derive(Clone)]
struct Endpoint {
    transport: Arc<Transport>,
    streaming: bool,
    version_mismatch: bool,
}

/// The turn currently open on the backend.
struct ActiveTurn {
    turn: TurnRef,
    cancel: watch::Sender<bool>,
    /// Handles of the turn's tasks, so a kill that outlasts its grace can stop them.
    aborts: Vec<AbortHandle>,
}

/// Emits events for one turn with a monotonically increasing `event_no` (0009-006).
#[derive(Clone)]
struct Emitter {
    shared: Arc<Shared>,
    turn: TurnRef,
    seq: Arc<AtomicU64>,
}

impl Emitter {
    fn next_no(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn progress(&self, text: String) {
        let event_no = self.next_no();
        self.shared.send(BackendEvent::Status {
            turn: self.turn.clone(),
            event_no,
            payload: StatusPayload::Progress(text),
        });
    }

    fn artifact(&self, key: &str, text: String, append: bool, last: bool) {
        let event_no = self.next_no();
        self.shared.send(BackendEvent::ArtifactDelta {
            turn: self.turn.clone(),
            event_no,
            artifact_key: key.to_owned(),
            text,
            append,
            last,
        });
    }

    fn input_required(&self, text: String) {
        let event_no = self.next_no();
        self.shared.send(BackendEvent::InputRequired {
            turn: self.turn.clone(),
            event_no,
            text,
        });
    }

    fn fault(&self, kind: FaultKind, detail: &str) {
        self.shared.send(BackendEvent::Fault {
            kind,
            detail: detail.to_owned(),
        });
    }

    fn terminal(&self, outcome: Outcome) {
        let event_no = self.next_no();
        self.shared.send(BackendEvent::Terminal {
            turn: self.turn.clone(),
            event_no,
            outcome,
        });
    }
}

/// Why a phase stopped before the peer answered.
#[derive(Debug, Clone, Copy)]
enum Interrupt {
    Canceled,
    TimedOut,
}

/// Cancel and deadline guard for a turn.
struct Ctl {
    cancel: watch::Receiver<bool>,
    deadline: Instant,
}

impl Ctl {
    /// Runs `fut` unless the turn is canceled or its deadline passes first.
    async fn run<T>(&mut self, fut: impl Future<Output = T>) -> Result<T, Interrupt> {
        tokio::select! {
            biased;
            () = wait_cancel(&mut self.cancel) => Err(Interrupt::Canceled),
            () = tokio::time::sleep_until(self.deadline) => Err(Interrupt::TimedOut),
            value = fut => Ok(value),
        }
    }

    fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
}

async fn wait_cancel(rx: &mut watch::Receiver<bool>) {
    loop {
        if *rx.borrow_and_update() {
            return;
        }
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// Outcome of one phase: a final outcome, or "not terminal yet, keep waiting".
enum Flow {
    Done(Outcome),
    Ended,
}

/// Per-turn protocol state.
struct TurnCtx {
    emitter: Emitter,
    origin: String,
    peer_ctx: String,
    task_id: Option<String>,
    last_state: Option<PeerState>,
    streamed: bool,
}

impl TurnCtx {
    /// Applies one decoded update. Returns the outcome when the update ends the wait.
    fn apply(&mut self, update: Update) -> Option<Outcome> {
        self.emitter.shared.live.store(true, Ordering::SeqCst);
        match update {
            Update::Task(view) => {
                if let Some(outcome) = self.foreign(&view.context_id) {
                    return Some(outcome);
                }
                if !view.id.is_empty()
                    && let Some(outcome) = self.adopt(&view.id)
                {
                    return Some(outcome);
                }
                self.on_state(view.state, &view.status_text, &view.artifact_text)
            }
            Update::Status {
                task_id,
                context_id,
                state,
                text,
            } => {
                if let Some(outcome) = self.foreign(&context_id) {
                    return Some(outcome);
                }
                if !task_id.is_empty()
                    && let Some(outcome) = self.adopt(&task_id)
                {
                    return Some(outcome);
                }
                self.on_state(state, &text, "")
            }
            Update::Artifact {
                task_id,
                context_id,
                artifact_id,
                text,
                append,
                last,
            } => {
                if let Some(outcome) = self.foreign(&context_id) {
                    return Some(outcome);
                }
                if !task_id.is_empty()
                    && let Some(outcome) = self.adopt(&task_id)
                {
                    return Some(outcome);
                }
                if !text.is_empty() {
                    self.streamed = true;
                    let key = if artifact_id.is_empty() {
                        REPLY_KEY
                    } else {
                        &artifact_id
                    };
                    self.emitter.artifact(key, text, append, last);
                }
                None
            }
            Update::Message { context_id, text } => {
                if let Some(outcome) = self.foreign(&context_id) {
                    return Some(outcome);
                }
                if !text.is_empty() {
                    self.emitter.artifact(REPLY_KEY, text, false, true);
                }
                Some(Outcome::Completed)
            }
        }
    }

    /// Maps a peer task state to progress or to the turn outcome (RFC-0008 0008-053, 0008-087).
    fn on_state(&mut self, state: PeerState, text: &str, artifact_text: &str) -> Option<Outcome> {
        match state {
            PeerState::Submitted | PeerState::Working => {
                if self.last_state.as_ref() != Some(&state) {
                    let label = if state == PeerState::Working {
                        "working"
                    } else {
                        "submitted"
                    };
                    self.emitter.progress(format!("peer task {label}"));
                }
                self.last_state = Some(state);
                None
            }
            PeerState::Completed => {
                if !self.streamed {
                    let body = if artifact_text.is_empty() {
                        text
                    } else {
                        artifact_text
                    };
                    if !body.is_empty() {
                        self.emitter
                            .artifact(REPLY_KEY, body.to_owned(), false, true);
                    }
                }
                Some(Outcome::Completed)
            }
            PeerState::Canceled => Some(Outcome::Canceled),
            PeerState::Rejected => Some(Outcome::Refused {
                reason: ReasonToken::AgentRefused,
                health_class: None,
            }),
            PeerState::InputRequired => {
                let question = if text.is_empty() { artifact_text } else { text };
                self.emitter
                    .input_required(redact(question, self.emitter.shared.secret_text()));
                // Input is answered by a new turn on the same contextId (0008-094).
                Some(Outcome::Completed)
            }
            PeerState::AuthRequired => Some(Outcome::Failed {
                reason: ReasonToken::BackendError,
                health_class: Some(HealthClass::PeerAuthFailed),
                detail: "peer requires authentication".to_owned(),
            }),
            PeerState::Failed => {
                if text.contains(REPLY_TIMEOUT_MARKER) {
                    Some(failed(
                        ReasonToken::BackendTimeout,
                        "peer did not reply in time",
                    ))
                } else {
                    let detail = if text.is_empty() {
                        "peer task failed"
                    } else {
                        text
                    };
                    Some(failed(
                        ReasonToken::BackendError,
                        &redact(detail, self.emitter.shared.secret_text()),
                    ))
                }
            }
            PeerState::Unknown(raw) => Some(failed(
                ReasonToken::BackendError,
                &redact(
                    &format!("unknown peer state {raw}"),
                    self.emitter.shared.secret_text(),
                ),
            )),
        }
    }

    /// Rejects a reply that names a foreign contextId (0009-046).
    fn foreign(&self, context_id: &str) -> Option<Outcome> {
        if context_id.is_empty() || context_id == self.peer_ctx {
            return None;
        }
        self.emitter
            .fault(FaultKind::Internal, "peer contextId mismatch");
        Some(failed(
            ReasonToken::BackendError,
            "peer returned a foreign contextId",
        ))
    }

    /// Adopts a peer task id from a reply for this turn. The id is recorded in the origin's
    /// ownership map before any lookup may use it (0009-077).
    fn adopt(&mut self, task_id: &str) -> Option<Outcome> {
        if self.task_id.as_deref() == Some(task_id) {
            return None;
        }
        let recorded = lock(&self.emitter.shared.ownership).record(&self.origin, task_id);
        match recorded {
            Ok(()) => {
                self.task_id = Some(task_id.to_owned());
                None
            }
            Err(TaskMapError::Foreign) => {
                self.emitter.fault(
                    FaultKind::Internal,
                    "peer task id belongs to another origin",
                );
                Some(failed(
                    ReasonToken::BackendError,
                    "peer returned a task id owned by another origin",
                ))
            }
            Err(_) => {
                self.emitter
                    .fault(FaultKind::Internal, "task ownership map unavailable");
                Some(failed(
                    ReasonToken::BackendError,
                    "cannot record peer task ownership",
                ))
            }
        }
    }

    /// Returns whether this origin created `task_id` through this driver (0009-077).
    fn owns(&self, task_id: &str) -> bool {
        lock(&self.emitter.shared.ownership).owns(&self.origin, task_id)
    }

    /// The turn's task id when this origin owns it; the only id lookups and cancels may use.
    fn owned_task_id(&self) -> Option<String> {
        self.task_id
            .as_deref()
            .filter(|id| self.owns(id))
            .map(str::to_owned)
    }

    /// Maps an interrupt to its outcome, cancelling the remote task when this origin owns it.
    async fn interrupted(&self, interrupt: Interrupt, transport: Option<&Transport>) -> Outcome {
        if let (Some(transport), Some(task_id)) = (transport, self.owned_task_id()) {
            cancel_remote(transport, &task_id).await;
        }
        outcome_of(interrupt)
    }

    /// Maps a transport or RPC failure to an outcome (RFC-0008 0008-078).
    fn fail(&self, error: &CallError, lookup: bool) -> Outcome {
        let secret = self.emitter.shared.secret_text();
        match error {
            CallError::Unreachable => {
                self.emitter.shared.live.store(false, Ordering::SeqCst);
                failed(ReasonToken::BackendCrashed, "peer unreachable")
            }
            CallError::Timeout => failed(ReasonToken::BackendTimeout, "peer timed out"),
            CallError::Malformed(malformed) => failed(
                ReasonToken::BackendError,
                &redact(&malformed.to_string(), secret),
            ),
            CallError::Status { status, rpc } => {
                let class = classify_status(*status)
                    .or_else(|| rpc.as_ref().map(|e| classify_code(e.code, lookup)))
                    .unwrap_or(FailClass::Other);
                let message = rpc.as_ref().map(|e| e.message.as_str());
                by_class(
                    class,
                    message,
                    &format!("peer returned HTTP {status}"),
                    secret,
                )
            }
            CallError::Rpc(error) => by_class(
                classify_code(error.code, lookup),
                Some(&error.message),
                "peer returned an error",
                secret,
            ),
        }
    }

    fn malformed(&self, malformed: Malformed) -> Outcome {
        self.fail(&CallError::Malformed(malformed), false)
    }
}

fn by_class(
    class: FailClass,
    message: Option<&str>,
    fallback: &str,
    secret: Option<&str>,
) -> Outcome {
    match class {
        FailClass::Auth => Outcome::Failed {
            reason: ReasonToken::BackendError,
            health_class: Some(HealthClass::PeerAuthFailed),
            detail: "peer rejected credentials".to_owned(),
        },
        FailClass::RateLimited => Outcome::Refused {
            reason: ReasonToken::RateLimited,
            health_class: Some(HealthClass::PeerRateLimited),
        },
        FailClass::LostTask => failed(
            ReasonToken::BackendLostTask,
            "peer no longer knows the task",
        ),
        FailClass::Other => {
            let text = message.filter(|m| !m.is_empty()).unwrap_or(fallback);
            failed(ReasonToken::BackendError, &redact(text, secret))
        }
    }
}

fn failed(reason: ReasonToken, detail: &str) -> Outcome {
    Outcome::Failed {
        reason,
        health_class: None,
        detail: detail.to_owned(),
    }
}

fn join_input(parts: &[InputPart]) -> String {
    parts
        .iter()
        .map(|InputPart::Text(text)| text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn send_params(text: &str, peer_ctx: &str) -> Value {
    serde_json::json!({
        "message": {
            "role": "ROLE_USER",
            "parts": [{"text": text, "mediaType": "text/plain"}],
            "messageId": namespace::message_id(),
            "contextId": peer_ctx,
        }
    })
}

/// Runs `attempt`, retrying only connection failures with bounded backoff (0009-045).
async fn with_retry<T, F, Fut>(mut attempt: F) -> Result<T, CallError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, CallError>>,
{
    let mut index = 0;
    loop {
        match attempt().await {
            Err(CallError::Unreachable) if index < RETRY_DELAYS.len() => {
                tokio::time::sleep(RETRY_DELAYS[index]).await;
                index += 1;
            }
            other => return other,
        }
    }
}

/// Best-effort CancelTask for a known task. Failures are ignored (0009-047).
async fn cancel_remote(transport: &Transport, task_id: &str) {
    let _ = tokio::time::timeout(
        CANCEL_BOUND,
        transport.call(CANCEL_TASK, task_params(task_id)),
    )
    .await;
}

/// Maps an interrupt to its turn outcome.
fn outcome_of(interrupt: Interrupt) -> Outcome {
    match interrupt {
        Interrupt::Canceled => Outcome::Canceled,
        Interrupt::TimedOut => failed(ReasonToken::BackendTimeout, "turn timed out"),
    }
}

/// Drives one turn to its outcome. Emits progress, artifacts, and input requests on the way.
async fn drive(
    shared: Arc<Shared>,
    emitter: Emitter,
    start: StartTurn,
    cancel: watch::Receiver<bool>,
) -> Outcome {
    if start.origin.is_empty() {
        return failed(ReasonToken::BackendError, "turn origin is empty");
    }
    let mut ctl = Ctl {
        cancel,
        deadline: Instant::now() + shared.settings.timeout,
    };
    let endpoint = match ctl.run(shared.endpoint()).await {
        Ok(endpoint) => endpoint,
        Err(interrupt) => return outcome_of(interrupt),
    };
    if endpoint.version_mismatch {
        emitter.fault(
            FaultKind::UnsupportedProtocolVersion,
            "EXTERNAL_VERSION_MISMATCH",
        );
    }
    // The namespacing key is per origin (0009-076); outbound text is scrubbed (0009-044).
    let peer_ctx = namespace::peer_context_id(
        shared.settings.context_key.expose().as_bytes(),
        &start.origin,
        &start.context_id,
    );
    let text = scrub(&join_input(&start.input), shared.secret_text());
    let params = send_params(&text, &peer_ctx);
    let transport = endpoint.transport.clone();
    let mut ctx = TurnCtx {
        emitter,
        origin: start.origin,
        peer_ctx,
        task_id: None,
        last_state: None,
        streamed: false,
    };

    let first = if endpoint.streaming {
        stream(
            &mut ctx,
            &transport,
            &mut ctl,
            SEND_STREAMING,
            params,
            false,
        )
        .await
    } else {
        unary(&mut ctx, &transport, &mut ctl, params).await
    };
    let flow = match first {
        Flow::Ended if endpoint.streaming => match ctx.task_id.clone() {
            Some(task_id) if ctx.owns(&task_id) => {
                stream(
                    &mut ctx,
                    &transport,
                    &mut ctl,
                    SUBSCRIBE_TASK,
                    task_params(&task_id),
                    true,
                )
                .await
            }
            Some(_) => Flow::Done(failed(
                ReasonToken::BackendLostTask,
                "peer task is not owned by this origin",
            )),
            None => Flow::Ended,
        },
        other => other,
    };
    match flow {
        Flow::Done(outcome) => outcome,
        Flow::Ended => poll(&mut ctx, &transport, &mut ctl).await,
    }
}

/// Non-streaming SendMessage (0009-043).
async fn unary(ctx: &mut TurnCtx, transport: &Transport, ctl: &mut Ctl, params: Value) -> Flow {
    let reply = match ctl
        .run(with_retry(|| transport.call(SEND_MESSAGE, params.clone())))
        .await
    {
        Ok(reply) => reply,
        Err(interrupt) => {
            return Flow::Done(ctx.interrupted(interrupt, Some(transport)).await);
        }
    };
    match reply {
        Ok(value) => match rpc::parse_result(&value) {
            Ok(update) => match ctx.apply(update) {
                Some(outcome) => Flow::Done(outcome),
                None => Flow::Ended,
            },
            Err(malformed) => Flow::Done(ctx.malformed(malformed)),
        },
        Err(error) => Flow::Done(ctx.fail(&error, false)),
    }
}

/// Streaming call (SendStreamingMessage or SubscribeToTask). `lookup` marks SubscribeToTask,
/// where a lost task maps to BACKEND_LOST_TASK and transport failures end the phase quietly.
async fn stream(
    ctx: &mut TurnCtx,
    transport: &Transport,
    ctl: &mut Ctl,
    method: &str,
    params: Value,
    lookup: bool,
) -> Flow {
    let opened = match ctl
        .run(with_retry(|| transport.open_stream(method, params.clone())))
        .await
    {
        Ok(opened) => opened,
        Err(interrupt) => {
            return Flow::Done(ctx.interrupted(interrupt, Some(transport)).await);
        }
    };
    let mut events: EventStream = match opened {
        Ok(events) => events,
        Err(CallError::Unreachable | CallError::Timeout) if lookup => return Flow::Ended,
        Err(error) => return Flow::Done(ctx.fail(&error, lookup)),
    };
    loop {
        let payload = match ctl.run(events.next_payload()).await {
            Ok(Ok(Some(payload))) => payload,
            Ok(Ok(None)) | Ok(Err(CallError::Timeout | CallError::Unreachable)) => {
                return Flow::Ended;
            }
            Ok(Err(error)) => return Flow::Done(ctx.fail(&error, lookup)),
            Err(interrupt) => {
                return Flow::Done(ctx.interrupted(interrupt, Some(transport)).await);
            }
        };
        match rpc::parse_envelope(payload.as_bytes()) {
            Ok(Envelope::Result(value)) => match rpc::parse_result(&value) {
                Ok(update) => {
                    if let Some(outcome) = ctx.apply(update) {
                        return Flow::Done(outcome);
                    }
                }
                Err(malformed) => return Flow::Done(ctx.malformed(malformed)),
            },
            Ok(Envelope::Error(error)) => {
                return Flow::Done(ctx.fail(&CallError::Rpc(error), lookup));
            }
            Err(malformed) => return Flow::Done(ctx.malformed(malformed)),
        }
    }
}

/// Polls GetTask with 2 s doubling to 10 s until a waiting or terminal state (0009-049).
async fn poll(ctx: &mut TurnCtx, transport: &Transport, ctl: &mut Ctl) -> Outcome {
    let Some(task_id) = ctx.task_id.clone() else {
        return failed(ReasonToken::BackendError, "peer returned no task id");
    };
    if !ctx.owns(&task_id) {
        return failed(
            ReasonToken::BackendLostTask,
            "peer task is not owned by this origin",
        );
    }
    let mut interval = POLL_MIN;
    let mut down = 0_u32;
    loop {
        let wait = interval.min(ctl.remaining());
        if let Err(interrupt) = ctl.run(tokio::time::sleep(wait)).await {
            return ctx.interrupted(interrupt, Some(transport)).await;
        }
        let reply = match ctl
            .run(transport.call(GET_TASK, task_params(&task_id)))
            .await
        {
            Ok(reply) => reply,
            Err(interrupt) => return ctx.interrupted(interrupt, Some(transport)).await,
        };
        match reply {
            Ok(value) => {
                down = 0;
                match rpc::parse_result(&value) {
                    Ok(update) => {
                        if let Some(outcome) = ctx.apply(update) {
                            return outcome;
                        }
                    }
                    Err(malformed) => return ctx.malformed(malformed),
                }
                interval = (interval * 2).min(POLL_MAX);
            }
            Err(CallError::Unreachable) => {
                down += 1;
                ctx.emitter.shared.live.store(false, Ordering::SeqCst);
                if down >= DOWN_LIMIT {
                    return failed(ReasonToken::BackendCrashed, "peer unreachable");
                }
            }
            Err(error) => return ctx.fail(&error, true),
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// 0009-045: a connection failure is resent and the resend reuses the original messageId.
    #[tokio::test]
    async fn req_0009_045_unreachable_resend_reuses_message_id() {
        let params = send_params("hello", "ctx-peer");
        let message_id = params["message"]["messageId"].clone();
        let mut attempts = 0;
        let mut seen = Vec::new();
        let out: Result<u8, CallError> = with_retry(|| {
            attempts += 1;
            seen.push(params.clone());
            let n = attempts;
            async move {
                if n < 3 {
                    Err(CallError::Unreachable)
                } else {
                    Ok(7)
                }
            }
        })
        .await;
        assert_eq!(out.unwrap(), 7);
        assert_eq!(attempts, 3);
        assert!(seen.iter().all(|p| p["message"]["messageId"] == message_id));
    }

    /// 0009-045: retries are bounded; the last connection failure is returned.
    #[tokio::test]
    async fn req_0009_045_retries_are_bounded() {
        let mut attempts = 0;
        let out: Result<u8, CallError> = with_retry(|| {
            attempts += 1;
            async { Err(CallError::Unreachable) }
        })
        .await;
        assert!(matches!(out, Err(CallError::Unreachable)));
        assert_eq!(attempts, RETRY_DELAYS.len() + 1);
    }

    #[tokio::test]
    async fn timeout_is_not_retried() {
        let mut attempts = 0;
        let out: Result<u8, CallError> = with_retry(|| {
            attempts += 1;
            async { Err(CallError::Timeout) }
        })
        .await;
        assert!(matches!(out, Err(CallError::Timeout)));
        assert_eq!(attempts, 1);
    }
}
