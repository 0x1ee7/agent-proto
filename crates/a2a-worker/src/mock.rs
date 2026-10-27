//! Scriptable mock `Backend` (RFC-0009 section 4.12, 0009-074).
//!
//! Each `start` consumes the next scripted turn and emits its steps through the
//! attached sink, with `turn` and `event_no` assigned by the mock. A `Hold` step
//! pauses the turn until a permission answer or [`MockControl::release`]. A
//! `Cancel` on an open turn ends it with `Canceled` unless ignored.
//!
//! Only available with the `testkit` feature or in tests.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::backend::{
    Backend, BackendError, BackendEvent, BackendSink, Capabilities, FaultKind, Health, Outcome,
    PermissionAnswer, PermissionRequest, ReasonToken, RequestId, StartTurn, StatusPayload, TurnRef,
    UsageSample,
};

/// One scripted step. The mock fills in `turn` and `event_no`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Emit a status event.
    Status(StatusPayload),
    /// Emit a text artifact delta.
    Artifact {
        /// Artifact key.
        key: String,
        /// Text.
        text: String,
        /// Append semantics.
        append: bool,
        /// Final delta.
        last: bool,
    },
    /// Emit a usage sample.
    Usage(UsageSample),
    /// Emit a permission request; the mock records it as pending.
    Permission(PermissionRequest),
    /// Emit an input-required event.
    InputRequired(String),
    /// Emit the terminal event and end the turn.
    Terminal(Outcome),
    /// Pause until a pending permission is answered or [`MockControl::release`] is called.
    Hold,
}

/// Command recorded by the mock, in call order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `Start` was received.
    Start(Box<StartTurn>),
    /// `ResolvePermission` was received.
    Resolve(RequestId, PermissionAnswer),
    /// `Cancel` was received.
    Cancel(TurnRef),
    /// `Shutdown` was received.
    Shutdown(Duration),
}

struct OpenTurn {
    turn: TurnRef,
    next_no: u64,
    steps: VecDeque<Step>,
    held: bool,
}

struct State {
    caps: Capabilities,
    sink: Option<Box<dyn BackendSink>>,
    scripts: VecDeque<Vec<Step>>,
    /// Turns currently open, in start order.
    open: Vec<OpenTurn>,
    /// Open-turn limit: `1` models an agent that runs one turn at a time (0009-020); larger
    /// values model a multi-session backend running contexts concurrently (0005-054).
    max_open: usize,
    /// Permission requests awaiting an answer, with the turn that raised each.
    pending: Vec<(TurnRef, RequestId)>,
    log: Vec<Command>,
    alive: bool,
    restarts: u32,
    ignore_cancel: bool,
}

impl State {
    fn emit(&mut self, event: BackendEvent) {
        if let Some(sink) = self.sink.as_mut() {
            sink.emit(event);
        }
    }

    /// Runs the open turn `session` until it ends, pauses for a permission, or holds.
    fn run(&mut self, session: &TurnRef) {
        loop {
            let Some(index) = self.open.iter().position(|open| &open.turn == session) else {
                return;
            };
            let open = &mut self.open[index];
            if open.held {
                return;
            }
            let Some(step) = open.steps.pop_front() else {
                return;
            };
            let turn = open.turn.clone();
            let event_no = open.next_no;
            let event = match step {
                Step::Hold => {
                    self.open[index].held = true;
                    return;
                }
                Step::Status(payload) => BackendEvent::Status {
                    turn,
                    event_no,
                    payload,
                },
                Step::Artifact {
                    key,
                    text,
                    append,
                    last,
                } => BackendEvent::ArtifactDelta {
                    turn,
                    event_no,
                    artifact_key: key,
                    text,
                    append,
                    last,
                },
                Step::Usage(sample) => BackendEvent::Usage {
                    turn,
                    event_no,
                    sample,
                },
                Step::Permission(request) => {
                    self.pending
                        .push((turn.clone(), request.request_id.clone()));
                    BackendEvent::PermissionRequest {
                        turn,
                        event_no,
                        request,
                    }
                }
                Step::InputRequired(text) => BackendEvent::InputRequired {
                    turn,
                    event_no,
                    text,
                },
                Step::Terminal(outcome) => BackendEvent::Terminal {
                    turn,
                    event_no,
                    outcome,
                },
            };
            let terminal = matches!(event, BackendEvent::Terminal { .. });
            let pauses = matches!(event, BackendEvent::PermissionRequest { .. });
            if let Some(open) = self.open.get_mut(index) {
                open.next_no += 1;
            }
            self.emit(event);
            if pauses {
                if let Some(open) = self.open.get_mut(index) {
                    open.held = true;
                }
                return;
            }
            if terminal {
                self.end_turn(session);
                return;
            }
        }
    }

