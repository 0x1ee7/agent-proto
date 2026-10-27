//! The actor: one thread owns all driver state (RFC-0009 4.9, 4.10, 0009-059 to 0009-062).
//!
//! Commands from the [`crate::AcpDriver`] handle and frames from the agent's
//! stdout reader arrive on one channel. Deadlines (turn idle, cancel grace,
//! permission timeout, restart backoff) are handled with `recv_timeout`.

use std::collections::{HashMap, VecDeque};
use std::io::BufReader;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use a2a_worker::backend::{
    BackendError, BackendEvent, BackendSink, FaultKind, Health, InputPart, Outcome, PeerTools,
    PermissionAnswer, PermissionOption, PermissionRequest, ReasonToken, RequestId, SessionHandle,
    StartTurn, StatusPayload, TurnRef, UsageSample, UsageSource,
};
use serde_json::{Value, json};

use crate::acp::{self, Elicitation, ServerCaps, Translated};
use crate::config::{AcpDriverConfig, WORKER_APPROVAL_EXPIRY};
use crate::frame;
use crate::mode;
use crate::process::{Spawner, Terminator};
use crate::redact::{detail, redact};

/// Window over which unexpected exits are counted (0009-061).
const EXIT_WINDOW: Duration = Duration::from_secs(600);
/// How long an unexpected exit waits for the reaped status before the tree is killed (0009-023).
const EXIT_OBSERVE: Duration = Duration::from_millis(500);
/// Unexpected exits inside [`EXIT_WINDOW`] that mark the driver unhealthy (0009-061).
const UNHEALTHY_EXITS: usize = 5;
/// Ceiling for restart backoff (0009-061).
const MAX_RESTART_DELAY: Duration = Duration::from_secs(60);
/// Artifact key for agent reply text.
const REPLY_ARTIFACT: &str = "reply";

/// Commands from the public handle. Replies, when present, are sent back synchronously.
pub(crate) enum Command {
    Attach(Box<dyn BackendSink>),
    Start(Box<StartTurn>, Sender<Result<(), BackendError>>),
    Resolve(RequestId, PermissionAnswer),
    Cancel(TurnRef, Sender<Result<(), BackendError>>),
    Health(Sender<Health>),
    Shutdown(Duration, Sender<()>),
}

/// Everything the actor receives.
pub(crate) enum Msg {
    Command(Command),
    /// One parsed frame from the stdout reader of generation `generation`.
    Frame {
        generation: u64,
        value: Value,
    },
    /// The stdout reader of generation `generation` reached EOF or failed.
    Closed {
        generation: u64,
    },
}

/// Returns the delay before respawn after the `n`th unexpected exit inside the window:
/// 1s, 2s, 4s, ... capped at 60s (0009-061).
pub(crate) fn restart_delay(exits_in_window: usize) -> Duration {
    let exponent = exits_in_window.saturating_sub(1).min(6) as u32;
    let secs = (1u64 << exponent).min(MAX_RESTART_DELAY.as_secs());
    Duration::from_secs(secs)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InitState {
    Down,
    Initializing,
    Ready,
}

/// Where the current turn is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Waiting for the agent process to be (re)spawned.
    AwaitSpawn,
    /// Process spawned; `initialize` in flight.
    AwaitInit,
    /// Session creation or restore in flight; agent replay is dropped.
    AwaitSession,
    /// `session/prompt` is open.
    Prompting,
    /// `session/cancel` sent; waiting for the prompt to end or the grace to expire.
    Canceling,
}

struct OpenTool {
    kind: String,
    title: String,
}

struct Turn {
    generation: u64,
    turn: TurnRef,
    context_id: String,
    restore: Option<SessionHandle>,
    text: String,
    phase: Phase,
    session_id: Option<String>,
    seq: u64,
    started: Instant,
    last_activity: Instant,
    text_emitted: bool,
    override_outcome: Option<Outcome>,
    open_tools: HashMap<String, OpenTool>,
    grace_deadline: Option<Instant>,
    peer_tools: Option<PeerTools>,
}

/// A JSON-RPC request we sent, awaiting its response.
enum Rpc {
    Initialize,
    NewSession {
        turn_gen: u64,
        context_id: String,
        peer_tools: Option<PeerTools>,
    },
    Restore {
        turn_gen: u64,
        context_id: String,
        session_id: String,
        peer_tools: Option<PeerTools>,
    },
    Prompt {
        turn_gen: u64,
    },
}

