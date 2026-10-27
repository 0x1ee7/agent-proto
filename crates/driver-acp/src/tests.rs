#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Offline integration tests: the real driver actor against an in-process fake
//! ACP agent. The prompt text selects the agent's scripted behaviour.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use a2a_worker::backend::{
    Backend, BackendError, BackendEvent, ChannelSink, FaultKind, InputPart, Outcome, PeerTools,
    PermissionAnswer, PermissionRequest, Protocol, ReasonToken, RequestId, SecretText,
    SessionHandle, StartTurn, StatusPayload, TurnRef, UsageSource,
};
use a2a_worker::backend_contract::check_turn_stream;
use serde_json::{Value, json};

use crate::driver::AcpDriver;
use crate::fake::{Counters, FakeSpawner, Step, ok, update as session_update};
use crate::{AcpDriverConfig, Profile};

type Rec = Arc<Mutex<Vec<Value>>>;

const WAIT: Duration = Duration::from_secs(20);

fn lock(rec: &Rec) -> MutexGuard<'_, Vec<Value>> {
    rec.lock().unwrap_or_else(PoisonError::into_inner)
}

fn method(frame: &Value) -> &str {
    frame.get("method").and_then(Value::as_str).unwrap_or("")
}

fn prompt_text(frame: &Value) -> String {
    frame["params"]["prompt"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_owned()
}

fn request(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

fn permission_params() -> Value {
    json!({
        "sessionId": "s-1",
        "toolCall": {"toolCallId": "t1", "kind": "execute", "title": "run ls"},
        "options": [
            {"optionId": "allow-once", "kind": "allow_once", "name": "Allow once"},
            {"optionId": "allow-with-updates", "kind": "allow_always", "name": "Allow always"},
            {"optionId": "reject", "kind": "reject_once", "name": "Reject"}
        ]
    })
}

fn elicitation_params(message: &str) -> Value {
    json!({
        "mode": "form",
        "sessionId": "s-1",
        "message": message,
        "requestedSchema": {
            "type": "object",
            "properties": {"value": {"type": "string", "enum": ["Approve Once", "Deny"]}},
            "required": ["value"]
        }
    })
}

/// Handshake replies: `initialize`, `session/new`, `session/load`, `session/resume`.
fn base(frame: &Value) -> Option<Vec<Value>> {
    let id = frame.get("id")?;
    let reply = match method(frame) {
        "initialize" => ok(
            id,
            json!({
                "protocolVersion": 1,
                "agentCapabilities": {"loadSession": true, "sessionCapabilities": {"resume": {}}}
            }),
        ),
        "session/new" => ok(id, json!({"sessionId": "s-1"})),
        "session/load" | "session/resume" => ok(id, json!({})),
        _ => return None,
    };
    Some(vec![reply])
}

fn last_prompt(rec: &Rec) -> Option<Value> {
    lock(rec)
        .iter()
        .rev()
        .find(|r| method(r) == "session/prompt")
        .cloned()
}

fn last_prompt_text(rec: &Rec) -> String {
    last_prompt(rec)
        .map(|p| prompt_text(&p))
        .unwrap_or_default()
}

/// Finishes the most recent `session/prompt` with `stopReason`.
fn finish(rec: &Rec, stop: &str) -> Step {
    let id = last_prompt(rec)
        .and_then(|p| p.get("id").cloned())
        .unwrap_or(Value::Null);
    Step::Emit(vec![ok(
        &id,
        json!({"stopReason": stop, "usage": {"inputTokens": 3, "outputTokens": 4}}),
    )])
}

fn prompt(rec: &Rec, id: &Value, text: &str) -> Step {
    let s = "s-1";
    if text == "crash" {
        return Step::Crash;
    }
    if let Some(stop) = text.strip_prefix("stop:") {
        return Step::Emit(vec![ok(id, json!({"stopReason": stop}))]);
    }
    match text {
        "hello" => Step::Emit(vec![
            session_update(
                s,
                json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "hello"}}),
            ),
            ok(
                id,
                json!({"stopReason": "end_turn", "usage": {"inputTokens": 3, "outputTokens": 4}}),
            ),
        ]),
        "error" => Step::Emit(vec![json!({
            "jsonrpc": "2.0", "id": id,
            "error": {"code": -32000, "message": "boom sk-secret12345"}
        })]),
        "perm" => Step::Emit(vec![request(
            50,
            "session/request_permission",
            permission_params(),
        )]),
        "perm2" => Step::Emit(vec![request(
            51,
            "session/request_permission",
            permission_params(),
        )]),
        "hang" => Step::Emit(vec![
            session_update(
                s,
                json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "kind": "execute",
                       "title": "run ls", "status": "pending"}),
            ),
            request(50, "session/request_permission", permission_params()),
        ]),
        "fs" => Step::Emit(vec![request(
            99,
            "fs/read_text_file",
            json!({"sessionId": "s-1", "path": "/x"}),
        )]),
        "elic" => Step::Emit(vec![request(
            60,
            "elicitation/create",
            elicitation_params("Allow tool: write\nPath: /x"),
        )]),
        "elic-bad" => Step::Emit(vec![request(
            61,
            "elicitation/create",
            elicitation_params("hello"),
        )]),
        "switch" => Step::Emit(vec![session_update(
            s,
            json!({"sessionUpdate": "current_mode_update", "currentModeId": "bypassPermissions"}),
        )]),
        "silent" => Step::Emit(vec![]),
        _ => finish(rec, "end_turn"),
    }
}

