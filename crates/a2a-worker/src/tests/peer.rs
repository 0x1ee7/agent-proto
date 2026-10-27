//! Two-Worker harness for RFC-0015 delegation tests.
//!
//! A parent and a child Worker share one bus-mem bus. The parent's peer tools are called over
//! the real IPC endpoint by a tokio client running on a helper thread, so the Worker under test
//! answers exactly what a shim would receive.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;

use crate::backend::{Capabilities, PeerTools};
use crate::delegation::DelegationConfig;
use a2a_mcp::client::{self, CallOutcome, ClientConfig};
use a2a_mcp::ipc::{ClientFrame, Secret, SendParams, TaskResult, Wait};

use super::*;

pub(crate) const PARENT: &str = "parent";
pub(crate) const CHILD: &str = "child";
/// Allowlisted peer with no Worker behind it: sends to it fail.
pub(crate) const GHOST: &str = "ghost";
/// Not allowlisted by anyone; exercises refusal (a).
pub(crate) const OUTSIDER: &str = "outsider";
const SHIM: &str = "a2abus-mcp-shim";
const IPC_TIMEOUT: Duration = Duration::from_secs(30);

static ENDPOINTS: AtomicU64 = AtomicU64::new(0);

/// Cheap handles the scenario future owns while the Workers are driven by [`drive_mesh`].
#[derive(Clone)]
pub(crate) struct Handles {
    pub(crate) bus: MemBus,
    pub(crate) parent: MockControl,
    pub(crate) child: MockControl,
}

pub(crate) struct Mesh {
    pub(crate) bus: MemBus,
    pub(crate) parent: TestWorker,
    pub(crate) child: TestWorker,
    pub(crate) parent_mock: MockControl,
    pub(crate) child_mock: MockControl,
    pub(crate) parent_store: SharedStore,
}

/// Per-test knobs for the parent Worker. The child always uses default budgets.
pub(crate) struct Spec {
    pub(crate) parent_peers: Vec<&'static str>,
    pub(crate) parent_budget: Allowance,
}

impl Default for Spec {
    fn default() -> Self {
        Self {
            parent_peers: vec![CHILD],
            parent_budget: Allowance::default(),
        }
    }
}

impl Mesh {
    pub(crate) fn handles(&self) -> Handles {
        Handles {
            bus: self.bus.clone(),
            parent: self.parent_mock.clone(),
            child: self.child_mock.clone(),
        }
    }

    /// Crash injection (0015-062): the parent parks after its child accepts a message and
    /// before `sent` is journaled. Restart it with [`restart_parent`].
    pub(crate) fn park_parent_after_accept(&mut self) {
        self.parent.park_after_accept();
    }
}

pub(crate) fn mesh(spec: Spec) -> Mesh {
    let bus = MemBus(Arc::new(
        BusMem::new(MemConfig {
            seed: 11,
            capabilities: m1_capabilities(),
            streams: vec![StreamConfig {
                name: stream(),
                subjects: vec![SubjectPattern::parse("a2a.v1.*.ev.>").unwrap()],
                retention_messages: None,
            }],
            kv_buckets: vec![
                bus_api::name::Bucket::new(&tenant(), "cards").unwrap(),
                bus_api::name::Bucket::new(&tenant(), "presence").unwrap(),
            ],
            grants: mesh_grants(),
            ack_wait: Duration::from_secs(30),
            overload_retry_after: Duration::from_secs(1),
            blob_tenant_quota: 1 << 20,
        })
        .unwrap(),
    ));
    let (parent_backend, parent_mock) = MockBackend::new(capabilities());
    let (child_backend, child_mock) = MockBackend::new(capabilities());
    let parent_store = SharedStore::default();
    let mut parent = node(
        &bus,
        PARENT,
        parent_backend,
        spec.parent_budget,
        parent_store.clone(),
    );
    let mut child = node(
        &bus,
        CHILD,
        child_backend,
        Allowance::default(),
        SharedStore::default(),
    );
    let parent_peers = spec.parent_peers.iter().map(|p| agent(p)).collect();
    parent
        .attach_delegation(DelegationConfig::new(
            endpoint_name(PARENT),
            SHIM,
            parent_peers,
        ))
        .unwrap();
    child
        .attach_delegation(DelegationConfig::new(
            endpoint_name(CHILD),
            SHIM,
            vec![agent(PARENT)],
        ))
        .unwrap();
    Mesh {
        bus,
        parent_store,
        parent,
        child,
        parent_mock,
        child_mock,
    }
}