enum PendingKind {
    Permission { offered: Vec<String> },
    Elicitation { allow_label: Option<&'static str> },
}

/// An agent request awaiting an operator answer.
struct Pending {
    rpc_id: Value,
    turn_gen: u64,
    deadline: Instant,
    kind: PendingKind,
}

/// One live agent process generation.
struct Conn {
    generation: u64,
    stdin: Option<Box<dyn std::io::Write + Send>>,
    terminator: Box<dyn Terminator>,
    stderr_tail: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
}

pub(crate) struct Core {
    config: AcpDriverConfig,
    spawner: Box<dyn Spawner>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    sink: Option<Box<dyn BackendSink>>,
    generation: u64,
    conn: Option<Conn>,
    init: InitState,
    caps: ServerCaps,
    failed: bool,
    unhealthy: bool,
    closed: bool,
    /// Why the agent is not live, recorded at each flip (surfaced as `Health.reason`).
    down_reason: Option<String>,
    /// Last health flags logged, so only changes reach the log.
    live_logged: bool,
    /// Kind and method of the last inbound frame, for health logs (no params).
    last_seen: String,
    spawned_once: bool,
    restarts: u32,
    exits: VecDeque<Instant>,
    respawn_at: Option<Instant>,
    sessions: HashMap<String, String>,
    /// Peer-tools binding each session was created with (RFC-0015 [0015-084]), keyed by context.
    bindings: HashMap<String, Option<PeerTools>>,
    turn: Option<Turn>,
    turn_gen: u64,
    rpc: HashMap<i64, Rpc>,
    next_rpc: i64,
    perms: HashMap<String, Pending>,
    next_request: u64,
    /// Distinguishes this driver's request ids from earlier processes: the task store persists
    /// open approvals across restarts, so a bare `perm-N` counter collides with them.
    request_epoch: u128,
}

impl Core {
    pub(crate) fn new(
        config: AcpDriverConfig,
        spawner: Box<dyn Spawner>,
        tx: Sender<Msg>,
        rx: Receiver<Msg>,
    ) -> Self {
        Self {
            config,
            spawner,
            tx,
            rx,
            sink: None,
            generation: 0,
            conn: None,
            init: InitState::Down,
            caps: ServerCaps {
                load_session: false,
                resume: false,
            },
            failed: false,
            unhealthy: false,
            closed: false,
            down_reason: None,
            live_logged: false,
            last_seen: "none".to_owned(),
            spawned_once: false,
            restarts: 0,
            exits: VecDeque::new(),
            respawn_at: None,
            sessions: HashMap::new(),
            bindings: HashMap::new(),
            turn: None,
            turn_gen: 0,
            rpc: HashMap::new(),
            next_rpc: 1,
            perms: HashMap::new(),
            next_request: 1,
            request_epoch: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        }
    }

    /// Runs until a `Shutdown` command arrives or every sender is gone.
    pub(crate) fn run(mut self) {
        loop {
            let msg = match self.next_wakeup() {
                Some(at) => match self
                    .rx
                    .recv_timeout(at.saturating_duration_since(Instant::now()))
                {
                    Ok(msg) => Some(msg),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                },
                None => match self.rx.recv() {
                    Ok(msg) => Some(msg),
                    Err(_) => break,
                },
            };
            if let Some(msg) = msg {
                let keep_running = match msg {
                    Msg::Command(command) => self.command(command),
                    Msg::Frame { generation, value } => {
                        if generation == self.generation {
                            self.frame(value);
                        }
                        true
                    }
                    Msg::Closed { generation } => {
                        if self
                            .conn
                            .as_ref()
                            .is_some_and(|c| c.generation == generation)
                        {
                            self.exited("agent stdout closed");
                        }
                        true
                    }
                };
                if !keep_running {
                    break;
                }
            }
            self.tick();
            self.observe_health();
        }
    }

    fn command(&mut self, command: Command) -> bool {
        match command {
            Command::Attach(sink) => self.sink = Some(sink),
            Command::Start(turn, reply) => {
                let result = self.start(*turn);
                let _ = reply.send(result);
            }
            Command::Resolve(id, answer) => {
                if self.perms.contains_key(id.as_str()) {
                    self.resolve_pending(id.as_str(), answer);
                }
            }
            Command::Cancel(turn, reply) => {
                let result = self.cancel(&turn);
                let _ = reply.send(result);
            }
            Command::Health(reply) => {
                let _ = reply.send(self.health());
            }
            Command::Shutdown(grace, reply) => {
                self.shutdown(grace);
                let _ = reply.send(());
                return false;
            }
        }
        true
    }

    fn health(&self) -> Health {
        let live = self.conn.is_some()
            && self.init == InitState::Ready
            && !self.failed
            && !self.unhealthy
            && !self.closed;
        let reason = (!live).then(|| {
            self.down_reason.clone().unwrap_or_else(|| {
                if self.conn.is_none() {
                    "agent process is not running".to_owned()
                } else {
                    "agent is starting".to_owned()
                }
            })
        });
        Health {
            live,
            restarts: self.restarts,
            reason,
        }
    }

    fn emit(&mut self, event: BackendEvent) {
        if let Some(sink) = self.sink.as_mut() {
            sink.emit(event);
        }
    }

    /// Emits a turn-scoped event, numbering it from the turn's counter (0009-006).
    fn turn_event(&mut self, make: impl FnOnce(TurnRef, u64) -> BackendEvent) {
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        turn.seq += 1;
        let (turn_ref, seq) = (turn.turn.clone(), turn.seq);
        let event = make(turn_ref, seq);
        self.emit(event);
    }

    fn phase(&self) -> Option<Phase> {
        self.turn.as_ref().map(|t| t.phase)
    }

    fn set_phase(&mut self, phase: Phase) {
        if let Some(turn) = self.turn.as_mut() {
            turn.phase = phase;
        }
    }

    /// Logs each change of driver liveness with the flags behind it (target `driver_acp`).
    fn observe_health(&mut self) {
        let health = self.health();
        if health.live == self.live_logged {
            return;
        }
        self.live_logged = health.live;
        let reason = health.reason.as_deref().map(redact).unwrap_or_default();
        tracing::info!(
            target: "driver_acp",
            live = health.live,
            conn = self.conn.is_some(),
            init = ?self.init,
            failed = self.failed,
            unhealthy = self.unhealthy,
            closed = self.closed,
            restarts = self.restarts,
            reason = %reason,
            last_seen = %self.last_seen,
            "agent health changed"
        );
    }

    fn touch(&mut self) {
        if let Some(turn) = self.turn.as_mut() {
            turn.last_activity = Instant::now();
        }
    }

    /// Override set by a driver-side timeout takes precedence over the fallback outcome.
    fn take_outcome(&mut self, fallback: Outcome) -> Outcome {
        self.turn
            .as_mut()
            .and_then(|t| t.override_outcome.take())
            .unwrap_or(fallback)
    }