/// The scripted agent used by most tests.
fn agent(rec: &Rec, frame: &Value) -> Step {
    if let Some(frames) = base(frame) {
        return Step::Emit(frames);
    }
    let id = frame.get("id").cloned().unwrap_or(Value::Null);
    match method(frame) {
        "session/prompt" => prompt(rec, &id, &prompt_text(frame)),
        "session/cancel" => {
            if matches!(last_prompt_text(rec).as_str(), "hang" | "switch") {
                finish(rec, "cancelled")
            } else {
                Step::Emit(vec![])
            }
        }
        // A driver response to an agent-issued request.
        "" => {
            if matches!(last_prompt_text(rec).as_str(), "hang" | "silent" | "switch") {
                Step::Emit(vec![])
            } else {
                finish(rec, "end_turn")
            }
        }
        _ => Step::Emit(vec![]),
    }
}

/// Agent that answers the protocol version with 2 (0009-016).
fn wrong_version(_rec: &Rec, frame: &Value) -> Step {
    if method(frame) == "initialize" {
        Step::Emit(vec![ok(&frame["id"], json!({"protocolVersion": 2}))])
    } else {
        Step::Emit(vec![])
    }
}

/// Agent that refuses `session/load` as an unknown session (0009-013).
fn load_not_found(rec: &Rec, frame: &Value) -> Step {
    if method(frame) == "session/load" {
        return Step::Emit(vec![json!({
            "jsonrpc": "2.0", "id": frame["id"],
            "error": {"code": -32002, "message": "ACP session not found"}
        })]);
    }
    agent(rec, frame)
}

struct Harness {
    spawner: FakeSpawner,
    counters: Arc<Counters>,
    rec: Rec,
}

impl Harness {
    fn hanging(self) -> Self {
        Self {
            spawner: self.spawner.hanging(),
            ..self
        }
    }
}

/// Builds a fake spawner; every frame the driver writes is recorded in `rec`.
fn harness(reply: fn(&Rec, &Value) -> Step) -> Harness {
    let rec: Rec = Arc::new(Mutex::new(Vec::new()));
    let shared = rec.clone();
    let (spawner, counters) = FakeSpawner::new(move || {
        let shared = shared.clone();
        Box::new(move |frame: &Value| {
            lock(&shared).push(frame.clone());
            reply(&shared, frame)
        })
    });
    Harness {
        spawner,
        counters,
        rec,
    }
}

struct Fixture {
    driver: AcpDriver,
    rx: Receiver<BackendEvent>,
    counters: Arc<Counters>,
    rec: Rec,
}

fn config() -> AcpDriverConfig {
    let root = std::env::temp_dir().join("driver-acp-tests");
    let mut cfg = AcpDriverConfig::new(
        Profile::Claude,
        root.join("claude-agent"),
        root.join("ws"),
        root.join("cfg"),
    );
    cfg.cancel_grace = Duration::from_millis(200);
    cfg
}

fn open(h: Harness, cfg: AcpDriverConfig) -> Fixture {
    let mut driver = AcpDriver::with_spawner(cfg, Box::new(h.spawner)).expect("valid config");
    let (sink, rx) = ChannelSink::channel();
    driver.attach(Box::new(sink));
    Fixture {
        driver,
        rx,
        counters: h.counters,
        rec: h.rec,
    }
}