/// Kills the parent worker without shutdown and reopens it on the same task store
/// (0015-063): the reopened worker reconciles the open turns and children it finds.
pub(crate) fn restart_parent(mesh: Mesh) -> Mesh {
    let Mesh {
        bus,
        parent,
        parent_store,
        child,
        parent_mock,
        child_mock,
    } = mesh;
    drop(parent);
    // The dead instance's presence lease lapses before the agent id is reclaimed.
    bus.control().advance(Duration::from_secs(31)).unwrap();
    let (backend, _mock) = MockBackend::new(capabilities());
    let mut reopened = node(
        &bus,
        PARENT,
        backend,
        Allowance::default(),
        parent_store.clone(),
    );
    reopened
        .attach_delegation(DelegationConfig::new(
            endpoint_name(PARENT),
            SHIM,
            vec![agent(CHILD)],
        ))
        .unwrap();
    Mesh {
        bus,
        parent: reopened,
        parent_store,
        child,
        parent_mock,
        child_mock,
    }
}

fn capabilities() -> Capabilities {
    Capabilities {
        protocol: Protocol::Acp,
        resume: false,
        cancel: true,
        usage_sources: vec![UsageSource::DriverCounted],
        approval_modes: vec!["default".to_owned()],
        streaming_text: true,
    }
}

/// Card and presence publication for a mesh node, so `a2a_discover` can list it (RFC-0004).
fn publication(name: &str) -> crate::presence::Publication {
    use a2a_types::proto::a2a::{AgentCapabilities, AgentInterface, AgentSkill};
    crate::presence::Publication {
        card: a2a_types::proto::a2a::AgentCard {
            name: name.to_owned(),
            description: format!("{name} agent"),
            version: "1.0.0".to_owned(),
            supported_interfaces: vec![AgentInterface {
                url: format!("bus://local/{name}"),
                protocol_binding: "urn:a2abus:binding:bus:1".to_owned(),
                tenant: "local".to_owned(),
                protocol_version: "1.0".to_owned(),
            }],
            capabilities: Some(AgentCapabilities {
                streaming: Some(true),
                ..Default::default()
            }),
            default_input_modes: vec!["text/plain".to_owned()],
            default_output_modes: vec!["text/plain".to_owned()],
            skills: vec![AgentSkill {
                id: "plan".to_owned(),
                name: "plan".to_owned(),
                description: "plans".to_owned(),
                tags: vec!["plan".to_owned()],
                ..Default::default()
            }],
            ..Default::default()
        },
        signing: crate::presence::CardSigning::Disabled,
        timing: crate::presence::Timing::default(),
    }
}

fn node(
    bus: &MemBus,
    name: &str,
    backend: MockBackend,
    budget: Allowance,
    store: SharedStore,
) -> TestWorker {
    let tasks = task::Config {
        owner: name.to_owned(),
        agent_deadline_ms: 3_600_000,
        default_budget: budget,
        metering: Metering {
            tokens: true,
            cost: true,
        },
    };
    let builder = WorkerBuilder::new(tenant(), agent(name), principal(name), tasks)
        .approval(ApprovalMode::AskAll)
        .working_directory(WORK_DIR, WorkspaceMode::PerSession)
        .publication(publication(name));
    block_on(builder.build(
        bus.clone(),
        backend,
        store,
        Box::new(MeshClock(bus.control())),
    ))
    .unwrap()
}

/// Virtual wall clock shared by both Workers.
struct MeshClock(MemControl);

impl Clock for MeshClock {
    fn now_ms(&self) -> Option<u64> {
        self.0
            .now()
            .ok()
            .and_then(|now| u64::try_from(now.as_millis()).ok())
    }
}

