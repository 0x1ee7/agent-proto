//! Delegation case S1 (RFC-0014 [0014-010], RFC-0015 [0015-054]). A requester submits to a
//! parent Worker, which delegates to a child Worker through the real IPC endpoint and the
//! delegation peer tools. In-process substitute: both Workers are real `a2a-worker` instances on
//! bus-mem driven by MockBackend; no omp, Claude Code or NATS process runs.

use std::path::PathBuf;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::task::Poll;
use std::time::Duration;

use a2a_client::{CallOptions, Client, Config as ClientConfig, Outbound, SendReply, text};
use a2a_core::budget::{Allowance, Metering};
use a2a_core::envelope::Clock;
use a2a_core::task;
use a2a_mcp::client::{self as mcp_client, CallOutcome, ClientConfig as McpConfig};
use a2a_mcp::ipc::{ClientFrame, Secret, SendParams, TaskResult, Wait, reason};
use a2a_types::ids::TaskId;
use a2a_types::proto::a2a as pb;
use a2a_worker::DelegationConfig;
use a2a_worker::backend::{Capabilities, Outcome, PeerTools, Protocol, UsageSource};
use a2a_worker::config::{ApprovalMode, WorkerBuilder};
use a2a_worker::mock::{Command, MockBackend, MockControl, Step};
use a2a_workspace::WorkspaceMode;
use bus_api::control::BusControl;
use bus_api::name::SubjectPattern;
use bus_mem::{Grant, MemBus, MemConfig, StreamConfig, m1_capabilities};

use super::rig::{
    Bus, MAX_POLLS, ScenarioWorker, SharedStore, VirtualClock, WORK_DIR, agent, count_commands,
    poll_once, principal, stream, subject_pattern, task_state, tenant, try_block_on, yield_now,
};

const PARENT: &str = "parent";
const CHILD: &str = "child";
const REQUESTER: &str = "planner";
/// Shim binary name written into the delegation config (RFC-0015 [0015-006]).
const SHIM: &str = "a2abus-mcp-shim";
const IPC_TIMEOUT: Duration = Duration::from_secs(30);
const WAIT_STEPS: usize = 10_000;

/// The two-Worker mesh on one bus: parent and child may each delegate to the other. The
/// requester sends to the parent.
struct Mesh {
    bus: Bus,
    parent: ScenarioWorker,
    parent_mock: MockControl,
    child: ScenarioWorker,
    child_mock: MockControl,
    requester: Client<Bus>,
}

static MESH_SEQ: AtomicU64 = AtomicU64::new(0);

impl Mesh {
    fn new(tag: &str) -> Result<Self, String> {
        // Each mesh binds its own IPC endpoints, so runs sharing this process never clash.
        let tag = format!("{tag}-{}", MESH_SEQ.fetch_add(1, Ordering::Relaxed));
        let bus: Bus = Arc::new(
            MemBus::new(MemConfig {
                seed: 11,
                capabilities: m1_capabilities(),
                streams: vec![StreamConfig {
                    name: stream()?,
                    subjects: vec![subject_pattern("a2a.v1.*.ev.>")?],
                    retention_messages: None,
                }],
                kv_buckets: Vec::new(),
                grants: mesh_grants()?,
                ack_wait: Duration::from_secs(30),
                overload_retry_after: Duration::from_secs(1),
                blob_tenant_quota: 1 << 20,
            })
            .map_err(|e| format!("bus config: {e:?}"))?,
        );
        let (parent, parent_mock) = node(&bus, &tag, PARENT, CHILD)?;
        let (child, child_mock) = node(&bus, &tag, CHILD, PARENT)?;
        let requester = Client::new(
            Arc::clone(&bus),
            Arc::new(VirtualClock(bus.control())) as Arc<dyn Clock>,
            ClientConfig::default(),
            tenant()?,
            agent(REQUESTER)?,
        )
        .map_err(|e| format!("client: {e:?}"))?;
        Ok(Self {
            bus,
            parent,
            parent_mock,
            child,
            child_mock,
            requester,
        })
    }
}

/// S1: the parent delegates to the child; both complete, share the context, and provenance
/// names the child (0015-054 accepted path, 0015-060).
pub(crate) fn s1_delegation_round_trip() -> Result<(), String> {
    let mut m = Mesh::new("s1")?;
    m.child_mock.enqueue(vec![
        answer("child-answer"),
        Step::Terminal(Outcome::Completed),
    ]);
    m.parent_mock
        .enqueue(vec![Step::Hold, Step::Terminal(Outcome::Completed)]);
    let scenario = round_trip(&m.requester, &m.parent_mock, &m.child_mock);
    drive_mesh(&m.bus, &mut m.parent, &mut m.child, scenario)
}