fn start(text: &str, turn: &str, session: Option<&str>) -> StartTurn {
    StartTurn {
        turn: TurnRef::new(turn),
        context_id: "ctx-1".to_owned(),
        input: vec![InputPart::Text(text.to_owned())],
        origin: "peer-test".to_owned(),
        peer_tools: None,
        deadline_ms: u64::MAX,
        session: session.map(SessionHandle::new),
    }
}

/// Collects events until the first `Terminal`, then until the stream is quiet.
fn drain(rx: &Receiver<BackendEvent>) -> Vec<BackendEvent> {
    let mut events = Vec::new();
    loop {
        let event = rx.recv_timeout(WAIT).expect("driver event before timeout");
        let done = matches!(event, BackendEvent::Terminal { .. });
        events.push(event);
        if done {
            break;
        }
    }
    while let Ok(event) = rx.recv_timeout(Duration::from_millis(150)) {
        events.push(event);
    }
    events
}

fn outcome(events: &[BackendEvent]) -> Outcome {
    events
        .iter()
        .find_map(|e| match e {
            BackendEvent::Terminal { outcome, .. } => Some(outcome.clone()),
            _ => None,
        })
        .expect("turn has a Terminal")
}

fn assert_contract(events: &[BackendEvent]) {
    let turn_events: Vec<BackendEvent> = events
        .iter()
        .filter(|e| e.turn().is_some())
        .cloned()
        .collect();
    check_turn_stream(&turn_events, Protocol::Acp).expect("turn stream obeys the seam contract");
}

fn until_permission(rx: &Receiver<BackendEvent>) -> PermissionRequest {
    loop {
        match rx.recv_timeout(WAIT).expect("event before timeout") {
            BackendEvent::PermissionRequest { request, .. } => return request,
            BackendEvent::Terminal { .. } => panic!("turn ended before a permission request"),
            _ => {}
        }
    }
}

/// Like [`until_permission`], but keeps every event seen up to and including the request
/// so the whole turn stream can be checked against the contract.
fn until_permission_events(rx: &Receiver<BackendEvent>) -> (PermissionRequest, Vec<BackendEvent>) {
    let mut seen = Vec::new();
    loop {
        let event = rx.recv_timeout(WAIT).expect("event before timeout");
        if let BackendEvent::PermissionRequest { request, .. } = &event {
            let request = request.clone();
            seen.push(event);
            return (request, seen);
        }
        if matches!(event, BackendEvent::Terminal { .. }) {
            panic!("turn ended before a permission request");
        }
        seen.push(event);
    }
}