fn endpoint_name(tag: &str) -> PathBuf {
    let unique = ENDPOINTS.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    if cfg!(windows) {
        PathBuf::from(format!(r"\\.\pipe\a2abus-test-{tag}-{pid}-{unique}"))
    } else {
        std::env::temp_dir().join(format!("a2abus-test-{tag}-{pid}-{unique}.sock"))
    }
}

fn mesh_grants() -> Vec<Grant> {
    let rpc = |name: &str| SubjectPattern::parse(&format!("a2a.v1.acme.rpc.{name}.>")).unwrap();
    let ev = |name: &str| SubjectPattern::parse(&format!("a2a.v1.acme.ev.{name}.>")).unwrap();
    let worker = |name: &str, publish: Vec<SubjectPattern>| Grant {
        principal: principal(name),
        tenant: tenant(),
        publish,
        serve: vec![rpc(name)],
        streams: vec![stream()],
    };
    let requester = Grant {
        principal: principal(REQUESTER),
        tenant: tenant(),
        publish: vec![rpc(PARENT)],
        serve: Vec::new(),
        streams: Vec::new(),
    };
    vec![
        worker(PARENT, vec![ev(PARENT), rpc(CHILD)]),
        worker(CHILD, vec![ev(CHILD), rpc(PARENT)]),
        requester,
    ]
}

/// Runs `scenario` while both Workers serve. Each Worker runs as a long-lived future polled
/// alongside the scenario, so a Worker waiting on its peer's reply never blocks the peer.
/// Virtual time advances 25 ms per poll.
pub(crate) fn drive_mesh<T>(mesh: &mut Mesh, scenario: impl Future<Output = T>) -> T {
    run_mesh(mesh, Some(Duration::from_millis(25)), scenario)
}

/// Like [`drive_mesh`], but virtual time moves only when the scenario moves it through the
/// bus control. Use it when an assertion counts events inside a virtual window while IPC
/// calls run on real threads: under [`drive_mesh`] every poll advances the clock, so the
/// real latency of a call turns into virtual time and can close the window early.
pub(crate) fn drive_mesh_scenario_clock<T>(
    mesh: &mut Mesh,
    scenario: impl Future<Output = T>,
) -> T {
    run_mesh(mesh, None, scenario)
}

fn run_mesh<T>(mesh: &mut Mesh, step: Option<Duration>, scenario: impl Future<Output = T>) -> T {
    let Mesh {
        bus, parent, child, ..
    } = mesh;
    let control = bus.control();
    let mut scenario = pin!(scenario);
    let mut parent_loop = pin!(serve_forever(parent));
    let mut child_loop = pin!(serve_forever(child));
    for _ in 0..MAX_POLLS {
        if let Some(step) = step {
            control.advance(step).expect("virtual clock advance");
        }
        if let Poll::Ready(value) = poll_once(scenario.as_mut()) {
            return value;
        }
        let _ = poll_once(parent_loop.as_mut());
        let _ = poll_once(child_loop.as_mut());
    }
    panic!("mesh scenario did not complete within {MAX_POLLS} polls");
}

/// Serves one Worker for the life of the scenario. Delegation requests are drained by
/// `pump`, so each turn pumps first, then makes one non-blocking serve attempt (a pending
/// serve is dropped and retried next turn, as the single-Worker `drive` does).
async fn serve_forever(worker: &mut TestWorker) {
    loop {
        worker.pump().await.expect("pump failed");
        let attempt = std::future::poll_fn(|cx| {
            let mut serve = pin!(worker.serve_one());
            Poll::Ready(serve.as_mut().poll(cx))
        })
        .await;
        if let Poll::Ready(result) = attempt {
            result.expect("serve_one failed");
        }
        yield_now().await;
    }
}

/// Waits for the Worker to start a turn and returns the peer tools it was given.
pub(crate) async fn peer_tools(mock: &MockControl) -> PeerTools {
    loop {
        let found = mock
            .commands()
            .into_iter()
            .find_map(|command| match command {
                Command::Start(start) => start.peer_tools,
                _ => None,
            });
        if let Some(tools) = found {
            return tools;
        }
        yield_now().await;
    }
}

/// Whether the mock has recorded a `Cancel` for a turn.
pub(crate) fn cancel_seen(mock: &MockControl) -> bool {
    mock.commands()
        .iter()
        .any(|command| matches!(command, Command::Cancel(_)))
}

