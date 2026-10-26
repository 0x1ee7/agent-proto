#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Offline tests: `HttpBackend` against an in-process fake Hermes A2A server on loopback.

use std::future::pending;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use a2a_worker::backend::{
    Backend, BackendError, BackendEvent, ChannelSink, FaultKind, HealthClass, InputPart, Outcome,
    Protocol, ReasonToken, StartTurn, TurnRef,
};
use a2a_worker::backend_contract::{check_all_turns_closed, check_turn_stream};
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use driver_a2a_http::{HttpBackend, PeerConfig};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::runtime::Runtime;

const TOKEN_ENV: &str = "TEST_PEER_TOKEN";
const TOKEN: &str = "spike-token";
const WORKING: &str = "TASK_STATE_WORKING";
const COMPLETED: &str = "TASK_STATE_COMPLETED";

#[derive(Clone, Debug)]
struct Seen {
    method: Method,
    path: String,
    headers: HeaderMap,
    body: Value,
}

impl Seen {
    fn rpc_method(&self) -> &str {
        self.body["method"].as_str().unwrap_or("")
    }

    fn peer_ctx(&self) -> String {
        self.body["params"]["message"]["contextId"]
            .as_str()
            .unwrap_or("")
            .to_owned()
    }

    fn rpc_id(&self) -> Value {
        self.body["id"].clone()
    }
}