    fn start(&mut self, cmd: StartTurn) -> Result<(), BackendError> {
        if self.closed || self.failed || self.unhealthy {
            return Err(BackendError::Closed);
        }
        if self.turn.is_some() {
            return Err(BackendError::Busy);
        }
        if let Err(reason) = mode::check_start(
            self.config.profile,
            &self.config.approval_mode,
            self.config.acknowledge_dangerous,
        ) {
            self.emit(BackendEvent::Fault {
                kind: FaultKind::ConfigRefused,
                detail: reason.to_owned(),
            });
            return Err(BackendError::Unsupported(reason));
        }
        let bound = self.bindings.get(&cmd.context_id);
        let peer_refusal =
            acp::peer_tools_refusal(&self.config.mcp_servers, cmd.peer_tools.as_ref()).or_else(
                || {
                    (bound.is_some_and(|b| *b != cmd.peer_tools))
                        .then_some("peer tools binding changed for an existing session")
                },
            );
        if let Some(reason) = peer_refusal {
            self.emit(BackendEvent::Fault {
                kind: FaultKind::ConfigRefused,
                detail: reason.to_owned(),
            });
            return Err(BackendError::Unsupported(reason));
        }
        let text = cmd
            .input
            .iter()
            .map(|part| match part {
                InputPart::Text(text) => text.as_str(),
            })
            .collect::<Vec<_>>()
            .join("\n");
        self.turn_gen += 1;
        let now = Instant::now();
        self.turn = Some(Turn {
            generation: self.turn_gen,
            turn: cmd.turn,
            context_id: cmd.context_id,
            restore: cmd.session,
            text,
            phase: Phase::AwaitSpawn,
            session_id: None,
            seq: 0,
            started: now,
            last_activity: now,
            text_emitted: false,
            override_outcome: None,
            open_tools: HashMap::new(),
            grace_deadline: None,
            peer_tools: cmd.peer_tools,
        });
        self.advance();
        Ok(())
    }

    /// Starts the agent while no turn is open, so backend health is known before work arrives
    /// (0012-009, 0004-015). An idle exit respawns after the backoff (0009-061). Nothing starts
    /// while `start` would refuse the approval mode (0009-064).
    fn ensure_process(&mut self) {
        if self.conn.is_some() || self.closed || self.failed || self.unhealthy {
            return;
        }
        if self.respawn_at.is_some_and(|at| Instant::now() < at) {
            return;
        }
        if let Err(reason) = mode::check_start(
            self.config.profile,
            &self.config.approval_mode,
            self.config.acknowledge_dangerous,
        ) {
            if self.down_reason.as_deref() != Some(reason) {
                tracing::warn!(target: "driver_acp", reason, "agent not started: configuration refused");
                self.down_reason = Some(reason.to_owned());
            }
            return;
        }
        if let Err(error) = self.spawn_process() {
            self.down_reason = Some(format!("agent could not be spawned: {error}"));
            self.note_exit();
        }
    }

    /// Moves the current turn forward as far as the process state allows.
    fn advance(&mut self) {
        if self.turn.is_none() {
            return;
        }
        if self.conn.is_none() {
            if self.respawn_at.is_some_and(|at| Instant::now() < at) {
                return;
            }
            if let Err(error) = self.spawn_process() {
                self.down_reason = Some(format!("agent could not be spawned: {error}"));
                self.note_exit();
                let outcome = self.take_outcome(Outcome::Failed {
                    reason: ReasonToken::BackendUnavailable,
                    health_class: None,
                    detail: detail(&format!("agent could not be spawned: {error}")),
                });
                self.finish(outcome);
            }
            return;
        }
        if self.init != InitState::Ready {
            self.set_phase(Phase::AwaitInit);
            return;
        }
        if matches!(self.phase(), Some(Phase::AwaitSpawn | Phase::AwaitInit)) {
            self.start_session();
        }
    }

    fn spawn_process(&mut self) -> Result<(), String> {
        let spec = self.config.launch_spec();
        let spawned = self.spawner.spawn(&spec).map_err(|e| e.to_string())?;
        self.generation += 1;
        self.down_reason = None;
        let generation = self.generation;
        let stdout = spawned.stdout;
        let tx = self.tx.clone();
        thread::spawn(move || {
            let _ = frame::read_frames(BufReader::new(stdout), |value| {
                let _ = tx.send(Msg::Frame { generation, value });
            });
            let _ = tx.send(Msg::Closed { generation });
        });
        self.conn = Some(Conn {
            generation,
            stdin: Some(spawned.stdin),
            terminator: spawned.terminator,
            stderr_tail: spawned.stderr_tail,
        });
        if self.spawned_once {
            self.restarts += 1;
        }
        self.spawned_once = true;
        self.respawn_at = None;
        self.init = InitState::Initializing;
        self.set_phase(Phase::AwaitInit);
        self.send_rpc(Rpc::Initialize, "initialize", acp::initialize_params());
        Ok(())
    }

    /// Chooses the session path: existing, restored (load or resume), or new (0009-012, 0009-013).
    fn start_session(&mut self) {
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        turn.phase = Phase::AwaitSession;
        let generation = turn.generation;
        let context_id = turn.context_id.clone();
        let restore = turn.restore.as_ref().map(|h| h.as_str().to_owned());
        if let Some(sid) = self.sessions.get(&context_id).cloned() {
            if let Some(turn) = self.turn.as_mut() {
                turn.session_id = Some(sid);
            }
            self.send_prompt();
            return;
        }
        if let Some(sid) = restore {
            let (method, reachable) = if self.caps.load_session {
                ("session/load", true)
            } else if self.caps.resume {
                ("session/resume", true)
            } else {
                ("", false)
            };
            if reachable {
                let peer_tools = self.turn.as_ref().and_then(|t| t.peer_tools.clone());
                let servers = acp::mcp_servers(&self.config.mcp_servers, peer_tools.as_ref());
                let params = acp::session_params(&self.config.workspace, &servers, Some(&sid));
                self.send_rpc(
                    Rpc::Restore {
                        turn_gen: generation,
                        context_id,
                        session_id: sid,
                        peer_tools,
                    },
                    method,
                    params,
                );
                return;
            }
            self.turn_notice("agent cannot restore sessions; started a new session");
        }
        self.send_new_session();
    }