    fn end_turn(&mut self, session: &TurnRef) {
        self.open.retain(|open| &open.turn != session);
        self.pending.retain(|(owner, _)| owner != session);
    }

    /// Ends the open turn at `index` with a terminal event.
    fn terminate_index(&mut self, index: usize, outcome: Outcome) {
        let open = self.open.remove(index);
        let event = BackendEvent::Terminal {
            turn: open.turn.clone(),
            event_no: open.next_no,
            outcome,
        };
        self.emit(event);
        self.pending.retain(|(owner, _)| owner != &open.turn);
    }

    /// Ends `session` with a terminal event, if it is open.
    fn terminate_turn(&mut self, session: &TurnRef, outcome: Outcome) {
        if let Some(index) = self.open.iter().position(|open| &open.turn == session) {
            self.terminate_index(index, outcome);
        }
    }

    /// Ends every open turn, building each terminal outcome with `outcome`.
    fn terminate_all(&mut self, outcome: impl Fn() -> Outcome) {
        while !self.open.is_empty() {
            self.terminate_index(0, outcome());
        }
    }
}

fn crash_outcome(detail: &str) -> Outcome {
    Outcome::Failed {
        reason: ReasonToken::BackendCrashed,
        health_class: None,
        detail: detail.to_owned(),
    }
}

/// Scriptable mock backend. Construct with [`MockBackend::new`].
pub struct MockBackend {
    state: Arc<Mutex<State>>,
}

impl std::fmt::Debug for MockBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MockBackend")
    }
}

/// Test-side handle that scripts and inspects a [`MockBackend`].
#[derive(Clone)]
pub struct MockControl {
    state: Arc<Mutex<State>>,
}

impl std::fmt::Debug for MockControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MockControl")
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

impl MockBackend {
    /// Creates a mock with the given capabilities, returning the backend and its control handle.
    pub fn new(caps: Capabilities) -> (Self, MockControl) {
        let state = Arc::new(Mutex::new(State {
            caps,
            sink: None,
            scripts: VecDeque::new(),
            open: Vec::new(),
            max_open: 1,
            pending: Vec::new(),
            log: Vec::new(),
            alive: true,
            restarts: 0,
            ignore_cancel: false,
        }));
        (
            Self {
                state: Arc::clone(&state),
            },
            MockControl { state },
        )
    }
}

impl Backend for MockBackend {
    fn capabilities(&self) -> Capabilities {
        lock(&self.state).caps.clone()
    }

    fn health(&self) -> Health {
        let state = lock(&self.state);
        Health {
            live: state.alive,
            restarts: state.restarts,
        }
    }

    fn attach(&mut self, sink: Box<dyn BackendSink>) {
        lock(&self.state).sink = Some(sink);
    }

    fn start(&mut self, cmd: StartTurn) -> Result<(), BackendError> {
        let mut state = lock(&self.state);
        if !state.alive {
            return Err(BackendError::Closed);
        }
        if state.sink.is_none() {
            return Err(BackendError::NoSink);
        }
        if state.open.len() >= state.max_open {
            return Err(BackendError::Busy);
        }
        state.log.push(Command::Start(Box::new(cmd.clone())));
        let steps: VecDeque<Step> = state.scripts.pop_front().unwrap_or_default().into();
        let session = cmd.turn.clone();
        state.open.push(OpenTurn {
            turn: cmd.turn,
            next_no: 1,
            steps,
            held: false,
        });
        state.run(&session);
        Ok(())
    }

    fn resolve_permission(
        &mut self,
        request_id: &RequestId,
        answer: PermissionAnswer,
    ) -> Result<(), BackendError> {
        let mut state = lock(&self.state);
        state.log.push(Command::Resolve(request_id.clone(), answer));
        if let Some(index) = state.pending.iter().position(|(_, id)| id == request_id) {
            let (session, _) = state.pending.remove(index);
            if let Some(open) = state.open.iter_mut().find(|open| open.turn == session) {
                open.held = false;
            }
            state.run(&session);
        }
        Ok(())
    }

    fn cancel(&mut self, turn: &TurnRef) -> Result<(), BackendError> {
        let mut state = lock(&self.state);
        state.log.push(Command::Cancel(turn.clone()));
        let matches_open = state.open.iter().any(|open| &open.turn == turn);
        if matches_open && !state.ignore_cancel {
            state.terminate_turn(turn, Outcome::Canceled);
        }
        Ok(())
    }

    fn shutdown(&mut self, grace: Duration) -> Result<(), BackendError> {
        let mut state = lock(&self.state);
        state.log.push(Command::Shutdown(grace));
        state.terminate_all(|| crash_outcome("agent shut down with an open turn"));
        state.alive = false;
        Ok(())
    }
}