enum Reply {
    Json(u16, Value),
    Raw(u16, &'static str),
    Sse(Vec<Value>),
    Hang,
}

type Handler = Arc<dyn Fn(&Seen) -> Reply + Send + Sync>;
type CardFn = fn(&str) -> Value;

#[derive(Clone)]
struct Fake {
    card: Option<Value>,
    handler: Handler,
    seen: Arc<Mutex<Vec<Seen>>>,
}

fn record(fake: &Fake, method: Method, uri: Uri, headers: HeaderMap, body: Value) -> Seen {
    let seen = Seen {
        method,
        path: uri.path().to_owned(),
        headers,
        body,
    };
    fake.seen.lock().expect("seen lock").push(seen.clone());
    seen
}

async fn card_route(
    State(fake): State<Fake>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    record(&fake, method, uri, headers, Value::Null);
    match &fake.card {
        Some(card) => axum::Json(card.clone()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn rpc_route(
    State(fake): State<Fake>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let seen = record(&fake, method, uri, headers, value);
    match (fake.handler)(&seen) {
        Reply::Json(status, value) => (status_code(status), axum::Json(value)).into_response(),
        Reply::Raw(status, text) => (
            status_code(status),
            [("content-type", "application/json")],
            text,
        )
            .into_response(),
        Reply::Sse(results) => {
            let mut text = String::new();
            for result in results {
                let frame = json!({"jsonrpc": "2.0", "id": seen.rpc_id(), "result": result});
                text.push_str(&format!("data: {frame}\n\n"));
            }
            text.push_str(": done\n\n");
            ([("content-type", "text/event-stream")], text).into_response()
        }
        Reply::Hang => pending::<Response>().await,
    }
}

fn status_code(status: u16) -> StatusCode {
    StatusCode::from_u16(status).expect("valid status")
}

struct Server {
    _rt: Runtime,
    base: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Server {
    fn start(
        card: Option<CardFn>,
        handler: impl Fn(&Seen) -> Reply + Send + Sync + 'static,
    ) -> Self {
        let rt = Runtime::new().expect("runtime");
        let listener = rt
            .block_on(TcpListener::bind("127.0.0.1:0"))
            .expect("bind loopback");
        let addr: SocketAddr = listener.local_addr().expect("local addr");
        let base = format!("http://{addr}/");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let fake = Fake {
            card: card.map(|build| build(&base)),
            handler: Arc::new(handler),
            seen: seen.clone(),
        };
        let app = Router::new()
            .route("/.well-known/agent.json", get(card_route))
            .route("/.well-known/agent-card.json", get(card_route))
            .fallback(rpc_route)
            .with_state(fake);
        rt.spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            _rt: rt,
            base,
            seen,
        }
    }

    fn requests(&self) -> Vec<Seen> {
        self.seen.lock().expect("seen lock").clone()
    }

    fn count(&self, method: &str) -> usize {
        self.requests()
            .iter()
            .filter(|s| s.rpc_method() == method)
            .count()
    }
}

fn card_streaming(base: &str) -> Value {
    json!({
        "name": "hermes",
        "supportedInterfaces": [{"url": base, "protocolBinding": "JSONRPC", "protocolVersion": "1.0"}],
        "capabilities": {"streaming": true},
    })
}

fn card_v03(base: &str) -> Value {
    json!({
        "name": "hermes",
        "supportedInterfaces": [{"url": base, "protocolBinding": "JSONRPC", "protocolVersion": "0.3"}],
        "capabilities": {"streaming": false},
    })
}

fn settings(base: &str, extra: Value) -> driver_a2a_http::PeerSettings {
    let mut config = json!({
        "agent_id": "hermes-peer",
        "base_url": base,
        "bearer_token_env": TOKEN_ENV,
    });
    if let (Some(map), Value::Object(more)) = (config.as_object_mut(), extra) {
        map.extend(more);
    }
    let peer: PeerConfig = serde_json::from_value(config).expect("peer config");
    peer.validate(|name| (name == TOKEN_ENV).then(|| TOKEN.to_owned()))
        .expect("settings")
}

fn backend(base: &str, extra: Value) -> (HttpBackend, Receiver<BackendEvent>) {
    let mut backend = HttpBackend::connect(settings(base, extra)).expect("connect");
    let (sink, rx) = ChannelSink::channel();
    backend.attach(Box::new(sink));
    (backend, rx)
}

fn start_turn(turn: &str, text: &str) -> StartTurn {
    StartTurn {
        turn: TurnRef::new(turn),
        context_id: "ctx-bus-1".to_owned(),
        input: vec![InputPart::Text(text.to_owned())],
        deadline_ms: 0,
        session: None,
    }
}

fn start(backend: &mut HttpBackend, turn: &str, text: &str) {
    backend
        .start(start_turn(turn, text))
        .expect("start accepted");
}

fn collect(rx: &Receiver<BackendEvent>, turn: &str) -> Vec<BackendEvent> {
    let mut events = Vec::new();
    loop {
        let event = rx
            .recv_timeout(Duration::from_secs(30))
            .expect("event within 30 s");
        let last = matches!(&event, BackendEvent::Terminal { turn: t, .. } if t.as_str() == turn);
        events.push(event);
        if last {
            return events;
        }
    }
}

fn terminal(events: &[BackendEvent]) -> Outcome {
    events
        .iter()
        .find_map(|event| match event {
            BackendEvent::Terminal { outcome, .. } => Some(outcome.clone()),
            _ => None,
        })
        .expect("terminal event")
}

fn failed_reason(events: &[BackendEvent]) -> ReasonToken {
    match terminal(events) {
        Outcome::Failed { reason, .. } => reason,
        other => panic!("expected Failed, got {other:?}"),
    }
}

fn artifact_text(events: &[BackendEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            BackendEvent::ArtifactDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn artifact(text: &str) -> Value {
    json!([{"artifactId": "a1", "parts": [{"text": text, "mediaType": "text/plain"}]}])
}

fn unexpected() -> Reply {
    Reply::Json(
        500,
        json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32603, "message": "unexpected request"}}),
    )
}

/// Reply carrying a `result.task` whose status text and artifacts are given.
fn task_reply(seen: &Seen, state: &str, text: &str, artifacts: Value) -> Reply {
    Reply::Json(
        200,
        json!({"jsonrpc": "2.0", "id": seen.rpc_id(), "result": {"task": {
            "id": "t1",
            "contextId": seen.peer_ctx(),
            "status": {"state": state, "message": {"role": "ROLE_AGENT", "parts": [{"text": text}]}},
            "artifacts": artifacts,
        }}}),
    )
}

fn rpc_error(seen: &Seen, code: i64, message: &str) -> Reply {
    Reply::Json(
        200,
        json!({"jsonrpc": "2.0", "id": seen.rpc_id(), "error": {"code": code, "message": message}}),
    )
}

fn streamed_frames(ctx: &str) -> Vec<Value> {
    vec![
        json!({"task": {"id": "t1", "contextId": ctx, "status": {"state": "TASK_STATE_SUBMITTED"}}}),
        json!({"statusUpdate": {"taskId": "t1", "contextId": ctx, "status": {"state": WORKING}}}),
        json!({"artifactUpdate": {"taskId": "t1", "contextId": ctx,
            "artifact": {"artifactId": "a1", "parts": [{"text": "stream "}]},
            "append": false, "lastChunk": false}}),
        json!({"artifactUpdate": {"taskId": "t1", "contextId": ctx,
            "artifact": {"artifactId": "a1", "parts": [{"text": "back"}]},
            "append": true, "lastChunk": true}}),
        json!({"statusUpdate": {"taskId": "t1", "contextId": ctx, "status": {"state": COMPLETED}}}),
    ]
}

#[test]
fn req_0009_043_unary_send_completes_and_meets_contract() {
    let server = Server::start(None, |seen| match seen.rpc_method() {
        "SendMessage" => task_reply(seen, COMPLETED, "", artifact("hello back")),
        _ => unexpected(),
    });
    let (mut backend, rx) = backend(&server.base, json!({}));

    start(&mut backend, "turn-1", "hello");
    let first = collect(&rx, "turn-1");
    check_turn_stream(&first, Protocol::A2aHttp).expect("turn 1 contract");
    check_all_turns_closed(&first).expect("turn 1 closed");
    assert_eq!(artifact_text(&first), "hello back");
    assert_eq!(terminal(&first), Outcome::Completed);

    start(&mut backend, "turn-2", "again");
    let second = collect(&rx, "turn-2");
    check_turn_stream(&second, Protocol::A2aHttp).expect("turn 2 contract");
    assert_eq!(terminal(&second), Outcome::Completed);

    let requests = server.requests();
    let sends: Vec<&Seen> = requests
        .iter()
        .filter(|s| s.rpc_method() == "SendMessage")
        .collect();
    assert_eq!(sends.len(), 2);
    for (send, text) in sends.iter().zip(["hello", "again"]) {
        assert_eq!(
            send.headers
                .get("a2a-version")
                .and_then(|v| v.to_str().ok()),
            Some("1.0")
        );
        assert_eq!(
            send.headers
                .get("authorization")
                .and_then(|v| v.to_str().ok()),
            Some("Bearer spike-token")
        );
        assert!(send.body["id"].as_str().unwrap_or("").starts_with("task-"));
        assert!(send.body["params"].get("tenant").is_none());
        let message = &send.body["params"]["message"];
        assert_eq!(message["role"], "ROLE_USER");
        assert_eq!(message["parts"][0]["text"], text);
        assert_eq!(message["messageId"].as_str().map(str::len), Some(32));
    }
    let ctx_first = sends[0].peer_ctx();
    assert!(ctx_first.starts_with("ctx-"));
    assert_eq!(
        ctx_first,
        sends[1].peer_ctx(),
        "same bus context keeps one peer context"
    );
    assert!(
        requests
            .iter()
            .filter(|s| s.method == Method::GET)
            .all(|s| s.headers.get("authorization").is_none()),
        "card fetch never carries the bearer"
    );
    assert!(requests.iter().any(|s| s.path.starts_with("/.well-known/")));
}

#[test]
fn req_0009_047_input_required_then_completed() {
    let server = Server::start(None, |seen| match seen.rpc_method() {
        "SendMessage" => task_reply(seen, "TASK_STATE_INPUT_REQUIRED", "which city?", json!([])),
        _ => unexpected(),
    });
    let (mut backend, rx) = backend(&server.base, json!({}));

    start(&mut backend, "turn-1", "weather");
    let events = collect(&rx, "turn-1");
    check_turn_stream(&events, Protocol::A2aHttp).expect("contract");
    assert!(events.iter().any(|e| matches!(
        e,
        BackendEvent::InputRequired { text, .. } if text.as_str() == "which city?"
    )));
    assert_eq!(terminal(&events), Outcome::Completed);
}

#[test]
fn req_0009_051_rejected_credentials_map_to_peer_auth_failed() {
    let server = Server::start(None, |_| {
        Reply::Json(
            401,
            json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32050, "message": "unauthorized"}}),
        )
    });
    let (mut backend, rx) = backend(&server.base, json!({}));

