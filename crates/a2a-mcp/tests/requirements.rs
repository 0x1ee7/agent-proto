//! Requirement tests for `a2a-mcp`: MCP stdio behaviour, the IPC client and
//! server over a real endpoint, and the shim binary. Test names cite the
//! RFC-0015 requirement IDs they exercise.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use a2a_mcp::client::{self, CallOutcome, ClientConfig};
use a2a_mcp::endpoint;
use a2a_mcp::ipc::{
    ClientFrame, DiscoverParams, Hello, HelloReply, IpcError, Provenance, Response, Secret,
    TaskResult,
};
use a2a_mcp::ipc_server::ServerRequest;
use a2a_mcp::redact::NoRedaction;
use a2a_mcp::server::{self, Options};
use a2a_mcp::testkit::{FakeWorker, TestGrant, b64url, endpoint_name};
use a2a_mcp::{GRANT_ENV, SERVER_NAME};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines, ReadHalf, WriteHalf};
use tokio::task::JoinHandle;

const GRANT_ID: [u8; 16] = [1; 16];
const CREDENTIAL: [u8; 32] = [2; 32];
const SLOT: [u8; 16] = [3; 16];
const WAIT: Duration = Duration::from_secs(10);

fn grant() -> TestGrant {
    TestGrant::new(&GRANT_ID, &CREDENTIAL, &SLOT)
}

fn slot() -> String {
    b64url(&SLOT)
}

fn credential() -> Secret {
    Secret::new(b64url(&CREDENTIAL))
}

fn client_config(endpoint: &str, credential: Option<Secret>) -> ClientConfig {
    ClientConfig {
        endpoint: endpoint.to_owned(),
        grant_id: b64url(&GRANT_ID),
        credential,
        session_slot: slot(),
    }
}

fn options() -> Options {
    Options {
        max_text_bytes: 65_536,
        redactor: Arc::new(NoRedaction),
        session_slot: slot(),
    }
}

fn task(text: &str) -> TaskResult {
    TaskResult {
        task_id: "task-1".to_owned(),
        context_id: "ctx-1".to_owned(),
        state: "COMPLETED".to_owned(),
        text: text.to_owned(),
        artifacts: Vec::new(),
        truncated: false,
        omitted_bytes: 0,
        provenance: Provenance {
            source_agent: "reviewer".to_owned(),
            task_id: "task-1".to_owned(),
            delegating_task_id: None,
            chain_length: 1,
        },
    }
}

fn respond_task(req: ServerRequest, result: TaskResult) {
    let id = req.frame.id();
    req.respond(Response::success(id, &result).expect("encode response"));
}

fn send_call(id: u64, arguments: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {"name": "a2a_send", "arguments": arguments},
    })
}

/// Reads one stdout line from the shim binary and checks it is an MCP frame.
async fn read_frame(stdout: &mut Lines<BufReader<tokio::process::ChildStdout>>) -> Value {
    let line = tokio::time::timeout(WAIT, stdout.next_line())
        .await
        .expect("stdout line in time")
        .expect("read stdout")
        .expect("stdout still open");
    let value: Value = serde_json::from_str(&line).expect("stdout line is JSON");
    assert_eq!(value["jsonrpc"], "2.0", "non-MCP stdout line: {line}");
    value
}

/// The shim running in-process over duplex pipes, driven like an MCP host.
struct Shim {
    stdin: WriteHalf<tokio::io::DuplexStream>,
    stdout: Lines<BufReader<ReadHalf<tokio::io::DuplexStream>>>,
    task: JoinHandle<io::Result<()>>,
}

impl Shim {
    fn start(endpoint: &str, credential: Option<Secret>) -> Self {
        let (host, shim_side) = tokio::io::duplex(1 << 21);
        let (host_read, host_write) = tokio::io::split(host);
        let (shim_read, shim_write) = tokio::io::split(shim_side);
        let client = client::spawn(client_config(endpoint, credential));
        let task = tokio::spawn(server::run(shim_read, shim_write, client, options()));
        Self {
            stdin: host_write,
            stdout: BufReader::new(host_read).lines(),
            task,
        }
    }