    fn send_new_session(&mut self) {
        let Some(turn) = self.turn.as_ref() else {
            return;
        };
        let generation = turn.generation;
        let context_id = turn.context_id.clone();
        let peer_tools = turn.peer_tools.clone();
        let servers = acp::mcp_servers(&self.config.mcp_servers, peer_tools.as_ref());
        let params = acp::session_params(&self.config.workspace, &servers, None);
        self.send_rpc(
            Rpc::NewSession {
                turn_gen: generation,
                context_id,
                peer_tools,
            },
            "session/new",
            params,
        );
    }

    fn send_prompt(&mut self) {
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        turn.phase = Phase::Prompting;
        turn.last_activity = Instant::now();
        let generation = turn.generation;
        let Some(sid) = turn.session_id.clone() else {
            return;
        };
        let text = turn.text.clone();
        self.send_rpc(
            Rpc::Prompt {
                turn_gen: generation,
            },
            "session/prompt",
            acp::prompt_params(&sid, &text),
        );
    }

    fn bind_session(&mut self, context_id: &str, session_id: &str) {
        self.sessions
            .insert(context_id.to_owned(), session_id.to_owned());
        if let Some(turn) = self.turn.as_mut() {
            turn.session_id = Some(session_id.to_owned());
        }
        let handle = SessionHandle::new(session_id);
        self.turn_event(|turn, event_no| BackendEvent::Status {
            turn,
            event_no,
            payload: StatusPayload::SessionBound(handle),
        });
    }

    fn turn_notice(&mut self, text: &str) {
        let text = text.to_owned();
        self.turn_event(|turn, event_no| BackendEvent::Status {
            turn,
            event_no,
            payload: StatusPayload::Notice(text),
        });
    }

    fn send_rpc(&mut self, rpc: Rpc, method: &str, params: Value) -> bool {
        let id = self.next_rpc;
        self.next_rpc += 1;
        self.rpc.insert(id, rpc);
        self.write(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
    }

    fn reply(&mut self, id: Value, result: Value) -> bool {
        self.write(json!({"jsonrpc": "2.0", "id": id, "result": result}))
    }

    fn reply_error(&mut self, id: Value, code: i64, message: &str) -> bool {
        self.write(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}))
    }

    /// Writes one frame to the agent's stdin. A failed write means the process is gone.
    fn write(&mut self, value: Value) -> bool {
        let result = match self.conn.as_mut().and_then(|c| c.stdin.as_mut()) {
            Some(stdin) => frame::write_frame(stdin, &value),
            None => return false,
        };
        match result {
            Ok(()) => true,
            Err(error) => {
                self.exited(&format!("stdin write failed: {error}"));
                false
            }
        }
    }

    fn frame(&mut self, value: Value) {
        self.touch();
        let method = value
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let id = value.get("id").cloned();
        let kind = match (method.is_some(), id.is_some()) {
            (true, true) => "request",
            (true, false) => "notification",
            _ => "response",
        };
        self.last_seen = format!(
            "{kind} {}",
            method
                .as_deref()
                .unwrap_or("-")
                .chars()
                .take(64)
                .collect::<String>()
        );
        match (method, id) {
            (Some(method), Some(id)) => {
                let params = value.get("params").cloned().unwrap_or(Value::Null);
                self.request(id, &method, params);
            }
            (Some(method), None) => {
                let params = value.get("params").cloned().unwrap_or(Value::Null);
                self.notification(&method, params);
            }
            (None, Some(id)) => {
                let outcome = match (value.get("error"), value.get("result")) {
                    (Some(error), _) => Err(error.clone()),
                    (None, result) => Ok(result.cloned().unwrap_or(Value::Null)),
                };
                self.response(id, outcome);
            }
            (None, None) => {}
        }
    }

    fn response(&mut self, id: Value, outcome: Result<Value, Value>) {
        let Some(rpc_id) = id.as_i64() else {
            return;
        };
        let Some(rpc) = self.rpc.remove(&rpc_id) else {
            return;
        };
        match rpc {
            Rpc::Initialize => self.on_initialize(outcome),
            Rpc::NewSession {
                turn_gen,
                context_id,
                peer_tools,
            } => self.on_new_session(turn_gen, &context_id, peer_tools, outcome),
            Rpc::Restore {
                turn_gen,
                context_id,
                session_id,
                peer_tools,
            } => self.on_restore(turn_gen, &context_id, &session_id, peer_tools, outcome),
            Rpc::Prompt { turn_gen } => self.on_prompt(turn_gen, outcome),
        }
    }

    fn on_initialize(&mut self, outcome: Result<Value, Value>) {
        let init = match outcome {
            Ok(init) => init,
            Err(error) => {
                let text = format!("initialize failed: {}", acp::error_detail(&error));
                self.fail_startup(text);
                return;
            }
        };
        let version = acp::protocol_version(&init);
        if version != Some(1) {
            self.failed = true;
            self.emit(BackendEvent::Fault {
                kind: FaultKind::UnsupportedProtocolVersion,
                detail: detail(&format!(
                    "agent answered protocol version {version:?}; expected 1"
                )),
            });
            self.fail_startup(format!(
                "agent answered protocol version {version:?}; expected 1"
            ));
            return;
        }
        self.caps = acp::server_caps(&init);
        self.init = InitState::Ready;
        if self.phase().is_some() {
            self.advance();
        }
    }