    start(&mut backend, "turn-1", "hi");
    let events = collect(&rx, "turn-1");
    check_turn_stream(&events, Protocol::A2aHttp).expect("contract");
    match terminal(&events) {
        Outcome::Failed {
            reason,
            health_class,
            detail,
        } => {
            assert_eq!(reason, ReasonToken::BackendError);
            assert_eq!(health_class, Some(HealthClass::PeerAuthFailed));
            assert!(!detail.contains(TOKEN), "detail never carries the token");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[test]
fn req_0009_051_rate_limit_maps_to_refused() {
    let server = Server::start(None, |_| {
        Reply::Json(
            429,
            json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32051, "message": "slow down"}}),
        )
    });
    let (mut backend, rx) = backend(&server.base, json!({}));

    start(&mut backend, "turn-1", "hi");
    let events = collect(&rx, "turn-1");
    assert_eq!(
        terminal(&events),
        Outcome::Refused {
            reason: ReasonToken::RateLimited,
            health_class: Some(HealthClass::PeerRateLimited),
        }
    );
}

#[test]
fn req_0009_050_lost_task_on_poll_maps_to_backend_lost_task() {
    let server = Server::start(None, |seen| match seen.rpc_method() {
        "SendMessage" => task_reply(seen, WORKING, "", json!([])),
        "GetTask" => rpc_error(seen, -32001, "task not found: t1"),
        _ => unexpected(),
    });
    let (mut backend, rx) = backend(&server.base, json!({}));

    start(&mut backend, "turn-1", "hi");
    let events = collect(&rx, "turn-1");
    check_turn_stream(&events, Protocol::A2aHttp).expect("contract");
    assert_eq!(failed_reason(&events), ReasonToken::BackendLostTask);
}