/// Polls the recorded driver frames until one matches (the fake reads on its own thread).
fn await_frame(rec: &Rec, pred: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..500 {
        if let Some(found) = lock(rec).iter().rev().find(|r| pred(r)).cloned() {
            return found;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("expected driver frame never arrived");
}

/// The driver's response to agent-issued request `id`.
fn response_to(rec: &Rec, id: i64) -> Value {
    await_frame(rec, |r| {
        r.get("id") == Some(&json!(id)) && r.get("method").is_none()
    })
}

#[test]
fn capabilities_0009_011() {
    let f = open(harness(agent), config());
    let caps = f.driver.capabilities();
    assert_eq!(caps.protocol, Protocol::Acp);
    assert!(caps.resume && caps.cancel && caps.streaming_text);
    assert_eq!(
        caps.usage_sources,
        vec![UsageSource::AcpUsage, UsageSource::DriverCounted]
    );
    assert_eq!(
        caps.approval_modes,
        vec!["default".to_owned(), "plan".to_owned()]
    );
}

#[test]
fn happy_turn_streams_text_usage_and_terminal_0009_006_0009_007_0009_012_0009_025_0009_055() {
    let mut f = open(harness(agent), config());
    f.driver.start(start("hello", "t1", None)).expect("start");
    let events = drain(&f.rx);
    assert_contract(&events);
    assert!(events.iter().any(|e| matches!(
        e,
        BackendEvent::Status { payload: StatusPayload::SessionBound(h), .. } if h.as_str() == "s-1"
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        BackendEvent::ArtifactDelta { text, append: true, last: false, .. } if text == "hello"
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        BackendEvent::Usage { sample, .. }
            if sample.source == UsageSource::AcpUsage
                && sample.tokens_in == Some(3)
                && sample.tokens_out == Some(4)
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        BackendEvent::Usage { sample, .. }
            if sample.source == UsageSource::DriverCounted && sample.turns == Some(1)
    )));
    assert_eq!(outcome(&events), Outcome::Completed);
}

#[test]
fn client_contract_0009_017_0009_018() {
    let mut f = open(harness(agent), config());
    f.driver.start(start("hello", "t1", None)).expect("start");
    drain(&f.rx);
    let init = await_frame(&f.rec, |r| method(r) == "initialize");
    let caps = &init["params"]["clientCapabilities"];
    assert_eq!(caps["fs"]["readTextFile"], json!(false));
    assert_eq!(caps["fs"]["writeTextFile"], json!(false));
    assert_eq!(caps["terminal"], json!(false));
    assert_eq!(caps["auth"]["terminal"], json!(false));
    assert!(caps["elicitation"]["form"].is_object());
    assert!(caps["elicitation"]["url"].is_null());
    let new = await_frame(&f.rec, |r| method(r) == "session/new");
    let cwd = new["params"]["cwd"].as_str().unwrap_or("");
    assert!(Path::new(cwd).is_absolute(), "cwd is absolute: {cwd}");
    assert!(new["params"]["mcpServers"].is_array());
}

#[test]
fn second_start_while_open_is_busy_0009_020() {
    let mut f = open(harness(agent), config());
    f.driver
        .start(start("hang", "t1", None))
        .expect("first start");
    assert_eq!(
        f.driver.start(start("hello", "t2", None)),
        Err(BackendError::Busy)
    );
    f.driver.cancel(&TurnRef::new("t1")).expect("cancel");
    let events = drain(&f.rx);
    assert_eq!(outcome(&events), Outcome::Canceled);
}

#[test]
fn stop_reasons_map_to_outcomes_0009_021_0009_012() {
    let mut f = open(harness(agent), config());
    let cases: [(&str, Outcome); 3] = [
        ("stop:end_turn", Outcome::Completed),
        ("stop:cancelled", Outcome::Canceled),
        (
            "stop:refusal",
            Outcome::Refused {
                reason: ReasonToken::AgentRefused,
                health_class: None,
            },
        ),
    ];
    for (i, (text, expected)) in cases.iter().enumerate() {
        f.driver
            .start(start(text, &format!("t{i}"), None))
            .expect("start");
        let events = drain(&f.rx);
        assert_contract(&events);
        assert_eq!(&outcome(&events), expected, "stop reason {text}");
    }
    for (i, text) in ["stop:max_tokens", "stop:weird"].iter().enumerate() {
        f.driver
            .start(start(text, &format!("u{i}"), None))
            .expect("start");
        let events = drain(&f.rx);
        let expected = if *text == "stop:max_tokens" {
            ReasonToken::AgentLimit
        } else {
            ReasonToken::BackendError
        };
        assert!(
            matches!(outcome(&events), Outcome::Failed { reason, .. } if reason == expected),
            "stop reason {text}"
        );
    }
    let sessions = lock(&f.rec)
        .iter()
        .filter(|r| method(r) == "session/new")
        .count();
    assert_eq!(sessions, 1, "one agent session per context_id");
}

#[test]
fn prompt_error_is_backend_error_and_redacted_0009_022() {
    let mut f = open(harness(agent), config());
    f.driver.start(start("error", "t1", None)).expect("start");
    let events = drain(&f.rx);
    match outcome(&events) {
        Outcome::Failed { reason, detail, .. } => {
            assert_eq!(reason, ReasonToken::BackendError);
            assert!(detail.contains("boom"), "detail: {detail}");
            assert!(
                !detail.contains("sk-secret12345"),
                "detail leaked: {detail}"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[test]
fn protocol_mismatch_closes_and_refuses_turns_0009_016() {
    let mut f = open(harness(wrong_version), config());
    f.driver.start(start("hello", "t1", None)).expect("start");
    let events = drain(&f.rx);
    assert!(events.iter().any(|e| matches!(
        e,
        BackendEvent::Fault {
            kind: FaultKind::UnsupportedProtocolVersion,
            ..
        }
    )));
    assert!(matches!(
        outcome(&events),
        Outcome::Failed {
            reason: ReasonToken::BackendUnavailable,
            ..
        }
    ));
    assert_eq!(
        f.driver.start(start("hello", "t2", None)),
        Err(BackendError::Closed)
    );
    // The agent exits on stdin close, so no tree kill is needed (0009-059).
    assert_eq!(f.counters.kills.load(Ordering::SeqCst), 0);
}

#[test]
fn undeclared_client_method_gets_method_not_found_0009_019() {
    let mut f = open(harness(agent), config());
    f.driver.start(start("fs", "t1", None)).expect("start");
    let events = drain(&f.rx);
    assert_eq!(outcome(&events), Outcome::Completed);
    let reply = response_to(&f.rec, 99);
    assert_eq!(reply["error"]["code"], json!(-32601));
}

#[test]
fn permission_round_trip_and_unoffered_answer_0009_032_0009_033_0009_034() {
    let mut f = open(harness(agent), config());
    f.driver.start(start("perm", "t1", None)).expect("start");
    let request = until_permission(&f.rx);
    assert_eq!(request.tool_kind, "execute");
    let offered: Vec<&str> = request
        .options
        .iter()
        .map(|o| o.option_id.as_str())
        .collect();
    assert_eq!(offered, ["allow-once", "allow-with-updates", "reject"]);
    assert_eq!(request.options[1].kind, "allow_always");
    f.driver
        .resolve_permission(
            &request.request_id,
            PermissionAnswer::Selected {
                option_id: "allow-once".to_owned(),
            },
        )
        .expect("resolve");
    let events = drain(&f.rx);
    assert_eq!(outcome(&events), Outcome::Completed);
    let answer = response_to(&f.rec, 50);
    assert_eq!(answer["result"]["outcome"]["outcome"], json!("selected"));
    assert_eq!(answer["result"]["outcome"]["optionId"], json!("allow-once"));

    f.driver.start(start("perm2", "t2", None)).expect("start");
    let request = until_permission(&f.rx);
    f.driver
        .resolve_permission(
            &request.request_id,
            PermissionAnswer::Selected {
                option_id: "bogus".to_owned(),
            },
        )
        .expect("resolve");
    drain(&f.rx);
    let answer = response_to(&f.rec, 51);
    assert_eq!(answer["result"]["outcome"]["outcome"], json!("cancelled"));
}

#[test]
fn permission_timeout_cancels_0009_033() {
    let mut cfg = config();
    cfg.permission_timeout = Duration::from_millis(300);
    let mut f = open(harness(agent), cfg);
    f.driver.start(start("perm", "t1", None)).expect("start");
    let events = drain(&f.rx);
    assert_eq!(outcome(&events), Outcome::Completed);
    let answer = response_to(&f.rec, 50);
    assert_eq!(answer["result"]["outcome"]["outcome"], json!("cancelled"));
}

#[test]
fn unknown_permission_answer_is_ignored_0009_010() {
    let mut f = open(harness(agent), config());
    f.driver
        .resolve_permission(&RequestId::new("nope"), PermissionAnswer::Cancelled)
        .expect("ignored");
    f.driver.start(start("hello", "t1", None)).expect("start");
    let events = drain(&f.rx);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, BackendEvent::Fault { .. }))
    );
    assert_eq!(outcome(&events), Outcome::Completed);
}

#[test]
fn form_elicitation_accept_and_decline_0009_082() {
    let mut f = open(harness(agent), config());
    f.driver.start(start("elic", "t1", None)).expect("start");
    let request = until_permission(&f.rx);
    assert_eq!(request.tool_kind, "form-elicitation");
    assert!(request.tool_title.contains("write"));
    let kinds: Vec<&str> = request.options.iter().map(|o| o.kind.as_str()).collect();
    assert_eq!(kinds, ["allow_once", "reject_once"]);
    f.driver
        .resolve_permission(
            &request.request_id,
            PermissionAnswer::Selected {
                option_id: "allow-once".to_owned(),
            },
        )
        .expect("resolve");
    assert_eq!(outcome(&drain(&f.rx)), Outcome::Completed);
    let answer = response_to(&f.rec, 60);
    assert_eq!(
        answer["result"],
        json!({"action": "accept", "content": {"value": "Approve Once"}})
    );

    f.driver
        .start(start("elic-bad", "t2", None))
        .expect("start");
    let events = drain(&f.rx);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, BackendEvent::PermissionRequest { .. }))
    );
    assert_eq!(outcome(&events), Outcome::Completed);
    let answer = response_to(&f.rec, 61);
    assert_eq!(answer["result"], json!({"action": "decline"}));
}