    /// Fails the open turn with BACKEND_UNAVAILABLE and closes the process (0009-072).
    fn fail_startup(&mut self, text: String) {
        self.down_reason = Some(text.clone());
        self.close_conn(self.config.shutdown_grace);
        self.note_exit();
        let outcome = self.take_outcome(Outcome::Failed {
            reason: ReasonToken::BackendUnavailable,
            health_class: None,
            detail: detail(&text),
        });
        self.finish(outcome);
    }

    fn on_new_session(
        &mut self,
        turn_gen: u64,
        context_id: &str,
        peer_tools: Option<PeerTools>,
        outcome: Result<Value, Value>,
    ) {
        match outcome {
            Ok(result) => {
                let Some(sid) = result
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                else {
                    self.fail_if_current(turn_gen, "session/new returned no sessionId");
                    return;
                };
                self.sessions.insert(context_id.to_owned(), sid.clone());
                self.bindings
                    .insert(context_id.to_owned(), peer_tools.clone());
                if self.is_current_awaiting_session(turn_gen) {
                    self.bind_session(context_id, &sid);
                    self.send_prompt();
                }
            }
            Err(error) => {
                let text = format!("session/new failed: {}", acp::error_detail(&error));
                self.fail_if_current(turn_gen, &text);
            }
        }
    }

    fn on_restore(
        &mut self,
        turn_gen: u64,
        context_id: &str,
        session_id: &str,
        peer_tools: Option<PeerTools>,
        outcome: Result<Value, Value>,
    ) {
        match outcome {
            Ok(_) => {
                self.sessions
                    .insert(context_id.to_owned(), session_id.to_owned());
                self.bindings.insert(context_id.to_owned(), peer_tools);
                if self.is_current_awaiting_session(turn_gen) {
                    self.bind_session(context_id, session_id);
                    self.send_prompt();
                }
            }
            Err(error) if acp::is_session_not_found(&error) => {
                if self.is_current_awaiting_session(turn_gen) {
                    self.turn_notice("previous agent session not found; started a new session");
                    self.send_new_session();
                }
            }
            Err(error) => {
                let text = format!("session restore failed: {}", acp::error_detail(&error));
                self.fail_if_current(turn_gen, &text);
            }
        }
    }

    fn on_prompt(&mut self, turn_gen: u64, outcome: Result<Value, Value>) {
        if !self.turn_is(turn_gen) {
            return;
        }
        let canceling = self.phase() == Some(Phase::Canceling);
        match outcome {
            Ok(result) => {
                let stop = result
                    .get("stopReason")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let fallback = if canceling {
                    Outcome::Canceled
                } else {
                    acp::outcome_for_stop(stop)
                };
                if let Some(sample) = acp::prompt_usage(&result, canceling) {
                    self.turn_event(|turn, event_no| BackendEvent::Usage {
                        turn,
                        event_no,
                        sample,
                    });
                }
                let outcome = self.take_outcome(fallback);
                self.finish(outcome);
            }
            Err(error) => {
                let fallback = if canceling {
                    Outcome::Canceled
                } else {
                    Outcome::Failed {
                        reason: ReasonToken::BackendError,
                        health_class: None,
                        detail: acp::error_detail(&error),
                    }
                };
                let outcome = self.take_outcome(fallback);
                self.finish(outcome);
            }
        }
    }

    fn turn_is(&self, turn_gen: u64) -> bool {
        self.turn.as_ref().is_some_and(|t| t.generation == turn_gen)
    }

    fn is_current_awaiting_session(&self, turn_gen: u64) -> bool {
        self.turn
            .as_ref()
            .is_some_and(|t| t.generation == turn_gen && t.phase == Phase::AwaitSession)
    }

    fn fail_if_current(&mut self, turn_gen: u64, text: &str) {
        if self.turn_is(turn_gen) {
            let outcome = self.take_outcome(Outcome::Failed {
                reason: ReasonToken::BackendError,
                health_class: None,
                detail: detail(text),
            });
            self.finish(outcome);
        }
    }

    fn request(&mut self, id: Value, method: &str, params: Value) {
        match method {
            "session/request_permission" => self.permission_request(id, &params),
            "elicitation/create" => self.elicitation_request(id, &params),
            _ => {
                // Client methods we do not advertise (fs, terminal, ...) get -32601 (0009-019).
                self.reply_error(id, -32601, "method not found");
            }
        }
    }

    fn notification(&mut self, method: &str, params: Value) {
        if method == "session/update" {
            self.session_update(&params);
        }
    }