#[test]
fn req_0009_049_polling_reaches_completed_and_emits_artifact() {
    let polls = Arc::new(AtomicUsize::new(0));
    let counter = polls.clone();
    let server = Server::start(None, move |seen| match seen.rpc_method() {
        "SendMessage" => task_reply(seen, WORKING, "", json!([])),
        "GetTask" => {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                task_reply(seen, WORKING, "", json!([]))
            } else {
                task_reply(seen, COMPLETED, "", artifact("polled"))
            }
        }
        _ => unexpected(),
    });
    let (mut backend, rx) = backend(&server.base, json!({}));

    start(&mut backend, "turn-1", "hi");
    let events = collect(&rx, "turn-1");
    check_turn_stream(&events, Protocol::A2aHttp).expect("contract");
    assert_eq!(artifact_text(&events), "polled");
    assert_eq!(terminal(&events), Outcome::Completed);
    assert_eq!(server.count("GetTask"), 2);
}

#[test]
fn req_0009_046_foreign_context_fails_turn() {
    let server = Server::start(None, |seen| match seen.rpc_method() {
        "SendMessage" => Reply::Json(
            200,
            json!({"jsonrpc": "2.0", "id": seen.rpc_id(), "result": {"task": {
                "id": "t1", "contextId": "ctx-foreign",
                "status": {"state": COMPLETED}, "artifacts": []
            }}}),
        ),
        _ => unexpected(),
    });
    let (mut backend, rx) = backend(&server.base, json!({}));

    start(&mut backend, "turn-1", "hi");
    let events = collect(&rx, "turn-1");
    assert!(events.iter().any(|e| matches!(
        e,
        BackendEvent::Fault {
            kind: FaultKind::Internal,
            ..
        }
    )));
    assert_eq!(failed_reason(&events), ReasonToken::BackendError);
}