#[test]
fn cancel_answers_pending_work_and_ends_once_0009_009_0009_035() {
    let mut f = open(harness(agent), config());
    f.driver.start(start("hang", "t1", None)).expect("start");
    let (_request, mut events) = until_permission_events(&f.rx);
    let turn = TurnRef::new("t1");
    f.driver.cancel(&turn).expect("first cancel");
    f.driver.cancel(&turn).expect("second cancel is idempotent");
    events.extend(drain(&f.rx));
    assert_contract(&events);
    assert_eq!(outcome(&events), Outcome::Canceled);
    let terminals = events
        .iter()
        .filter(|e| matches!(e, BackendEvent::Terminal { .. }))
        .count();
    assert_eq!(terminals, 1, "exactly one Terminal (0009-009)");
    assert!(events.iter().any(|e| matches!(
        e,
        BackendEvent::Status { payload: StatusPayload::Progress(text), .. } if text.contains("\"failed\"")
    )));
    let answer = response_to(&f.rec, 50);
    assert_eq!(answer["result"]["outcome"]["outcome"], json!("cancelled"));
    await_frame(&f.rec, |r| method(r) == "session/cancel");
}

#[test]
fn cancel_grace_expiry_kills_tree_0009_036() {
    let mut f = open(harness(agent).hanging(), config());
    f.driver.start(start("silent", "t1", None)).expect("start");
    await_frame(&f.rec, |r| method(r) == "session/prompt");
    f.driver.cancel(&TurnRef::new("t1")).expect("cancel");
    let events = drain(&f.rx);
    assert_eq!(outcome(&events), Outcome::Canceled);
    assert_eq!(f.counters.kills.load(Ordering::SeqCst), 1);
}