/// An in-flight IPC call made by a helper thread with its own tokio runtime.
pub(crate) struct PendingCall {
    rx: mpsc::Receiver<CallOutcome>,
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
}

impl PendingCall {
    /// Cancels the call on the client side; the client sends `CancelCall` (0015-046).
    pub(crate) fn cancel(&mut self) {
        if let Some(tx) = self.cancel.take() {
            let _ = tx.send(());
        }
    }
}

/// Starts `frame` against the endpoint and grant described by `peer`.
pub(crate) fn ipc_call(peer: &PeerTools, frame: ClientFrame) -> PendingCall {
    ipc_call_with(
        ClientConfig {
            endpoint: peer.endpoint.to_string_lossy().into_owned(),
            grant_id: peer.grant_id.clone(),
            credential: Some(Secret::new(peer.grant_credential.expose().to_owned())),
            session_slot: peer.slot.clone(),
        },
        frame,
    )
}

/// Starts `frame` with an explicit client configuration.
pub(crate) fn ipc_call_with(config: ClientConfig, frame: ClientFrame) -> PendingCall {
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let outcome = runtime.block_on(async {
            let client = client::spawn(config);
            client
                .call(frame, tokio::time::Instant::now() + IPC_TIMEOUT, cancel_rx)
                .await
        });
        let _ = tx.send(outcome);
    });
    PendingCall {
        rx,
        cancel: Some(cancel_tx),
    }
}

/// Waits for an IPC call to finish while the Workers keep running.
pub(crate) async fn finished(call: &mut PendingCall) -> CallOutcome {
    loop {
        match call.rx.try_recv() {
            Ok(outcome) => return outcome,
            Err(mpsc::TryRecvError::Empty) => yield_now().await,
            Err(mpsc::TryRecvError::Disconnected) => panic!("ipc call thread ended silently"),
        }
    }
}

/// `a2a_send` frame: blocking unless `wait` says otherwise.
pub(crate) fn send_frame(target: &str, text: &str, key: &str, wait: Wait) -> ClientFrame {
    send_frame_within(target, text, key, wait, None)
}

/// `a2a_send` frame with an explicit `timeoutMs` (the call deadline is then bounded by it).
pub(crate) fn send_frame_within(
    target: &str,
    text: &str,
    key: &str,
    wait: Wait,
    timeout_ms: Option<u64>,
) -> ClientFrame {
    ClientFrame::Send {
        id: 0,
        deadline_ms: 60_000,
        params: SendParams {
            agent: target.to_owned(),
            text: text.to_owned(),
            context_id: None,
            wait,
            timeout_ms,
            idempotency_key: key.to_owned(),
        },
    }
}

/// Unwraps a successful reply into its typed result; panics on a refusal.
pub(crate) fn expect_result(outcome: CallOutcome) -> TaskResult {
    match outcome {
        CallOutcome::Done(Ok(response)) => response
            .into_result::<TaskResult>()
            .unwrap_or_else(|err| panic!("call refused: {}", err.reason)),
        other => panic!("call did not complete: {}", describe(&other)),
    }
}

/// The refusal reason of a failed call: a transport error or an IPC error response.
pub(crate) fn refusal(outcome: CallOutcome) -> String {
    match outcome {
        CallOutcome::Done(Ok(response)) => match response.into_result::<TaskResult>() {
            Err(err) => err.reason,
            Ok(result) => panic!("expected refusal, got {result:?}"),
        },
        CallOutcome::Done(Err(err)) => err.reason,
        other => panic!("expected refusal, got {}", describe(&other)),
    }
}

fn describe(outcome: &CallOutcome) -> String {
    match outcome {
        CallOutcome::Cancelled => "cancelled".to_owned(),
        CallOutcome::Done(Ok(response)) => format!("response {response:?}"),
        CallOutcome::Done(Err(err)) => format!("transport error {}", err.reason),
    }
}