impl MockControl {
    /// Queues the steps of the next turn to be started.
    pub fn enqueue(&self, steps: Vec<Step>) {
        lock(&self.state).scripts.push_back(steps);
    }

    /// Returns the commands received so far, in order.
    pub fn commands(&self) -> Vec<Command> {
        lock(&self.state).log.clone()
    }

    /// Returns the open turn reference, if any.
    pub fn open_turn(&self) -> Option<TurnRef> {
        lock(&self.state).open.first().map(|open| open.turn.clone())
    }

    /// Releases a held turn and runs its remaining steps.
    pub fn release(&self) {
        let mut state = lock(&self.state);
        let sessions: Vec<TurnRef> = state.open.iter().map(|open| open.turn.clone()).collect();
        for session in sessions {
            if let Some(open) = state.open.iter_mut().find(|open| open.turn == session) {
                open.held = false;
            }
            state.run(&session);
        }
    }

    /// Makes `cancel` leave the turn open (cancel race, 0009-074).
    pub fn set_ignore_cancel(&self, ignore: bool) {
        lock(&self.state).ignore_cancel = ignore;
    }

    /// Sets how many turns the mock keeps open at once. The default `1` models a
    /// one-turn-at-a-time agent; a larger limit models concurrent contexts (0005-054).
    pub fn set_max_open(&self, limit: usize) {
        lock(&self.state).max_open = limit;
    }

    /// Simulates agent process death: ends any open turn with `Failed` `BACKEND_CRASHED` and marks the backend closed.
    pub fn crash(&self, detail: &str) {
        let mut state = lock(&self.state);
        state.terminate_all(|| crash_outcome(detail));
        state.alive = false;
    }

    /// Simulates a driver restart: the backend is live again with an incremented restart count.
    /// The open turn of the prior incarnation is dropped without a terminal event (0009-008).
    pub fn restart(&self) {
        let mut state = lock(&self.state);
        state.open.clear();
        state.pending.clear();
        state.alive = true;
        state.restarts += 1;
    }