    async fn send(&mut self, value: &Value) {
        self.send_raw(&value.to_string()).await;
    }

    async fn send_raw(&mut self, line: &str) {
        self.stdin
            .write_all(line.as_bytes())
            .await
            .expect("write stdin");
        self.stdin.write_all(b"\n").await.expect("write newline");
        self.stdin.flush().await.expect("flush stdin");
    }

    async fn recv(&mut self) -> Value {
        self.recv_within(WAIT).await
    }

    async fn recv_within(&mut self, limit: Duration) -> Value {
        let line = tokio::time::timeout(limit, self.stdout.next_line())
            .await
            .expect("stdout line in time")
            .expect("read stdout")
            .expect("stdout still open");
        serde_json::from_str(&line).expect("stdout line is JSON")
    }

    /// Closes stdin (EOF) and returns the task that runs the server.
    async fn close(mut self) -> JoinHandle<io::Result<()>> {
        self.stdin.shutdown().await.expect("shutdown stdin");
        self.task
    }
}

// RFC-0015 [0015-014], [0015-015], [0015-012]: initialize is answered locally,
// within 500 ms, with no worker running.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn initialize_is_answered_locally_0015_012_014_015() {
    let mut shim = Shim::start(&endpoint_name("init"), Some(credential()));
    shim.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "0"},
        },
    }))
    .await;
    let reply = shim.recv_within(Duration::from_millis(500)).await;
    assert_eq!(reply["id"], 1);
    assert_eq!(reply["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(reply["result"]["capabilities"], json!({"tools": {}}));
    assert_eq!(reply["result"]["serverInfo"]["name"], SERVER_NAME);

    // A notification gets no reply; the next answer is the ping.
    shim.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .await;
    shim.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "ping"}))
        .await;
    let pong = shim.recv().await;
    assert_eq!(pong["id"], 2);
    assert_eq!(pong["result"], json!({}));
}

// RFC-0015 [0015-020], [0015-014]: tools/list answers locally with exactly
// four tools, each with additionalProperties false.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tools_list_is_local_and_exact_0015_014_020() {
    let mut shim = Shim::start(&endpoint_name("tools"), Some(credential()));
    shim.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
        .await;
    let reply = shim.recv_within(Duration::from_millis(500)).await;
    let tools = reply["result"]["tools"].as_array().expect("tools array");
    let names: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name"))
        .collect();
    assert_eq!(names, ["a2a_discover", "a2a_send", "a2a_get", "a2a_cancel"]);
    for tool in tools {
        assert_eq!(tool["inputSchema"]["additionalProperties"], false);
    }
}

// RFC-0015 [0015-013]: malformed JSON and unknown methods are JSON-RPC
// protocol errors; the session keeps going afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_json_and_unknown_method_are_protocol_errors_0015_013() {
    let mut shim = Shim::start(&endpoint_name("malformed"), Some(credential()));
    shim.send_raw("{not json").await;
    let parse_error = shim.recv().await;
    assert!(parse_error["id"].is_null());
    assert_eq!(parse_error["error"]["code"], -32700);

    shim.send(&json!({"jsonrpc": "2.0", "id": 3, "method": "resources/list"}))
        .await;
    let unknown = shim.recv().await;
    assert_eq!(unknown["id"], 3);
    assert_eq!(unknown["error"]["code"], -32601);
}

// RFC-0015 [0015-013]: an oversized line is refused with a null-id protocol
// error and the session continues.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversized_line_is_refused_and_session_continues_0015_013() {
    let mut shim = Shim::start(&endpoint_name("oversized"), Some(credential()));
    shim.send_raw(&"a".repeat(1_048_577)).await;
    let refused = shim.recv().await;
    assert!(refused["id"].is_null());
    assert_eq!(refused["error"]["code"], -32600);

    shim.send(&json!({"jsonrpc": "2.0", "id": 4, "method": "ping"}))
        .await;
    assert_eq!(shim.recv().await["id"], 4);
}