/// S5 (RFC-0014 [0014-016], RFC-0015 [0015-057]): A delegates to B and B calls back to A. The
/// return is refused LOOP_DETECTED at once, not queued behind A's held turn, and B fails.
pub(crate) fn s5_loop_detected() -> Result<(), String> {
    let mut m = Mesh::new("s5")?;
    m.parent_mock
        .enqueue(vec![Step::Hold, Step::Terminal(Outcome::Completed)]);
    m.child_mock.enqueue(vec![Step::Hold]);
    let scenario = loop_round(&m.requester, &m.parent_mock, &m.child_mock);
    drive_mesh(&m.bus, &mut m.parent, &mut m.child, scenario)
}

async fn loop_round(
    requester: &Client<Bus>,
    parent_mock: &MockControl,
    child_mock: &MockControl,
) -> Result<(), String> {
    let parent_task = send_to(requester, PARENT, "start-loop").await?;
    let parent_tools = peer_tools(parent_mock).await?;
    let to_child = ipc_start(&parent_tools, CHILD, "b", "k1");
    let child_tools = peer_tools(child_mock).await?;
    let back = ipc_finish(ipc_start(&child_tools, PARENT, "a", "k2")).await;
    let refused = format!("call refused: {}", reason::LOOP_DETECTED);
    check(matches!(&back, Err(e) if *e == refused), || {
        format!("loop return was {:?}, want {refused}", back.as_ref().err())
    })?;
    let delegated = ipc_finish(to_child).await;
    check(matches!(&delegated, Err(e) if *e == refused), || {
        format!(
            "delegated return was {:?}, want {refused}",
            delegated.as_ref().err()
        )
    })?;
    check(
        count_commands(child_mock, |c| matches!(c, Command::Start(_))) == 1,
        || "child turn did not start exactly once".to_owned(),
    )?;
    let id = TaskId::parse(&parent_task.id).map_err(|e| format!("task id: {e:?}"))?;
    // 0015-070: the failed child fails the delegating parent.
    let done = until_state(requester, PARENT, &id, pb::TaskState::Failed).await?;
    check(task_state(&done) == pb::TaskState::Failed as i32, || {
        "parent task did not fail with its child".to_owned()
    })
}

async fn round_trip(
    requester: &Client<Bus>,
    parent_mock: &MockControl,
    child_mock: &MockControl,
) -> Result<(), String> {
    let parent_task = send_to(requester, PARENT, "delegate").await?;
    let tools = peer_tools(parent_mock).await?;
    let result = ipc_finish(ipc_start(&tools, CHILD, "compute", "k1")).await?;
    check(result.state == "COMPLETED", || {
        format!("delegated state {}, want COMPLETED", result.state)
    })?;
    check(result.text.contains("child-answer"), || {
        "framed text lacks the child answer".to_owned()
    })?;
    check(result.text.contains(CHILD), || {
        "framed text does not name the source agent (0015-035)".to_owned()
    })?;
    check(result.context_id == parent_task.context_id, || {
        "delegated task does not share the parent context".to_owned()
    })?;
    check(result.provenance.source_agent == CHILD, || {
        format!(
            "source_agent {}, want {CHILD}",
            result.provenance.source_agent
        )
    })?;
    check(
        result.provenance.delegating_task_id.as_deref() == Some(parent_task.id.as_str()),
        || "delegating_task_id does not name the parent task".to_owned(),
    )?;
    check(result.provenance.chain_length == 1, || {
        format!("chain_length {}, want 1", result.provenance.chain_length)
    })?;
    check(
        count_commands(child_mock, |c| matches!(c, Command::Start(_))) == 1,
        || "child turn did not start exactly once".to_owned(),
    )?;
    parent_mock.release();
    let id = TaskId::parse(&parent_task.id).map_err(|e| format!("task id: {e:?}"))?;
    let done = until_state(requester, PARENT, &id, pb::TaskState::Completed).await?;
    check(task_state(&done) == pb::TaskState::Completed as i32, || {
        "parent task did not complete".to_owned()
    })
}