#[test]
fn req_0008_098_non_object_body_is_malformed() {
    let server = Server::start(None, |_| Reply::Raw(200, "[1,2]"));
    let (mut backend, rx) = backend(&server.base, json!({}));

    start(&mut backend, "turn-1", "hi");
    let events = collect(&rx, "turn-1");
    assert_eq!(failed_reason(&events), ReasonToken::BackendError);
}

#[test]
fn req_0008_078_reply_timeout_marker_maps_to_backend_timeout() {
    let server = Server::start(None, |seen| match seen.rpc_method() {
        "SendMessage" => task_reply(
            seen,
            "TASK_STATE_FAILED",
            "[agent did not reply in time]",
            json!([]),
        ),
        _ => unexpected(),
    });
    let (mut backend, rx) = backend(&server.base, json!({}));

    start(&mut backend, "turn-1", "hi");
    let events = collect(&rx, "turn-1");
    assert_eq!(failed_reason(&events), ReasonToken::BackendTimeout);
}

#[test]
fn req_0009_047_cancel_after_working_sends_cancel_task() {
    let server = Server::start(None, |seen| match seen.rpc_method() {
        "SendMessage" | "GetTask" => task_reply(seen, WORKING, "", json!([])),
        "CancelTask" => task_reply(seen, "TASK_STATE_CANCELED", "", json!([])),
        _ => unexpected(),
    });
    let (mut backend, rx) = backend(&server.base, json!({}));

    start(&mut backend, "turn-1", "hi");
    let first = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("first event");
    assert!(matches!(first, BackendEvent::Status { .. }));
    backend.cancel(&TurnRef::new("turn-1")).expect("cancel");

    let mut events = vec![first];
    events.extend(collect(&rx, "turn-1"));
    check_turn_stream(&events, Protocol::A2aHttp).expect("contract");
    assert_eq!(terminal(&events), Outcome::Canceled);
    let cancel = server
        .requests()
        .into_iter()
        .find(|s| s.rpc_method() == "CancelTask")
        .expect("CancelTask sent");
    assert_eq!(cancel.body["params"]["id"], "t1");
}

#[test]
fn req_0009_053_turn_timeout_fails_backend_timeout_and_cancels_remote() {
    let server = Server::start(None, |seen| match seen.rpc_method() {
        "SendMessage" | "GetTask" => task_reply(seen, WORKING, "", json!([])),
        "CancelTask" => task_reply(seen, "TASK_STATE_CANCELED", "", json!([])),
        _ => unexpected(),
    });
    let (mut backend, rx) = backend(&server.base, json!({"timeout_secs": 1}));

    start(&mut backend, "turn-1", "hi");
    let events = collect(&rx, "turn-1");
    check_turn_stream(&events, Protocol::A2aHttp).expect("contract");
    assert_eq!(failed_reason(&events), ReasonToken::BackendTimeout);
    assert_eq!(server.count("CancelTask"), 1);
}

#[test]
fn req_0008_078_unreachable_peer_maps_to_backend_crashed() {
    let closed = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = closed.local_addr().expect("addr");
    drop(closed);
    let (mut backend, rx) = backend(&format!("http://{addr}/"), json!({}));

    start(&mut backend, "turn-1", "hi");
    let events = collect(&rx, "turn-1");
    check_turn_stream(&events, Protocol::A2aHttp).expect("contract");
    assert_eq!(failed_reason(&events), ReasonToken::BackendCrashed);
}

#[test]
fn req_0008_071_version_mismatch_faults_and_turn_proceeds() {
    let server = Server::start(Some(card_v03), |seen| match seen.rpc_method() {
        "SendMessage" => task_reply(seen, COMPLETED, "", artifact("ok")),
        _ => unexpected(),
    });
    let (mut backend, rx) = backend(&server.base, json!({}));

    start(&mut backend, "turn-1", "hi");
    let events = collect(&rx, "turn-1");
    assert!(events.iter().any(|e| matches!(
        e,
        BackendEvent::Fault {
            kind: FaultKind::UnsupportedProtocolVersion,
            ..
        }
    )));
    assert_eq!(terminal(&events), Outcome::Completed);
}