    /// Emits a driver-level fault.
    pub fn fault(&self, kind: FaultKind, detail: &str) {
        lock(&self.state).emit(BackendEvent::Fault {
            kind,
            detail: detail.to_owned(),
        });
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::backend::{ChannelSink, Protocol, UsageSource};
    use std::sync::mpsc::Receiver;

    fn caps() -> Capabilities {
        Capabilities {
            protocol: Protocol::Acp,
            resume: false,
            cancel: true,
            usage_sources: vec![UsageSource::DriverCounted],
            approval_modes: vec!["default".to_owned()],
            streaming_text: true,
        }
    }

    fn start_cmd(turn: &str) -> StartTurn {
        StartTurn {
            turn: TurnRef::new(turn),
            context_id: "ctx".to_owned(),
            input: Vec::new(),
            deadline_ms: 0,
            session: None,
            origin: "origin".to_owned(),
            peer_tools: None,
        }
    }

    fn attached() -> (MockBackend, MockControl, Receiver<BackendEvent>) {
        let (mut backend, control) = MockBackend::new(caps());
        let (sink, rx) = ChannelSink::channel();
        backend.attach(Box::new(sink));
        (backend, control, rx)
    }

    fn drain(rx: &Receiver<BackendEvent>) -> Vec<BackendEvent> {
        rx.try_iter().collect()
    }

    fn text(s: &str) -> Step {
        Step::Artifact {
            key: "out".to_owned(),
            text: s.to_owned(),
            append: true,
            last: false,
        }
    }

    /// Guards 0009-006, 0009-007: a scripted turn emits contiguous event_no and one Terminal.
    #[test]
    fn scripted_turn_numbers_events_and_terminates_once() {
        let (mut backend, control, rx) = attached();
        control.enqueue(vec![
            text("a"),
            text("b"),
            Step::Terminal(Outcome::Completed),
        ]);
        backend.start(start_cmd("turn-0001")).expect("start");
        let events = drain(&rx);
        let numbers: Vec<u64> = events.iter().filter_map(BackendEvent::event_no).collect();
        assert_eq!(numbers, vec![1, 2, 3]);
        assert!(matches!(events.last(), Some(BackendEvent::Terminal { .. })));
        assert!(control.open_turn().is_none());
    }

    /// Guards 0009-020: a second start while a turn is open is refused.
    #[test]
    fn second_start_while_open_is_busy() {
        let (mut backend, control, _rx) = attached();
        control.enqueue(vec![Step::Hold]);
        backend.start(start_cmd("turn-0001")).expect("first start");
        assert_eq!(
            backend.start(start_cmd("turn-0002")),
            Err(BackendError::Busy)
        );
    }

    /// Guards 0009-033: a held permission resumes only with a pending request id.
    #[test]
    fn permission_answer_resumes_held_turn() {
        let (mut backend, control, rx) = attached();
        control.enqueue(vec![
            Step::Permission(PermissionRequest {
                request_id: RequestId::new("perm-1"),
                tool_kind: "execute".to_owned(),
                tool_title: "bash".to_owned(),
                options: Vec::new(),
            }),
            Step::Terminal(Outcome::Completed),
        ]);
        backend.start(start_cmd("turn-0001")).expect("start");
        assert_eq!(drain(&rx).len(), 1, "turn holds after the request");
        backend
            .resolve_permission(
                &RequestId::new("perm-1"),
                PermissionAnswer::Selected {
                    option_id: "allow".to_owned(),
                },
            )
            .expect("resolve");
        let rest = drain(&rx);
        assert!(matches!(rest.last(), Some(BackendEvent::Terminal { .. })));
    }

    /// Guards 0009-010: an unknown request id is ignored without a fault or resume.
    #[test]
    fn unknown_permission_is_ignored() {
        let (mut backend, control, rx) = attached();
        control.enqueue(vec![Step::Hold, Step::Terminal(Outcome::Completed)]);
        backend.start(start_cmd("turn-0001")).expect("start");
        backend
            .resolve_permission(&RequestId::new("nope"), PermissionAnswer::Cancelled)
            .expect("ignored");
        assert!(drain(&rx).is_empty());
        assert!(control.open_turn().is_some());
    }

    /// Guards 0009-009: cancel is idempotent and yields exactly one Canceled terminal.
    #[test]
    fn cancel_is_idempotent_and_terminates_once() {
        let (mut backend, control, rx) = attached();
        control.enqueue(vec![Step::Hold]);
        backend.start(start_cmd("turn-0001")).expect("start");
        backend.cancel(&TurnRef::new("turn-0001")).expect("cancel");
        backend.cancel(&TurnRef::new("turn-0001")).expect("again");
        let events = drain(&rx);
        let terminals = events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    BackendEvent::Terminal {
                        outcome: Outcome::Canceled,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(terminals, 1);
    }

    /// Guards 0009-036: an ignored cancel leaves the turn open for the supervisor to kill.
    #[test]
    fn ignored_cancel_leaves_turn_open() {
        let (mut backend, control, rx) = attached();
        control.enqueue(vec![Step::Hold]);
        control.set_ignore_cancel(true);
        backend.start(start_cmd("turn-0001")).expect("start");
        backend.cancel(&TurnRef::new("turn-0001")).expect("cancel");
        assert!(drain(&rx).is_empty());
        assert!(control.open_turn().is_some());
    }

    /// Guards 0009-059 and 0009-060: a crash ends the open turn as BACKEND_CRASHED and closes the backend.
    #[test]
    fn crash_ends_open_turn_as_backend_crashed() {
        let (mut backend, control, rx) = attached();
        control.enqueue(vec![Step::Hold]);
        backend.start(start_cmd("turn-0001")).expect("start");
        control.crash("process exited");
        let events = drain(&rx);
        assert!(matches!(
            events.last(),
            Some(BackendEvent::Terminal {
                outcome: Outcome::Failed {
                    reason: ReasonToken::BackendCrashed,
                    ..
                },
                ..
            })
        ));
        assert!(!backend.health().live);
        assert_eq!(
            backend.start(start_cmd("turn-0002")),
            Err(BackendError::Closed)
        );
    }

    /// Guards 0009-008: a restart drops the prior incarnation's turn without a terminal and bumps restarts.
    #[test]
    fn restart_drops_open_turn_and_counts_restarts() {
        let (mut backend, control, rx) = attached();
        control.enqueue(vec![Step::Hold]);
        backend.start(start_cmd("turn-0001")).expect("start");
        control.restart();
        assert!(drain(&rx).is_empty());
        let health = backend.health();
        assert!(health.live);
        assert_eq!(health.restarts, 1);
    }

    /// Guards 0009-011: capabilities report the configured record.
    #[test]
    fn capabilities_report_configuration() {
        let (backend, _control) = MockBackend::new(caps());
        assert_eq!(backend.capabilities(), caps());
    }

    /// Guards 0009-020: start without an attached sink is refused.
    #[test]
    fn start_without_sink_is_refused() {
        let (mut backend, _control) = MockBackend::new(caps());
        assert_eq!(
            backend.start(start_cmd("turn-0001")),
            Err(BackendError::NoSink)
        );
    }

    /// Guards 0009-074: a fault is delivered without a turn reference.
    #[test]
    fn fault_is_delivered_without_turn() {
        let (_backend, control, rx) = attached();
        control.fault(FaultKind::ConfigRefused, "dangerous mode");
        let events = drain(&rx);
        assert!(matches!(
            events.as_slice(),
            [BackendEvent::Fault {
                kind: FaultKind::ConfigRefused,
                ..
            }]
        ));
    }
}