// RFC-0015 [0015-014]: an unknown tool name is an invalid-params protocol error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_tool_name_is_invalid_params_0015_014() {
    let mut shim = Shim::start(&endpoint_name("unknown-tool"), Some(credential()));
    shim.send(&json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "tools/call",
        "params": {"name": "a2a_missing", "arguments": {}},
    }))
    .await;
    let reply = shim.recv().await;
    assert_eq!(reply["id"], 5);
    assert_eq!(reply["error"]["code"], -32602);
}

// RFC-0015 [0015-021], [0015-016]: invalid arguments are a tool error; nothing
// reaches the worker.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invalid_send_arguments_are_tool_error_without_ipc_0015_016_021() {
    let ep = endpoint_name("bad-args");
    let worker = FakeWorker::start(&ep, grant()).expect("start worker");
    let mut shim = Shim::start(&ep, Some(credential()));
    shim.send(&send_call(6, json!({"agent": "Bad Agent", "text": "hi"})))
        .await;
    let reply = shim.recv().await;
    assert_eq!(reply["id"], 6);
    assert_eq!(reply["result"]["isError"], true);
    assert_eq!(
        reply["result"]["structuredContent"]["reason"],
        "MALFORMED_MESSAGE"
    );
    assert!(worker.next_request(Duration::from_millis(300)).is_none());
}

// RFC-0015 [0015-016], [0015-044], [0015-047], [0015-048]: a send round trip
// carries the idempotency key and the default deadline, and the child result
// comes back as a successful tool result.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_round_trip_carries_key_and_deadline_0015_016_044_047_048() {
    let ep = endpoint_name("send");
    let worker = FakeWorker::start(&ep, grant()).expect("start worker");
    let mut shim = Shim::start(&ep, Some(credential()));
    shim.send(&send_call(
        42,
        json!({"agent": "reviewer", "text": "review please"}),
    ))
    .await;

    let req = worker.next_request(WAIT).expect("send frame");
    {
        let ClientFrame::Send {
            deadline_ms,
            params,
            ..
        } = &req.frame
        else {
            panic!("expected send frame, got {:?}", req.frame);
        };
        assert_eq!(*deadline_ms, 3_600_000);
        assert_eq!(params.agent, "reviewer");
        assert_eq!(params.text, "review please");
        assert_eq!(params.idempotency_key, format!("42:{}", slot()));
    }
    respond_task(req, task("done"));

    let reply = shim.recv().await;
    assert_eq!(reply["id"], 42);
    assert_eq!(reply["result"]["isError"], false);
    assert_eq!(reply["result"]["content"][0]["text"], "done");
    assert_eq!(reply["result"]["structuredContent"]["taskId"], "task-1");
    assert_eq!(reply["result"]["structuredContent"]["truncated"], false);
}

// RFC-0015 [0015-032]: text above 65536 bytes is cut and flagged.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversized_result_text_is_capped_0015_032() {
    let ep = endpoint_name("cap");
    let worker = FakeWorker::start(&ep, grant()).expect("start worker");
    let mut shim = Shim::start(&ep, Some(credential()));
    shim.send(&send_call(7, json!({"agent": "reviewer", "text": "go"})))
        .await;

    let req = worker.next_request(WAIT).expect("send frame");
    respond_task(req, task(&"x".repeat(70_000)));

    let reply = shim.recv().await;
    let text = reply["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    assert_eq!(text.len(), 65_536);
    assert_eq!(reply["result"]["structuredContent"]["truncated"], true);
    assert_eq!(reply["result"]["structuredContent"]["omittedBytes"], 4_464);
}