    fn permission_request(&mut self, id: Value, params: &Value) {
        let session = params
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or("");
        let open = self
            .turn
            .as_ref()
            .filter(|t| t.phase == Phase::Prompting && t.session_id.as_deref() == Some(session))
            .map(|t| t.generation);
        let Some(turn_gen) = open else {
            self.reply(id, json!({"outcome": {"outcome": "cancelled"}}));
            return;
        };
        let options = acp::permission_options(params);
        let offered = options.iter().map(|o| o.option_id.clone()).collect();
        let tool = params.get("toolCall");
        let tool_kind = tool
            .and_then(|t| t.get("kind"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let tool_title = redact(
            tool.and_then(|t| t.get("title"))
                .and_then(Value::as_str)
                .unwrap_or(""),
        );
        let request_id = self.next_pending_id();
        self.perms.insert(
            request_id.clone(),
            Pending {
                rpc_id: id,
                turn_gen,
                deadline: Instant::now() + self.config.permission_timeout,
                kind: PendingKind::Permission { offered },
            },
        );
        self.turn_event(|turn, event_no| BackendEvent::PermissionRequest {
            turn,
            event_no,
            request: PermissionRequest {
                request_id: RequestId::new(request_id),
                tool_kind,
                tool_title,
                options,
            },
        });
    }

    fn elicitation_request(&mut self, id: Value, params: &Value) {
        let (tool_title, allow_label) = match acp::parse_elicitation(params) {
            Elicitation::Decline => {
                self.reply(id, json!({"action": "decline"}));
                return;
            }
            Elicitation::Ask {
                tool_title,
                allow_label,
            } => (tool_title, allow_label),
        };
        let Some(turn_gen) = self
            .turn
            .as_ref()
            .filter(|t| t.phase == Phase::Prompting)
            .map(|t| t.generation)
        else {
            self.reply(id, json!({"action": "cancel"}));
            return;
        };
        let mut options = Vec::new();
        if allow_label.is_some() {
            options.push(PermissionOption {
                option_id: "allow-once".to_owned(),
                kind: "allow_once".to_owned(),
            });
        }
        options.push(PermissionOption {
            option_id: "reject-once".to_owned(),
            kind: "reject_once".to_owned(),
        });
        let request_id = self.next_pending_id();
        self.perms.insert(
            request_id.clone(),
            Pending {
                rpc_id: id,
                turn_gen,
                deadline: Instant::now() + self.config.permission_timeout,
                kind: PendingKind::Elicitation { allow_label },
            },
        );
        self.turn_event(|turn, event_no| BackendEvent::PermissionRequest {
            turn,
            event_no,
            request: PermissionRequest {
                request_id: RequestId::new(request_id),
                tool_kind: "form-elicitation".to_owned(),
                tool_title,
                options,
            },
        });
    }

    fn next_pending_id(&mut self) -> String {
        let id = format!("perm-{}-{}", self.request_epoch, self.next_request);
        self.next_request += 1;
        id
    }

    /// Answers one pending agent request with the operator's answer (0009-032 to 0009-034, 0009-082).
    fn resolve_pending(&mut self, request_id: &str, answer: PermissionAnswer) {
        let Some(pending) = self.perms.remove(request_id) else {
            return;
        };
        self.touch();
        let result = match (&pending.kind, answer) {
            (PendingKind::Permission { offered }, PermissionAnswer::Selected { option_id })
                if offered.contains(&option_id) =>
            {
                json!({"outcome": {"outcome": "selected", "optionId": option_id}})
            }
            (PendingKind::Permission { .. }, _) => json!({"outcome": {"outcome": "cancelled"}}),
            (
                PendingKind::Elicitation { allow_label },
                PermissionAnswer::Selected { option_id },
            ) => match (option_id.as_str(), allow_label) {
                ("allow-once", Some(label)) => {
                    json!({"action": "accept", "content": {"value": label}})
                }
                ("reject-once", _) => json!({"action": "decline"}),
                _ => json!({"action": "cancel"}),
            },
            (PendingKind::Elicitation { .. }, PermissionAnswer::Cancelled) => {
                json!({"action": "cancel"})
            }
        };
        self.reply(pending.rpc_id, result);
    }

    /// Answers every pending request that belongs to `turn_gen` as cancelled.
    fn resolve_turn_permissions(&mut self, turn_gen: u64) {
        let ids: Vec<String> = self
            .perms
            .iter()
            .filter(|(_, p)| p.turn_gen == turn_gen)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            self.resolve_pending(&id, PermissionAnswer::Cancelled);
        }
    }

    fn session_update(&mut self, params: &Value) {
        let session = params
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or("");
        let Some(turn) = self.turn.as_ref() else {
            return;
        };
        let live = matches!(turn.phase, Phase::Prompting | Phase::Canceling)
            && turn.session_id.as_deref() == Some(session);
        if !live {
            return;
        }
        let Some(update) = params.get("update") else {
            return;
        };
        match update.get("sessionUpdate").and_then(Value::as_str) {
            Some("tool_call" | "tool_call_update") => self.track_tool(update),
            Some("current_mode_update") => {
                let mode_id = update
                    .get("currentModeId")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if mode::switch_refused(
                    self.config.profile,
                    &self.config.approval_mode,
                    self.config.acknowledge_dangerous,
                    mode_id,
                ) {
                    self.emit(BackendEvent::Fault {
                        kind: FaultKind::ConfigRefused,
                        detail: detail("mid-session switch to a dangerous approval mode refused"),
                    });
                    if self.phase() == Some(Phase::Prompting) {
                        self.begin_cancel();
                    }
                }
                return;
            }
            _ => {}
        }
        match acp::translate_update(update) {
            Translated::Text(text) => {
                if let Some(turn) = self.turn.as_mut() {
                    turn.text_emitted = true;
                }
                self.turn_event(|turn, event_no| BackendEvent::ArtifactDelta {
                    turn,
                    event_no,
                    artifact_key: REPLY_ARTIFACT.to_owned(),
                    text,
                    append: true,
                    last: false,
                });
            }
            Translated::Progress(text) => self.turn_event(|turn, event_no| BackendEvent::Status {
                turn,
                event_no,
                payload: StatusPayload::Progress(text),
            }),
            Translated::Notice(text) => self.turn_event(|turn, event_no| BackendEvent::Status {
                turn,
                event_no,
                payload: StatusPayload::Notice(text),
            }),
            Translated::Usage(sample) => self.turn_event(|turn, event_no| BackendEvent::Usage {
                turn,
                event_no,
                sample,
            }),
            Translated::Dropped => {}
        }
    }

    /// Tracks open tool calls so cancel can report them as failed (0009-035).
    fn track_tool(&mut self, update: &Value) {
        let Some(id) = update
            .get("toolCallId")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            return;
        };
        let status = update.get("status").and_then(Value::as_str).unwrap_or("");
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        if matches!(status, "completed" | "failed") {
            turn.open_tools.remove(&id);
            return;
        }
        let kind = update.get("kind").and_then(Value::as_str).unwrap_or("");
        let title = update.get("title").and_then(Value::as_str).unwrap_or("");
        let entry = turn.open_tools.entry(id).or_insert_with(|| OpenTool {
            kind: String::new(),
            title: String::new(),
        });
        if !kind.is_empty() {
            entry.kind = kind.to_owned();
        }
        if !title.is_empty() {
            entry.title = title.to_owned();
        }
    }

