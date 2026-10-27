//! In-process rig for the scenario cases: a real [`Worker`] over `bus-mem` with the testkit
//! [`MockBackend`], a virtual clock, and requester [`Client`]s. Time moves only through
//! [`VirtualClock`]; nothing sleeps. Failures are returned as `Err(String)`, never panics.

use std::pin::{Pin, pin};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use bus_api::control::BusControl;

use a2a_client::{Client, Config as ClientConfig};
use a2a_core::budget::{Allowance, Metering};
use a2a_core::envelope::{self, Clock};
use a2a_core::task::{self, StoreError, TaskStore};
use a2a_types::ids::{AgentId, TaskId, TenantId};
use a2a_types::proto::a2a as pb;
use a2a_worker::backend::{Capabilities, Protocol, UsageSource};
use a2a_worker::config::{ApprovalMode, DEFAULT_MAX_WAITING_PER_CONTEXT, WorkerBuilder};
use a2a_worker::mock::{MockBackend, MockControl};
use a2a_worker::worker::Worker;
use a2a_workspace::WorkspaceMode;
use bus_api::name::{Principal, StreamName, SubjectPattern};
use bus_mem::{Grant, MemBus, MemConfig, MemControl, StreamConfig, m1_capabilities};

pub(crate) const TENANT: &str = "acme";
pub(crate) const WORKER: &str = "worker";
pub(crate) const REQUESTER: &str = "planner";
pub(crate) const OBSERVER: &str = "observer";
/// Working directory the worker reports approvals against (RFC-0006 [0006-006]).
pub(crate) const WORK_DIR: &str = "agent-work";
/// Event stream of the tenant; the client attaches to `A2AB_EV_<tenant>` (RFC-0010 4.5).
const EVENT_STREAM: &str = "A2AB_EV_acme";
pub(crate) const MAX_POLLS: usize = 500_000;
/// Default approval expiry (RFC-0006 default, 300 s).
pub(crate) const APPROVAL_EXPIRY_MS: u64 = 300_000;

pub(crate) type Bus = Arc<MemBus>;
pub(crate) type ScenarioWorker = Worker<Bus, MockBackend, SharedStore>;

/// [`TaskStore`] shared between the rig and the Worker.
#[derive(Clone, Default)]
pub(crate) struct SharedStore(Arc<Mutex<Vec<Vec<u8>>>>);

impl TaskStore for SharedStore {
    fn append(&mut self, record: &[u8]) -> Result<(), StoreError> {
        lock(&self.0).push(record.to_vec());
        Ok(())
    }