#[test]
fn idle_timeout_fails_turn_backend_timeout_0009_024() {
    let mut cfg = config();
    cfg.turn_idle_timeout = Duration::from_millis(300);
    let mut f = open(harness(agent), cfg);
    f.driver.start(start("silent", "t1", None)).expect("start");
    let events = drain(&f.rx);
    assert_contract(&events);
    assert!(matches!(
        outcome(&events),
        Outcome::Failed {
            reason: ReasonToken::BackendTimeout,
            ..
        }
    ));
}

#[test]
fn crash_mid_turn_is_backend_crashed_and_not_replayed_0009_014_0009_023() {
    let mut f = open(harness(agent), config());
    f.driver.start(start("crash", "t1", None)).expect("start");
    let events = drain(&f.rx);
    assert!(matches!(
        outcome(&events),
        Outcome::Failed {
            reason: ReasonToken::BackendCrashed,
            ..
        }
    ));
    f.driver.start(start("hello", "t2", None)).expect("restart");
    let events = drain(&f.rx);
    assert_eq!(outcome(&events), Outcome::Completed);
    assert_eq!(f.counters.spawns.load(Ordering::SeqCst), 2);
    let crash_prompts = lock(&f.rec)
        .iter()
        .filter(|r| method(r) == "session/prompt" && prompt_text(r) == "crash")
        .count();
    assert_eq!(crash_prompts, 1, "the in-flight turn is never replayed");
}

#[test]
fn restore_uses_session_load_when_advertised_0009_013() {
    let mut f = open(harness(agent), config());
    f.driver
        .start(start("hello", "t1", Some("s-old")))
        .expect("start");
    let events = drain(&f.rx);
    assert_eq!(outcome(&events), Outcome::Completed);
    assert!(events.iter().any(|e| matches!(
        e,
        BackendEvent::Status { payload: StatusPayload::SessionBound(h), .. } if h.as_str() == "s-old"
    )));
    let rec = lock(&f.rec);
    assert!(rec.iter().any(|r| method(r) == "session/load"));
    assert!(!rec.iter().any(|r| method(r) == "session/new"));
}

/// A peer-tools binding with fixed base64url-shaped fields and absolute paths.
fn peer() -> PeerTools {
    PeerTools {
        slot: "c2xvdA".to_owned(),
        grant_id: "Z3JhbnQ".to_owned(),
        grant_credential: SecretText::new("Y3JlZA"),
        endpoint: std::env::temp_dir().join("a2abus-endpoint"),
        shim_path: std::env::temp_dir().join("a2abus-mcp-shim"),
    }
}

/// The exact RFC-0015 [0015-084] `mcpServers` array for [`peer`].
fn expected_servers(peer: &PeerTools) -> Value {
    json!([{
        "name": "a2abus",
        "command": peer.shim_path.to_string_lossy(),
        "args": [
            "--grant", "Z3JhbnQ",
            "--endpoint", peer.endpoint.to_string_lossy(),
            "--slot", "c2xvdA",
        ],
        "env": [{"name": "A2ABUS_MCP_GRANT", "value": "Y3JlZA"}],
    }])
}