    fn cancel(&mut self, turn_ref: &TurnRef) -> Result<(), BackendError> {
        let Some(turn) = self.turn.as_ref() else {
            return Ok(());
        };
        if &turn.turn != turn_ref {
            return Ok(());
        }
        match turn.phase {
            Phase::Canceling => {}
            Phase::Prompting => self.begin_cancel(),
            _ => {
                let outcome = self.take_outcome(Outcome::Canceled);
                self.finish(outcome);
            }
        }
        Ok(())
    }

    /// Answers pending requests, reports open tools as failed, sends `session/cancel`,
    /// and arms the grace timer (0009-035, 0009-036). The turn ends when the prompt
    /// returns or the grace expires.
    fn begin_cancel(&mut self) {
        let grace = self.config.cancel_grace;
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        turn.phase = Phase::Canceling;
        turn.grace_deadline = Some(Instant::now() + grace);
        let generation = turn.generation;
        let session = turn.session_id.clone().unwrap_or_default();
        let tools: Vec<(String, OpenTool)> = turn.open_tools.drain().collect();
        self.resolve_turn_permissions(generation);
        for (id, tool) in tools {
            let payload = json!({
                "toolCallId": id,
                "kind": tool.kind,
                "title": tool.title,
                "status": "failed",
                "detail": "canceled",
            });
            let text = acp::progress_text(payload);
            self.turn_event(|turn, event_no| BackendEvent::Status {
                turn,
                event_no,
                payload: StatusPayload::Progress(text),
            });
        }
        self.write(json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": {"sessionId": session}
        }));
    }

    fn tick(&mut self) {
        let now = Instant::now();
        let expired: Vec<String> = self
            .perms
            .iter()
            .filter(|(_, p)| p.deadline <= now)
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            self.backstop_expired(&id);
        }
        let Some(turn) = self.turn.as_ref() else {
            self.ensure_process();
            return;
        };
        let phase = turn.phase;
        let grace_expired = turn.grace_deadline.is_some_and(|g| g <= now);
        // While an approval is pending the agent waits on the operator; only its deadline runs (0009-024 reading).
        let idle_expired =
            self.perms.is_empty() && now >= turn.last_activity + self.config.turn_idle_timeout;
        if phase == Phase::Canceling {
            if grace_expired {
                self.grace_expired();
            }
        } else if idle_expired {
            self.idle_expired(phase);
        } else if phase == Phase::AwaitSpawn && self.conn.is_none() {
            self.advance();
        }
    }

    fn next_wakeup(&self) -> Option<Instant> {
        let mut best: Option<Instant> = None;
        let mut consider = |at: Instant| best = Some(best.map_or(at, |b| b.min(at)));
        if let Some(turn) = self.turn.as_ref() {
            if turn.phase == Phase::Canceling {
                if let Some(grace) = turn.grace_deadline {
                    consider(grace);
                }
            } else {
                if self.perms.is_empty() {
                    consider(turn.last_activity + self.config.turn_idle_timeout);
                }
            }
            if turn.phase == Phase::AwaitSpawn && self.conn.is_none() {
                if let Some(at) = self.respawn_at {
                    consider(at);
                }
            }
        }
        if self.turn.is_none() && self.conn.is_none() {
            if let Some(at) = self.respawn_at {
                consider(at);
            }
        }
        for pending in self.perms.values() {
            consider(pending.deadline);
        }
        best
    }

    /// The driver's own permission backstop fired (0009-033). The worker's approval expiry is
    /// authoritative and normally fires first, so no answer arrived in time. The turn ends as a
    /// timeout, reported by a Fault with detail, never as a silent crash.
    fn backstop_expired(&mut self, request_id: &str) {
        let Some(turn_gen) = self.perms.get(request_id).map(|p| p.turn_gen) else {
            return;
        };
        let detail = format!(
            "permission backstop expired after {:?}; worker approval expiry is {:?}",
            self.config.permission_timeout, WORKER_APPROVAL_EXPIRY
        );
        self.emit(BackendEvent::Fault {
            kind: FaultKind::Internal,
            detail: detail.clone(),
        });
        if let Some(turn) = self.turn.as_mut().filter(|t| t.generation == turn_gen) {
            turn.override_outcome = Some(Outcome::Failed {
                reason: ReasonToken::BackendTimeout,
                health_class: None,
                detail,
            });
        }
        self.resolve_pending(request_id, PermissionAnswer::Cancelled);
    }

    /// Turn idle past the limit: record BACKEND_TIMEOUT and cancel (0009-024).
    fn idle_expired(&mut self, phase: Phase) {
        if let Some(turn) = self.turn.as_mut() {
            turn.override_outcome = Some(Outcome::Failed {
                reason: ReasonToken::BackendTimeout,
                health_class: None,
                detail: "turn idle timeout".to_owned(),
            });
        }
        if phase == Phase::Prompting {
            self.begin_cancel();
        } else {
            let outcome = self.take_outcome(Outcome::Canceled);
            self.finish(outcome);
        }
    }

    /// Cancel grace expired: kill the process tree and end the turn (0009-036).
    fn grace_expired(&mut self) {
        self.down_reason = Some("cancel grace expired; agent process killed".to_owned());
        self.close_conn(Duration::ZERO);
        self.note_exit();
        let outcome = self.take_outcome(Outcome::Canceled);
        self.finish(outcome);
    }

    /// Unexpected stdout EOF or stdin write failure (0009-023).
    fn exited(&mut self, reason: &str) {
        let tail = self
            .conn
            .as_ref()
            .and_then(|c| {
                c.stderr_tail
                    .lock()
                    .ok()
                    .map(|b| String::from_utf8_lossy(&b).into_owned())
            })
            .unwrap_or_default();
        let phase = self.phase();
        let status = self.close_conn(EXIT_OBSERVE);
        tracing::warn!(target: "driver_acp::child", cause = reason, status = ?status, "agent process ended unexpectedly");
        let text = stream_end_text(reason, status.as_deref(), &tail);
        self.down_reason = Some(text.clone());
        self.emit(BackendEvent::Fault {
            kind: FaultKind::StreamEnded,
            detail: detail(&text),
        });
        if self.turn.is_some() {
            let fallback = match phase {
                Some(Phase::Canceling) => Outcome::Canceled,
                Some(Phase::AwaitInit) => Outcome::Failed {
                    reason: ReasonToken::BackendUnavailable,
                    health_class: None,
                    detail: detail(&text),
                },
                _ => Outcome::Failed {
                    reason: ReasonToken::BackendCrashed,
                    health_class: None,
                    detail: detail(&text),
                },
            };
            let outcome = self.take_outcome(fallback);
            self.finish(outcome);
        }
        self.note_exit();
    }

    /// Closes stdin, waits up to `grace`, then kills the tree (0009-059, 0009-060).
    /// Returns the exit status only if the process exited on its own within `grace`.
    fn close_conn(&mut self, grace: Duration) -> Option<String> {
        let mut status = None;
        if let Some(mut conn) = self.conn.take() {
            conn.stdin = None;
            if conn.terminator.wait_exit(grace) {
                status = conn.terminator.exit_description();
            } else {
                conn.terminator.kill_tree();
            }
        }
        self.init = InitState::Down;
        self.caps = ServerCaps {
            load_session: false,
            resume: false,
        };
        self.sessions.clear();
        self.bindings.clear();
        self.rpc.clear();
        self.perms.clear();
        status
    }

    /// Records an unexpected exit and schedules the next respawn (0009-061).
    fn note_exit(&mut self) {
        let now = Instant::now();
        self.exits.push_back(now);
        while self
            .exits
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) > EXIT_WINDOW)
        {
            self.exits.pop_front();
        }
        if self.exits.len() >= UNHEALTHY_EXITS {
            self.unhealthy = true;
            self.down_reason = Some("agent exited too often; driver unhealthy".to_owned());
            self.respawn_at = None;
        } else {
            self.respawn_at = Some(now + restart_delay(self.exits.len()));
        }
    }

    /// Ends the open turn: resolves its permissions, flushes the final text delta,
    /// emits the per-turn driver usage and exactly one Terminal (0009-007, 0009-055).
    fn finish(&mut self, outcome: Outcome) {
        let Some(generation) = self.turn.as_ref().map(|t| t.generation) else {
            return;
        };
        self.resolve_turn_permissions(generation);
        let Some((text_emitted, started)) = self.turn.as_ref().map(|t| (t.text_emitted, t.started))
        else {
            return;
        };
        if text_emitted {
            self.turn_event(|turn, event_no| BackendEvent::ArtifactDelta {
                turn,
                event_no,
                artifact_key: REPLY_ARTIFACT.to_owned(),
                text: String::new(),
                append: true,
                last: true,
            });
        }
        let wall_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.turn_event(|turn, event_no| BackendEvent::Usage {
            turn,
            event_no,
            sample: UsageSample {
                source: UsageSource::DriverCounted,
                turns: Some(1),
                wall_ms: Some(wall_ms),
                tokens_in: None,
                tokens_out: None,
                cost: None,
                gauges: None,
            },
        });
        self.turn_event(|turn, event_no| BackendEvent::Terminal {
            turn,
            event_no,
            outcome,
        });
        self.turn = None;
    }

    fn shutdown(&mut self, grace: Duration) {
        let outcome = self.take_outcome(Outcome::Canceled);
        self.finish(outcome);
        self.down_reason = Some("driver shut down".to_owned());
        self.closed = true;
        self.close_conn(grace);
    }
}

/// Text of a `StreamEnded` fault: reason, then the reaped exit status, then the stderr tail (0009-023).
fn stream_end_text(reason: &str, status: Option<&str>, tail: &str) -> String {
    let mut text = reason.to_owned();
    if let Some(status) = status {
        text.push_str(&format!(" ({status})"));
    }
    let tail = tail.trim();
    if !tail.is_empty() {
        text.push_str(": ");
        text.push_str(tail);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_backoff_doubles_and_caps_0009_061() {
        assert_eq!(restart_delay(1), Duration::from_secs(1));
        assert_eq!(restart_delay(2), Duration::from_secs(2));
        assert_eq!(restart_delay(3), Duration::from_secs(4));
        assert_eq!(restart_delay(4), Duration::from_secs(8));
        assert_eq!(restart_delay(20), Duration::from_secs(60));
    }

    #[test]
    fn stream_end_detail_carries_status_and_stderr_0009_023() {
        assert_eq!(
            stream_end_text("agent stdout closed", Some("exit status: 3"), "boom\n"),
            "agent stdout closed (exit status: 3): boom"
        );
        assert_eq!(
            stream_end_text("agent stdout closed", None, "  "),
            "agent stdout closed"
        );
    }
}