#[test]
fn req_0009_043_streaming_completes_from_sse_without_polling() {
    let server = Server::start(Some(card_streaming), |seen| match seen.rpc_method() {
        "SendStreamingMessage" => Reply::Sse(streamed_frames(&seen.peer_ctx())),
        _ => unexpected(),
    });
    let (mut backend, rx) = backend(&server.base, json!({"streaming": true}));

    start(&mut backend, "turn-1", "hi");
    let events = collect(&rx, "turn-1");
    check_turn_stream(&events, Protocol::A2aHttp).expect("contract");
    assert_eq!(artifact_text(&events), "stream back");
    assert_eq!(terminal(&events), Outcome::Completed);
    assert_eq!(server.count("GetTask"), 0);
}

#[test]
fn req_0009_049_streaming_drop_resubscribes_with_subscribe_to_task() {
    let server = Server::start(Some(card_streaming), |seen| match seen.rpc_method() {
        "SendStreamingMessage" => Reply::Sse(vec![json!({"task": {
            "id": "t1", "contextId": seen.peer_ctx(),
            "status": {"state": "TASK_STATE_SUBMITTED"}
        }})]),
        "SubscribeToTask" => Reply::Sse(vec![json!({"statusUpdate": {
            "taskId": "t1", "status": {"state": COMPLETED}
        }})]),
        _ => unexpected(),
    });
    let (mut backend, rx) = backend(&server.base, json!({"streaming": true}));

    start(&mut backend, "turn-1", "hi");
    let events = collect(&rx, "turn-1");
    check_turn_stream(&events, Protocol::A2aHttp).expect("contract");
    assert_eq!(terminal(&events), Outcome::Completed);
    let subscribe = server
        .requests()
        .into_iter()
        .find(|s| s.rpc_method() == "SubscribeToTask")
        .expect("SubscribeToTask sent");
    assert_eq!(subscribe.body["params"]["id"], "t1");
}

#[test]
fn req_0009_020_second_start_is_busy_and_cancel_before_task_id_is_canceled() {
    let server = Server::start(None, |_| Reply::Hang);
    let (mut backend, rx) = backend(&server.base, json!({}));

    start(&mut backend, "turn-1", "hi");
    assert_eq!(
        backend.start(start_turn("turn-2", "hi")),
        Err(BackendError::Busy)
    );
    backend
        .cancel(&TurnRef::new("unknown"))
        .expect("unknown turn is ok");
    backend.cancel(&TurnRef::new("turn-1")).expect("cancel");

    let events = collect(&rx, "turn-1");
    check_turn_stream(&events, Protocol::A2aHttp).expect("contract");
    assert_eq!(terminal(&events), Outcome::Canceled);
    assert_eq!(
        server.count("CancelTask"),
        0,
        "no task id, so no CancelTask"
    );
}

#[test]
fn shutdown_ends_open_turn_refuses_start_and_reports_not_live() {
    let server = Server::start(None, |_| Reply::Hang);
    let (mut backend, rx) = backend(&server.base, json!({}));

    start(&mut backend, "turn-1", "hi");
    backend.shutdown(Duration::from_secs(5)).expect("shutdown");
    let events = collect(&rx, "turn-1");
    check_all_turns_closed(&events).expect("closed");
    assert_eq!(terminal(&events), Outcome::Canceled);
    assert_eq!(
        backend.start(start_turn("turn-2", "hi")),
        Err(BackendError::Closed)
    );
    assert!(!backend.health().live);
}

#[test]
fn start_without_sink_is_refused() {
    let server = Server::start(None, |_| unexpected());
    let mut backend = HttpBackend::connect(settings(&server.base, json!({}))).expect("connect");

    assert_eq!(
        backend.start(start_turn("turn-1", "hi")),
        Err(BackendError::NoSink)
    );
}