fn start_with_peer(
    text: &str,
    turn: &str,
    session: Option<&str>,
    peer_tools: Option<PeerTools>,
) -> StartTurn {
    StartTurn {
        peer_tools,
        ..start(text, turn, session)
    }
}

#[test]
fn peer_tools_entry_reaches_session_new_0015_084() {
    let peer = peer();
    let mut f = open(harness(agent), config());
    f.driver
        .start(start_with_peer("hello", "t1", None, Some(peer.clone())))
        .expect("start");
    drain(&f.rx);
    let new = await_frame(&f.rec, |r| method(r) == "session/new");
    assert_eq!(new["params"]["mcpServers"], expected_servers(&peer));
}

#[test]
fn peer_tools_entry_reaches_session_load_0015_084() {
    let peer = peer();
    let mut f = open(harness(agent), config());
    f.driver
        .start(start_with_peer(
            "hello",
            "t1",
            Some("s-old"),
            Some(peer.clone()),
        ))
        .expect("start");
    drain(&f.rx);
    let load = await_frame(&f.rec, |r| method(r) == "session/load");
    assert_eq!(load["params"]["mcpServers"], expected_servers(&peer));
    assert_eq!(load["params"]["sessionId"], "s-old");
}

#[test]
fn peer_tools_none_passes_empty_servers_0015_085() {
    let mut f = open(harness(agent), config());
    f.driver.start(start("hello", "t1", None)).expect("start");
    drain(&f.rx);
    let new = await_frame(&f.rec, |r| method(r) == "session/new");
    assert_eq!(new["params"]["mcpServers"], json!([]));
}

#[test]
fn peer_tools_with_operator_servers_refused_0015_084() {
    let mut cfg = config();
    cfg.mcp_servers = vec![json!({"name": "operator"})];
    let mut f = open(harness(agent), cfg);
    let result = f
        .driver
        .start(start_with_peer("hello", "t1", None, Some(peer())));
    assert!(matches!(result, Err(BackendError::Unsupported(_))));
    let event = f.rx.recv_timeout(WAIT).expect("fault event");
    assert!(matches!(
        event,
        BackendEvent::Fault {
            kind: FaultKind::ConfigRefused,
            ..
        }
    ));
    assert!(!lock(&f.rec).iter().any(|r| method(r) == "initialize"));
}

#[test]
fn peer_tools_binding_change_refused_0015_084() {
    let mut f = open(harness(agent), config());
    f.driver
        .start(start_with_peer("hello", "t1", None, Some(peer())))
        .expect("first start");
    drain(&f.rx);
    let second = f.driver.start(start_with_peer("again", "t2", None, None));
    assert!(matches!(second, Err(BackendError::Unsupported(_))));
}

#[test]
fn restore_not_found_becomes_new_session_with_notice_0009_013() {
    let mut f = open(harness(load_not_found), config());
    f.driver
        .start(start("hello", "t1", Some("s-old")))
        .expect("start");
    let events = drain(&f.rx);
    assert_eq!(outcome(&events), Outcome::Completed);
    assert!(events.iter().any(|e| matches!(
        e,
        BackendEvent::Status {
            payload: StatusPayload::Notice(_),
            ..
        }
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        BackendEvent::Status { payload: StatusPayload::SessionBound(h), .. } if h.as_str() == "s-1"
    )));
    assert!(lock(&f.rec).iter().any(|r| method(r) == "session/new"));
}

#[test]
fn config_refused_at_start_0009_064() {
    let mut cfg = config();
    cfg.approval_mode = "bypass".to_owned();
    let mut f = open(harness(agent), cfg);
    let result = f.driver.start(start("hello", "t1", None));
    assert!(matches!(result, Err(BackendError::Unsupported(_))));
    let event = f.rx.recv_timeout(WAIT).expect("fault event");
    assert!(matches!(
        event,
        BackendEvent::Fault {
            kind: FaultKind::ConfigRefused,
            ..
        }
    ));
    assert_eq!(f.counters.spawns.load(Ordering::SeqCst), 0);
}

#[test]
fn dangerous_mode_switch_is_refused_and_cancels_0009_067() {
    let mut f = open(harness(agent), config());
    f.driver.start(start("switch", "t1", None)).expect("start");
    let events = drain(&f.rx);
    assert!(events.iter().any(|e| matches!(
        e,
        BackendEvent::Fault {
            kind: FaultKind::ConfigRefused,
            ..
        }
    )));
    assert_eq!(outcome(&events), Outcome::Canceled);
}