fn check(ok: bool, what: impl FnOnce() -> String) -> Result<(), String> {
    if ok { Ok(()) } else { Err(what()) }
}

fn mesh_grants() -> Result<Vec<Grant>, String> {
    let rpc = |name: &str| subject_pattern(&format!("a2a.v1.acme.rpc.{name}.>"));
    let ev = |name: &str| subject_pattern(&format!("a2a.v1.acme.ev.{name}.>"));
    let worker = |name: &str, publish: Vec<SubjectPattern>| -> Result<Grant, String> {
        Ok(Grant {
            principal: principal(name)?,
            tenant: tenant()?,
            publish,
            serve: vec![rpc(name)?],
            streams: vec![stream()?],
        })
    };
    Ok(vec![
        worker(PARENT, vec![ev(PARENT)?, rpc(CHILD)?])?,
        worker(CHILD, vec![ev(CHILD)?, rpc(PARENT)?])?,
        Grant {
            principal: principal(REQUESTER)?,
            tenant: tenant()?,
            publish: vec![rpc(PARENT)?],
            serve: Vec::new(),
            streams: Vec::new(),
        },
    ])
}

/// Builds one Worker on the mesh bus that may delegate to `peer`, with its own mock backend.
fn node(
    bus: &Bus,
    tag: &str,
    name: &str,
    peer: &str,
) -> Result<(ScenarioWorker, MockControl), String> {
    let (backend, mock) = MockBackend::new(Capabilities {
        protocol: Protocol::Acp,
        resume: false,
        cancel: true,
        usage_sources: vec![UsageSource::DriverCounted],
        approval_modes: vec!["default".to_owned()],
        streaming_text: true,
    });
    let tasks = task::Config {
        owner: name.to_owned(),
        agent_deadline_ms: 3_600_000,
        default_budget: Allowance::default(),
        metering: Metering {
            tokens: true,
            cost: true,
        },
    };
    let builder = WorkerBuilder::new(tenant()?, agent(name)?, principal(name)?, tasks)
        .approval(ApprovalMode::AskAll)
        .working_directory(WORK_DIR, WorkspaceMode::PerSession);
    let mut worker = try_block_on(builder.build(
        Arc::clone(bus),
        backend,
        SharedStore::default(),
        Box::new(VirtualClock(bus.control())),
    ))?
    .map_err(|e| format!("worker build: {e:?}"))?;
    worker
        .attach_delegation(DelegationConfig::new(
            PathBuf::from(a2a_mcp::testkit::endpoint_name(&format!("{tag}-{name}"))),
            SHIM,
            vec![agent(peer)?],
        ))
        .map_err(|e| format!("attach_delegation: {e:?}"))?;
    Ok((worker, mock))
}

/// Runs `scenario` while both Workers serve. Each Worker runs as a long-lived loop that is
/// polled alongside the scenario, so a Worker waiting on its peer never blocks the other (the
/// same arrangement as the worker's own mesh harness).
fn drive_mesh(
    bus: &Bus,
    parent: &mut ScenarioWorker,
    child: &mut ScenarioWorker,
    scenario: impl std::future::Future<Output = Result<(), String>>,
) -> Result<(), String> {
    let mut scenario = pin!(scenario);
    let mut parent_loop = pin!(serve_forever(parent));
    let mut child_loop = pin!(serve_forever(child));
    for _ in 0..MAX_POLLS {
        bus.control()
            .advance(Duration::from_millis(25))
            .map_err(|e| format!("advance: {e:?}"))?;
        if let Poll::Ready(value) = poll_once(scenario.as_mut()) {
            return value;
        }
        if let Poll::Ready(result) = poll_once(parent_loop.as_mut()) {
            return result;
        }
        if let Poll::Ready(result) = poll_once(child_loop.as_mut()) {
            return result;
        }
    }
    Err(format!(
        "mesh scenario did not complete within {MAX_POLLS} polls"
    ))
}

/// Serves one Worker for the life of the scenario: pump its state machine, then make one
/// non-blocking serve attempt (a pending serve is dropped and retried next turn).
async fn serve_forever(worker: &mut ScenarioWorker) -> Result<(), String> {
    loop {
        worker.pump().await.map_err(|e| format!("pump: {e:?}"))?;
        let attempt = std::future::poll_fn(|cx| {
            let mut serve = pin!(worker.serve_one());
            Poll::Ready(serve.as_mut().poll(cx))
        })
        .await;
        if let Poll::Ready(result) = attempt {
            result.map_err(|e| format!("serve_one: {e:?}"))?;
        }
        yield_now().await;
    }
}