// RFC-0015 [0015-016]: a child failure is an isError result carrying the
// child's reason and metadata.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn child_failure_is_tool_error_0015_016() {
    let ep = endpoint_name("child-failure");
    let worker = FakeWorker::start(&ep, grant()).expect("start worker");
    let mut shim = Shim::start(&ep, Some(credential()));
    shim.send(&send_call(8, json!({"agent": "reviewer", "text": "go"})))
        .await;

    let req = worker.next_request(WAIT).expect("send frame");
    let id = req.frame.id();
    let error = IpcError::new("TASK_NOT_FOUND").with_meta("taskId", "task-9");
    req.respond(Response::failure(id, error));

    let reply = shim.recv().await;
    assert_eq!(reply["id"], 8);
    assert!(reply.get("error").is_none());
    assert_eq!(reply["result"]["isError"], true);
    assert_eq!(
        reply["result"]["structuredContent"]["reason"],
        "TASK_NOT_FOUND"
    );
    assert_eq!(reply["result"]["structuredContent"]["taskId"], "task-9");
}

// RFC-0015 [0015-043]: a wrong credential is UNAUTHENTICATED; no request
// reaches the worker.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wrong_credential_is_unauthenticated_0015_042_043() {
    let ep = endpoint_name("auth");
    let worker = FakeWorker::start(&ep, grant()).expect("start worker");
    let mut shim = Shim::start(&ep, Some(Secret::new(b64url(&[9u8; 32]))));
    shim.send(&json!({
        "jsonrpc": "2.0",
        "id": 9,
        "method": "tools/call",
        "params": {"name": "a2a_discover", "arguments": {}},
    }))
    .await;
    let reply = shim.recv().await;
    assert_eq!(reply["id"], 9);
    assert_eq!(reply["result"]["isError"], true);
    assert_eq!(
        reply["result"]["structuredContent"]["reason"],
        "UNAUTHENTICATED"
    );
    assert!(worker.next_request(Duration::from_millis(300)).is_none());
}

// RFC-0015 [0015-040], [0015-041]: a hello with an unsupported ipcVersion is
// refused with IPC_VERSION_UNSUPPORTED.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unsupported_ipc_version_is_refused_0015_040_041() {
    let ep = endpoint_name("version");
    let _worker = FakeWorker::start(&ep, grant()).expect("start worker");
    let mut stream = endpoint::connect(&ep).await.expect("connect");
    endpoint::write_frame(
        &mut stream,
        &Hello {
            ipc_version: 99,
            shim: "test".to_owned(),
        },
    )
    .await
    .expect("write hello");
    let reply: HelloReply = endpoint::read_message(&mut stream)
        .await
        .expect("read hello reply");
    assert!(!reply.ok);
    assert_eq!(
        reply.error.expect("error reason").reason,
        "IPC_VERSION_UNSUPPORTED"
    );
}

// RFC-0015 [0015-044]: a call whose local deadline passes fails with
// DEADLINE_EXCEEDED.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_deadline_expiry_is_deadline_exceeded_0015_044() {
    let ep = endpoint_name("deadline");
    let _worker = FakeWorker::start(&ep, grant()).expect("start worker");
    let client = client::spawn(client_config(&ep, Some(credential())));
    let (_keep_alive, cancel) = tokio::sync::oneshot::channel::<()>();
    let frame = ClientFrame::Discover {
        id: 0,
        deadline_ms: 200,
        params: DiscoverParams {
            skill: None,
            tag: None,
            limit: None,
        },
    };
    let outcome = client
        .call(
            frame,
            tokio::time::Instant::now() + Duration::from_millis(200),
            cancel,
        )
        .await;
    match outcome {
        CallOutcome::Done(Err(error)) => assert_eq!(error.reason, "DEADLINE_EXCEEDED"),
        other => panic!("expected DEADLINE_EXCEEDED, got {other:?}"),
    }
}