#[test]
fn shutdown_cancels_turn_and_kills_tree_0009_059_0009_060() {
    let mut f = open(harness(agent).hanging(), config());
    f.driver.start(start("silent", "t1", None)).expect("start");
    await_frame(&f.rec, |r| method(r) == "session/prompt");
    f.driver
        .shutdown(Duration::from_millis(100))
        .expect("shutdown");
    let events = drain(&f.rx);
    assert_eq!(outcome(&events), Outcome::Canceled);
    assert_eq!(f.counters.kills.load(Ordering::SeqCst), 1);
    assert_eq!(
        f.driver.start(start("hello", "t2", None)),
        Err(BackendError::Closed)
    );
}

/// omp-style session: a `write` to the `xd://mcp__a2abus_a2a_send` route under `always-ask-cached`
/// is relayed as a form elicitation; the tool then completes and the turn ends.
fn omp_mcp_write(rec: &Rec, frame: &Value) -> Step {
    if let Some(frames) = base(frame) {
        return Step::Emit(frames);
    }
    match method(frame) {
        "session/prompt" => Step::Emit(vec![
            session_update(
                "s-1",
                json!({"sessionUpdate": "tool_call", "toolCallId": "w1", "kind": "edit",
                       "title": "write xd://mcp__a2abus_a2a_send", "status": "pending"}),
            ),
            request(
                70,
                "elicitation/create",
                elicitation_params("Allow tool: write xd://mcp__a2abus_a2a_send"),
            ),
        ]),
        // The driver's answer to the elicitation: the tool runs, then the turn finishes.
        "" => {
            let mut frames = vec![
                session_update(
                    "s-1",
                    json!({"sessionUpdate": "tool_call_update", "toolCallId": "w1",
                           "kind": "edit", "status": "completed"}),
                ),
                session_update(
                    "s-1",
                    json!({"sessionUpdate": "agent_message_chunk",
                           "content": {"type": "text", "text": "done"}}),
                ),
            ];
            if let Step::Emit(more) = finish(rec, "end_turn") {
                frames.extend(more);
            }
            Step::Emit(frames)
        }
        _ => Step::Emit(vec![]),
    }
}

#[test]
fn omp_mcp_route_write_approval_keeps_driver_live_0009_024_0009_032() {
    let mut cfg = config();
    cfg.turn_idle_timeout = Duration::from_millis(300);
    let mut f = open(harness(omp_mcp_write), cfg);
    f.driver
        .start(start("mcp-write", "t1", None))
        .expect("start");
    let (request, mut seen) = until_permission_events(&f.rx);
    assert_eq!(request.tool_kind, "form-elicitation");
    assert!(request.tool_title.contains("xd://mcp__a2abus_a2a_send"));
    // The approval outlasts the idle limit: the driver stays live, the turn stays open.
    thread::sleep(Duration::from_millis(600));
    let health = f.driver.health();
    assert!(health.live);
    assert!(health.reason.is_none());
    f.driver
        .resolve_permission(
            &request.request_id,
            PermissionAnswer::Selected {
                option_id: "allow-once".to_owned(),
            },
        )
        .expect("resolve");
    seen.extend(drain(&f.rx));
    assert_eq!(outcome(&seen), Outcome::Completed);
    assert!(!seen.iter().any(|e| matches!(e, BackendEvent::Fault { .. })));
    let answer = response_to(&f.rec, 70);
    assert_eq!(
        answer["result"],
        json!({"action": "accept", "content": {"value": "Approve Once"}})
    );
    assert!(seen.iter().any(|e| matches!(
        e,
        BackendEvent::Status { payload: StatusPayload::Progress(p), .. } if p.contains("\"kind\":\"edit\"") && p.contains("mcp__a2abus_a2a_send")
    )));
}

#[test]
fn approval_wait_is_bounded_by_permission_timeout_not_idle_0009_024_0009_033() {
    let mut cfg = config();
    cfg.turn_idle_timeout = Duration::from_millis(200);
    cfg.permission_timeout = Duration::from_millis(600);
    let mut f = open(harness(agent), cfg);
    f.driver.start(start("perm", "t1", None)).expect("start");
    let events = drain(&f.rx);
    assert_eq!(outcome(&events), Outcome::Completed);
    let answer = response_to(&f.rec, 50);
    assert_eq!(answer["result"]["outcome"]["outcome"], json!("cancelled"));
}