/// RPC as `sender` to `target` (the generalised form of the single-Worker `call`).
pub(crate) async fn rpc(
    bus: &MemBus,
    sender: &str,
    target: &str,
    op: Op,
    body: RequestBody,
) -> Reply {
    let parts = RequestParts {
        tenant: tenant(),
        agent: agent(target),
        msg_id: MessageId::generate(),
        sender: agent(sender),
        origin: None,
        ts_ms: now_ms(bus),
        ttl_ms: TTL_MS,
        traceparent: None,
        tracestate: None,
        extensions: Vec::new(),
        service_params: Vec::new(),
        json: false,
        op,
        body,
    };
    let out = envelope::encode_request(parts, envelope::MAX_INLINE_BYTES).unwrap();
    bus.request(
        &principal(sender),
        &out.subject,
        &out.headers,
        out.payload,
        Duration::from_secs(60),
    )
    .await
    .unwrap()
}

/// Starts a task on `target` as the requester.
pub(crate) async fn submit_to(bus: &MemBus, target: &str, text: &str) -> pb::Task {
    let body = RequestBody::SendMessage(pb::SendMessageRequest {
        tenant: TENANT.to_owned(),
        message: Some(pb::Message {
            message_id: MessageId::generate().as_str().to_owned(),
            role: pb::Role::User as i32,
            parts: vec![pb::Part {
                content: Some(pb::part::Content::Text(text.to_owned())),
                ..Default::default()
            }],
            ..Default::default()
        }),
        configuration: Some(pb::SendMessageConfiguration {
            return_immediately: true,
            ..Default::default()
        }),
        ..Default::default()
    });
    let reply = rpc(bus, REQUESTER, target, Op::Send, body).await;
    match envelope::decode_reply::<pb::SendMessageResponse>(&reply)
        .unwrap()
        .payload
    {
        Some(pb::send_message_response::Payload::Task(task)) => task,
        other => panic!("expected Task reply, got {other:?}"),
    }
}

/// Reads a task on `target` as the requester (tasks are scoped to their submitter).
pub(crate) async fn get_from(bus: &MemBus, target: &str, id: &str) -> pb::Task {
    let body = RequestBody::GetTask(pb::GetTaskRequest {
        tenant: TENANT.to_owned(),
        id: id.to_owned(),
        history_length: None,
    });
    let reply = rpc(bus, REQUESTER, target, Op::Get, body).await;
    envelope::decode_reply::<pb::Task>(&reply)
        .unwrap_or_else(|err| panic!("get {target} {id}: {err:?}; reply {reply:?}"))
}

/// Waits until `id` on `target` reaches `state` (wire name), then returns the task.
pub(crate) async fn until_state(
    bus: &MemBus,
    target: &str,
    id: &str,
    state: pb::TaskState,
) -> pb::Task {
    loop {
        let task = get_from(bus, target, id).await;
        if task
            .status
            .as_ref()
            .is_some_and(|s| s.state == state as i32)
        {
            return task;
        }
        yield_now().await;
    }
}

/// Waits until `id` on `target` reaches any terminal state, then returns the task.
pub(crate) async fn until_terminal(bus: &MemBus, target: &str, id: &str) -> pb::Task {
    let terminal = [
        pb::TaskState::Completed as i32,
        pb::TaskState::Failed as i32,
        pb::TaskState::Canceled as i32,
    ];
    loop {
        let task = get_from(bus, target, id).await;
        if task
            .status
            .as_ref()
            .is_some_and(|s| terminal.contains(&s.state))
        {
            return task;
        }
        yield_now().await;
    }
}

/// Cancels a task on `target` as the requester.
pub(crate) async fn cancel_on(bus: &MemBus, target: &str, id: &str) -> pb::Task {
    let body = RequestBody::CancelTask(pb::CancelTaskRequest {
        tenant: TENANT.to_owned(),
        id: id.to_owned(),
        metadata: None,
    });
    let reply = rpc(bus, REQUESTER, target, Op::Cancel, body).await;
    envelope::decode_reply::<pb::Task>(&reply)
        .unwrap_or_else(|err| panic!("cancel {target} {id}: {err:?}; reply {reply:?}"))
}

/// Turn-level scripts for the Workers.
pub(crate) fn text(value: &str) -> Step {
    Step::Artifact {
        key: "answer".to_owned(),
        text: value.to_owned(),
        append: false,
        last: true,
    }
}