fn answer(value: &str) -> Step {
    Step::Artifact {
        key: "answer".to_owned(),
        text: value.to_owned(),
        append: false,
        last: true,
    }
}

async fn send_to(client: &Client<Bus>, target: &str, content: &str) -> Result<pb::Task, String> {
    let mut outbound = Outbound::new(vec![text(content)]);
    outbound.return_immediately = true;
    let reply = client
        .send(&agent(target)?, &outbound, &CallOptions::default())
        .await
        .map_err(|e| format!("send: {e:?}"))?;
    match reply.value {
        SendReply::Task(task) => Ok(task),
        other => Err(format!("expected a Task reply, got {other:?}")),
    }
}

/// Waits until the parent Worker starts a turn and returns the peer tools it was given.
async fn peer_tools(mock: &MockControl) -> Result<PeerTools, String> {
    for _ in 0..WAIT_STEPS {
        let found = mock
            .commands()
            .into_iter()
            .find_map(|command| match command {
                Command::Start(start) => start.peer_tools,
                _ => None,
            });
        if let Some(tools) = found {
            return Ok(tools);
        }
        yield_now().await;
    }
    Err("parent turn never started".to_owned())
}

/// An `a2a_send` call in flight over the delegation endpoint. It runs on a helper thread with
/// its own runtime; [`ipc_finish`] collects the outcome.
struct IpcCall(mpsc::Receiver<Result<CallOutcome, String>>);

fn ipc_start(tools: &PeerTools, target: &str, content: &str, key: &str) -> IpcCall {
    let config = McpConfig {
        endpoint: tools.endpoint.to_string_lossy().into_owned(),
        grant_id: tools.grant_id.clone(),
        credential: Some(Secret::new(tools.grant_credential.expose().to_owned())),
        session_slot: tools.slot.clone(),
    };
    let frame = ClientFrame::Send {
        id: 0,
        deadline_ms: 60_000,
        params: SendParams {
            agent: target.to_owned(),
            text: content.to_owned(),
            context_id: None,
            wait: Wait::Blocking,
            timeout_ms: None,
            idempotency_key: key.to_owned(),
        },
    };
    let (tx, rx) = mpsc::channel::<Result<CallOutcome, String>>();
    std::thread::spawn(move || {
        let outcome = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => Ok(runtime.block_on(async {
                // Held until the call returns: dropping the sender would read as a cancel.
                let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
                let client = mcp_client::spawn(config);
                client
                    .call(frame, tokio::time::Instant::now() + IPC_TIMEOUT, cancel_rx)
                    .await
            })),
            Err(e) => Err(format!("tokio runtime: {e}")),
        };
        let _ = tx.send(outcome);
    });
    IpcCall(rx)
}

/// Waits for an [`ipc_start`] call. A refusal is `Err("call refused: <reason>")`.
async fn ipc_finish(call: IpcCall) -> Result<TaskResult, String> {
    let outcome = loop {
        match call.0.try_recv() {
            Ok(outcome) => break outcome?,
            Err(mpsc::TryRecvError::Empty) => yield_now().await,
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err("ipc call thread ended silently".to_owned());
            }
        }
    };
    match outcome {
        CallOutcome::Done(Ok(response)) => response
            .into_result::<TaskResult>()
            .map_err(|err| format!("call refused: {}", err.reason)),
        CallOutcome::Done(Err(err)) => Err(format!("transport error: {}", err.reason)),
        CallOutcome::Cancelled => Err("call cancelled".to_owned()),
    }
}

/// Polls `get_task` on `target` until the task reaches `want`. Bounded.
async fn until_state(
    client: &Client<Bus>,
    target: &str,
    id: &TaskId,
    want: pb::TaskState,
) -> Result<pb::Task, String> {
    let mut last = pb::TaskState::Unspecified as i32;
    for _ in 0..WAIT_STEPS {
        let task = client
            .get_task(&agent(target)?, id, &CallOptions::default())
            .await
            .map_err(|e| format!("get_task: {e:?}"))?
            .value;
        last = task_state(&task);
        if last == want as i32 {
            return Ok(task);
        }
        yield_now().await;
    }
    Err(format!(
        "task {id} never reached {want:?}; last state {last}"
    ))
}