    fn records(&self) -> Result<Vec<Vec<u8>>, StoreError> {
        Ok(lock(&self.0).clone())
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Virtual wall clock: bus-mem virtual time in milliseconds.
pub(crate) struct VirtualClock(pub(crate) MemControl);

impl Clock for VirtualClock {
    fn now_ms(&self) -> Option<u64> {
        self.0
            .now()
            .ok()
            .and_then(|now| u64::try_from(now.as_millis()).ok())
    }
}

pub(crate) struct Rig {
    pub(crate) bus: Bus,
    pub(crate) mock: MockControl,
    pub(crate) worker: ScenarioWorker,
    pub(crate) clock: Arc<VirtualClock>,
    pub(crate) store: SharedStore,
}

/// Rig construction overrides. `wire` replaces the worker's inbound policy (RFC-0002 [0002-081]);
/// `max_waiting` sets the per-context queue quota (RFC-0005 [0005-028]).
#[derive(Default)]
pub(crate) struct RigSpec {
    pub(crate) wire: Option<envelope::Config>,
    pub(crate) max_waiting: Option<usize>,
}

/// Builds the default rig: bus-mem with the M1 grants, a worker with `AskAll` approvals and the
/// default waiting quota, and a [`MockBackend`] controlled through the returned `Rig::mock`.
pub(crate) fn rig() -> Result<Rig, String> {
    rig_with(RigSpec::default())
}

/// Builds the rig with `spec` applied to the worker (see [`RigSpec`]).
pub(crate) fn rig_with(spec: RigSpec) -> Result<Rig, String> {
    let bus = Arc::new(
        MemBus::new(MemConfig {
            seed: 7,
            capabilities: m1_capabilities(),
            streams: vec![StreamConfig {
                name: stream()?,
                subjects: vec![subject_pattern("a2a.v1.*.ev.>")?],
                retention_messages: None,
            }],
            kv_buckets: Vec::new(),
            grants: grants()?,
            ack_wait: Duration::from_secs(30),
            overload_retry_after: Duration::from_secs(1),
            blob_tenant_quota: 1 << 20,
        })
        .map_err(|e| format!("bus config: {e:?}"))?,
    );
    let store = SharedStore::default();
    let (backend, mock) = MockBackend::new(capabilities());
    let clock = Arc::new(VirtualClock(bus.control()));
    let worker = open_worker(&bus, backend, store.clone(), spec)?;
    Ok(Rig {
        bus,
        mock,
        worker,
        clock,
        store,
    })
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

/// Builds a worker on `bus` over `store`. Reopening on the same store reconciles the
/// task records the previous incarnation left behind.
fn open_worker(
    bus: &Bus,
    backend: MockBackend,
    store: SharedStore,
    spec: RigSpec,
) -> Result<ScenarioWorker, String> {
    let tasks = task::Config {
        owner: WORKER.to_owned(),
        agent_deadline_ms: 3_600_000,
        default_budget: Allowance::default(),
        metering: Metering {
            tokens: true,
            cost: true,
        },
    };
    let mut builder = WorkerBuilder::new(tenant()?, agent(WORKER)?, principal(WORKER)?, tasks)
        .approval(ApprovalMode::AskAll)
        .max_waiting_per_context(spec.max_waiting.unwrap_or(DEFAULT_MAX_WAITING_PER_CONTEXT))
        .working_directory(WORK_DIR, WorkspaceMode::PerSession);
    if let Some(wire) = spec.wire {
        builder = builder.wire(wire);
    }
    try_block_on(builder.build(
        Arc::clone(bus),
        backend,
        store,
        Box::new(VirtualClock(bus.control())),
    ))?
    .map_err(|e| format!("worker build: {e:?}"))
}

/// Simulates a worker crash and restart on the same bus and task store: the old worker is
/// dropped, the virtual clock passes the presence lease, and a fresh worker with a fresh
/// backend mock reopens the store (RFC-0015 [0015-063] reconciliation applies).
pub(crate) fn reopen(rig: Rig) -> Result<Rig, String> {
    let Rig {
        bus,
        worker,
        clock,
        store,
        ..
    } = rig;
    drop(worker);
    advance(&clock, 31_000)?;
    let (backend, mock) = MockBackend::new(capabilities());
    let worker = open_worker(&bus, backend, store.clone(), RigSpec::default())?;
    Ok(Rig {
        bus,
        mock,
        worker,
        clock,
        store,
    })
}

/// A requester client that sends as `sender`, on the rig's virtual clock.
pub(crate) fn client(rig: &Rig, sender: &str) -> Result<Client<Bus>, String> {
    Client::new(
        Arc::clone(&rig.bus),
        Arc::clone(&rig.clock) as Arc<dyn Clock>,
        ClientConfig::default(),
        tenant()?,
        agent(sender)?,
    )
    .map_err(|e| format!("client: {e:?}"))
}

fn grants() -> Result<Vec<Grant>, String> {
    let rpc = subject_pattern("a2a.v1.acme.rpc.worker.>")?;
    let ev = subject_pattern("a2a.v1.acme.ev.worker.>")?;
    Ok(vec![
        Grant {
            principal: principal(WORKER)?,
            tenant: tenant()?,
            publish: vec![ev],
            serve: vec![rpc.clone()],
            streams: vec![stream()?],
        },
        Grant {
            principal: principal(REQUESTER)?,
            tenant: tenant()?,
            // Granted to an unserved subject so an unknown peer reaches the no-responder path
            // rather than being refused by the ACL (RFC-0002 [0002-093]).
            publish: vec![rpc.clone(), subject_pattern("a2a.v1.acme.rpc.ghost.>")?],
            serve: Vec::new(),
            // The requester reads the event stream for the tasks it sent (approval, status).
            streams: vec![stream()?],
        },
        // The observer may read the event stream but holds no publish grant: its requests are
        // refused at the bus (RFC-0011 [0011-044]).
        Grant {
            principal: principal(OBSERVER)?,
            tenant: tenant()?,
            publish: Vec::new(),
            serve: Vec::new(),
            streams: vec![stream()?],
        },
    ])
}

pub(crate) fn stream() -> Result<StreamName, String> {
    StreamName::parse(EVENT_STREAM).map_err(|e| format!("stream name: {e:?}"))
}

pub(crate) fn subject_pattern(text: &str) -> Result<SubjectPattern, String> {
    SubjectPattern::parse(text).map_err(|e| format!("subject pattern `{text}`: {e:?}"))
}

pub(crate) fn tenant() -> Result<TenantId, String> {
    TenantId::parse(TENANT).map_err(|e| format!("tenant: {e:?}"))
}

pub(crate) fn agent(name: &str) -> Result<AgentId, String> {
    AgentId::parse(name).map_err(|e| format!("agent `{name}`: {e:?}"))
}

pub(crate) fn task_id(task: &pb::Task) -> Result<TaskId, String> {
    TaskId::parse(&task.id).map_err(|e| format!("task id `{}`: {e:?}", task.id))
}

pub(crate) fn principal(name: &str) -> Result<Principal, String> {
    Principal::named(name).map_err(|e| format!("principal `{name}`: {e:?}"))
}

pub(crate) fn poll_once<F: Future + ?Sized>(fut: Pin<&mut F>) -> Poll<F::Output> {
    let mut cx = Context::from_waker(Waker::noop());
    fut.poll(&mut cx)
}

/// Polls a future that completes without concurrent work (rig setup).
pub(crate) fn try_block_on<T>(fut: impl Future<Output = T>) -> Result<T, String> {
    let mut fut = pin!(fut);
    for _ in 0..MAX_POLLS {
        if let Poll::Ready(value) = poll_once(fut.as_mut()) {
            return Ok(value);
        }
    }
    Err(format!(
        "setup future did not complete within {MAX_POLLS} polls"
    ))
}

/// Runs `scenario` while the Worker serves requests and pumps its state machine. Each
/// iteration polls the scenario, then one `serve_one`, then `pump`.
pub(crate) fn drive<T>(
    worker: &mut ScenarioWorker,
    scenario: impl Future<Output = Result<T, String>>,
) -> Result<T, String> {
    let mut scenario = pin!(scenario);
    for _ in 0..MAX_POLLS {
        if let Poll::Ready(value) = poll_once(scenario.as_mut()) {
            return value;
        }
        {
            let mut serve = pin!(worker.serve_one());
            if let Poll::Ready(result) = poll_once(serve.as_mut()) {
                result.map_err(|e| format!("serve_one: {e:?}"))?;
            }
        }
        let mut step = pin!(worker.pump());
        match poll_once(step.as_mut()) {
            Poll::Ready(result) => result.map_err(|e| format!("pump: {e:?}"))?,
            Poll::Pending => return Err("pump did not complete in one poll".to_owned()),
        }
    }
    Err(format!(
        "scenario did not complete within {MAX_POLLS} polls"
    ))
}

/// Yields once so the Worker can make progress between scenario steps.
pub(crate) async fn yield_now() {
    struct Yield(bool);
    impl Future for Yield {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.0 {
                Poll::Ready(())
            } else {
                self.0 = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }
    Yield(false).await;
}

/// Moves virtual time forward; the Worker's deadlines observe the same clock.
pub(crate) fn advance(clock: &VirtualClock, ms: u64) -> Result<(), String> {
    clock
        .0
        .advance(Duration::from_millis(ms))
        .map_err(|e| format!("advance: {e:?}"))
}

/// Polls `get_task` until the task reaches `want`, yielding between polls. Bounded.
pub(crate) async fn wait_for_state(
    client: &Client<Bus>,
    id: &TaskId,
    want: pb::TaskState,
) -> Result<pb::Task, String> {
    for _ in 0..10_000 {
        let task = client
            .get_task(&agent(WORKER)?, id, &a2a_client::CallOptions::default())
            .await
            .map_err(|e| format!("get_task: {e:?}"))?
            .value;
        if task_state(&task) == want as i32 {
            return Ok(task);
        }
        yield_now().await;
    }
    Err(format!("task {id} never reached {want:?}"))
}

pub(crate) fn task_state(task: &pb::Task) -> i32 {
    task.status.as_ref().map_or(0, |status| status.state)
}

pub(crate) fn count_commands(
    mock: &MockControl,
    pred: impl Fn(&a2a_worker::mock::Command) -> bool,
) -> usize {
    mock.commands().iter().filter(|c| pred(c)).count()
}