// RFC-0015 [0015-018], [0015-046], [0015-069]: notifications/cancelled sends a
// cancel-call naming the pending call; the original reply is not delivered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_notification_sends_cancel_call_0015_018_046() {
    let ep = endpoint_name("cancel");
    let worker = FakeWorker::start(&ep, grant()).expect("start worker");
    let mut shim = Shim::start(&ep, Some(credential()));
    shim.send(&send_call(5, json!({"agent": "reviewer", "text": "slow"})))
        .await;

    let send_req = worker.next_request(WAIT).expect("send frame");
    let send_id = send_req.frame.id();

    shim.send(&json!({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": {"requestId": 5, "reason": "user"},
    }))
    .await;

    let cancel_req = worker.next_request(WAIT).expect("cancel-call frame");
    {
        let ClientFrame::CancelCall { call_id, .. } = &cancel_req.frame else {
            panic!("expected cancel-call frame, got {:?}", cancel_req.frame);
        };
        assert_eq!(*call_id, send_id);
    }
    let cancel_id = cancel_req.frame.id();
    cancel_req.respond(Response::success(cancel_id, &json!({})).expect("encode"));

    // The child answers late; the shim must not deliver that reply for id 5.
    respond_task(send_req, task("late"));

    shim.send(&json!({"jsonrpc": "2.0", "id": 6, "method": "ping"}))
        .await;
    let next = shim.recv().await;
    assert_eq!(next["id"], 6);
}

// RFC-0015 [0015-017]: stdin EOF cancels pending IPC calls and the run ends
// promptly with Ok.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stdin_eof_cancels_pending_and_exits_0015_017() {
    let ep = endpoint_name("eof");
    let worker = FakeWorker::start(&ep, grant()).expect("start worker");
    let mut shim = Shim::start(&ep, Some(credential()));
    shim.send(&send_call(10, json!({"agent": "reviewer", "text": "hang"})))
        .await;

    let pending = worker.next_request(WAIT).expect("send frame");
    let pending_id = pending.frame.id();

    let task = shim.close().await;
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("run returns within 2 s")
        .expect("server task joins")
        .expect("run returns Ok");

    let cancel = worker.next_request(WAIT).expect("cancel-call on EOF");
    match &cancel.frame {
        ClientFrame::CancelCall { call_id, .. } => assert_eq!(*call_id, pending_id),
        other => panic!("expected cancel-call, got {other:?}"),
    }
}

// RFC-0015 [0015-013], [0015-017]: the shim binary writes only MCP frames to
// stdout, keeps the credential out of stderr, and exits 0 on EOF.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shim_binary_stdout_is_mcp_only_and_exits_zero_0015_013_017() {
    let ep = endpoint_name("binary");
    let worker = FakeWorker::start(&ep, grant()).expect("start worker");
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_a2abus-mcp-shim"))
        .arg("--grant")
        .arg(b64url(&GRANT_ID))
        .arg("--endpoint")
        .arg(&ep)
        .arg("--slot")
        .arg(slot())
        .env(GRANT_ENV, b64url(&CREDENTIAL))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn shim");
    let mut stdin = child.stdin.take().expect("stdin pipe");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout pipe")).lines();
    let mut stderr = child.stderr.take().expect("stderr pipe");

    stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
        .await
        .expect("write initialize");
    let init = read_frame(&mut stdout).await;
    assert_eq!(init["id"], 1);

    stdin
        .write_all(
            format!(
                "{}\n",
                send_call(7, json!({"agent": "reviewer", "text": "bin"}))
            )
            .as_bytes(),
        )
        .await
        .expect("write send");
    let req = worker.next_request(WAIT).expect("send frame");
    respond_task(req, task("bin-done"));
    let reply = read_frame(&mut stdout).await;
    assert_eq!(reply["id"], 7);
    assert_eq!(reply["result"]["content"][0]["text"], "bin-done");

    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(3), child.wait())
        .await
        .expect("shim exits within 3 s")
        .expect("wait for shim");
    assert!(status.success(), "shim exit status: {status}");

    let mut err = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut stderr, &mut err)
        .await
        .expect("read stderr");
    assert!(err.len() <= 65_536, "stderr capped at 64 KiB");
    assert!(!String::from_utf8_lossy(&err).contains(&b64url(&CREDENTIAL)));
}
