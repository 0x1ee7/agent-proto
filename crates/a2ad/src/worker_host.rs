//! Worker host (RFC-0012 section 4.5, RFC-0009, RFC-0010, RFC-0013, RFC-0015).
//!
//! Runs one `kind = "worker"` agent in this process. The worker serves its RPC subjects under the
//! agent's own bus identity (RFC-0010 0010-011), publishes its card and presence (RFC-0004), drives
//! its backend (RFC-0009), records audit events (RFC-0013) and, when the agent has a delegation
//! table, serves its delegation endpoint (RFC-0015).
//!
//! Child processes receive a filtered environment: provider keys are dropped unless the agent names
//! them in `env_allow`, and `A2AB_*` identity variables are supervisor-owned (RFC-0012 0012-062).
//! Backend selection maps the configured driver to `driver-acp` or `driver-a2a-http` (RFC-0009).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use a2a_audit::clock::SystemClock as AuditClock;
use a2a_audit::pipeline::{PublishError, Publisher};
use a2a_audit::redact::Redactor;
use a2a_audit::spool::SPOOL_CAP_BYTES;
use a2a_audit::{EmitterIdentity, Pipeline, PipelineConfig};
use a2a_core::budget::{Allowance, Metering};
use a2a_core::envelope::{Clock, SystemClock};
use a2a_core::task;
use a2a_types::ids::{AgentId, TenantId};
use a2a_types::proto::a2a::{AgentCapabilities, AgentCard, AgentInterface, AgentSkill};
use a2a_types::proto::a2abus::{AuditEvent, EmitterKind};
use a2a_worker::DelegationConfig;
use a2a_worker::audit::AuditRecorder;
use a2a_worker::backend::{
    Backend, BackendError, BackendSink, Capabilities, Health, PermissionAnswer, RequestId,
    StartTurn, TurnRef,
};
use a2a_worker::config::{ApprovalMode, WorkerBuilder};
use a2a_worker::error::WorkerError;
use a2a_worker::presence::{CardSigning, Publication, Timing};
use a2a_worker::store::FileStore;
use a2a_worker::worker::Worker;
use a2a_worker::workspace::WorkspaceBinding;
use a2a_workspace::{WorkspaceConfig, WorkspaceMode};
use bus_api::bus::Bus;
use bus_api::error::BusError;
use bus_api::headers::Headers;
use bus_api::name::{MsgId, Principal, StreamName, Subject};
use bus_nats::{NatsBus, layout};
use bytes::Bytes;
use driver_a2a_http::{HttpBackend, PeerConfig};
use driver_acp::{AcpDriver, AcpDriverConfig, Profile};
use tokio::runtime::Runtime;

use crate::broker;
use crate::config::{self, AgentSpec, Backend as SpecBackend, Config, WorkspacePolicy};
use crate::creds;
use crate::host::{HostControl, HostError, HostHealth};
use crate::hosts;

/// Poll interval of the serve loop when no backend wakeup is due sooner.
const POLL: Duration = Duration::from_millis(100);
/// Interval between audit flushes (RFC-0013 spool drain).
const FLUSH_EVERY: Duration = Duration::from_secs(1);
/// Window in which a restarted worker retries its claim while the previous instance's presence
/// record expires (RFC-0004 0004-025): the 30 s presence TTL (a2a-worker default) plus a margin.
const CLAIM_RETRY_WINDOW: Duration = Duration::from_secs(45);
/// Pause between claim attempts.
const CLAIM_RETRY_EVERY: Duration = Duration::from_secs(1);
/// a2a-worker refusal when another live instance holds the agent id (presence.rs).
const SERVED_ELSEWHERE: &str = "agent id is served by another live instance";
/// Poll interval of the audit bridge thread's inbox.
const AUDIT_POLL: Duration = Duration::from_millis(5);
/// Agent default task deadline in milliseconds (RFC-0005 0005-040).
const AGENT_DEADLINE_MS: u64 = 3_600_000;
/// Workspace garbage collection interval in milliseconds.
const WORKSPACE_GC_MS: u64 = 3_600_000;
/// Approval mode used when the agent table names none.
const DEFAULT_APPROVAL: &str = "ask-all";
/// Binding identifier advertised on the card (RFC-0004). Same value as the presence module's.
const BINDING: &str = "urn:a2abus:binding:bus:1";
/// Delegation shim binary name without the platform suffix (RFC-0015).
const SHIM_NAME: &str = "a2abus-mcp-shim";
/// Prefix of the supervisor-owned identity variables.
const IDENTITY_PREFIX: &str = "A2AB_";

/// The driver instance of one worker (RFC-0009). Chosen once from the agent table.
pub(crate) enum AnyBackend {
    /// ACP driver for `acp-omp` or `acp-claude`.
    Acp(AcpDriver),
    /// A2A HTTP driver for `a2a-http`.
    Http(HttpBackend),
}

impl Backend for AnyBackend {
    fn capabilities(&self) -> Capabilities {
        match self {
            Self::Acp(backend) => backend.capabilities(),
            Self::Http(backend) => backend.capabilities(),
        }
    }

    fn health(&self) -> Health {
        match self {
            Self::Acp(backend) => backend.health(),
            Self::Http(backend) => backend.health(),
        }
    }

    fn can_start(&self) -> bool {
        match self {
            Self::Acp(backend) => backend.can_start(),
            Self::Http(backend) => backend.can_start(),
        }
    }

    fn attach(&mut self, sink: Box<dyn BackendSink>) {
        match self {
            Self::Acp(backend) => backend.attach(sink),
            Self::Http(backend) => backend.attach(sink),
        }
    }

    fn start(&mut self, cmd: StartTurn) -> Result<(), BackendError> {
        match self {
            Self::Acp(backend) => backend.start(cmd),
            Self::Http(backend) => backend.start(cmd),
        }
    }

    fn resolve_permission(
        &mut self,
        request_id: &RequestId,
        answer: PermissionAnswer,
    ) -> Result<(), BackendError> {
        match self {
            Self::Acp(backend) => backend.resolve_permission(request_id, answer),
            Self::Http(backend) => backend.resolve_permission(request_id, answer),
        }
    }

    fn cancel(&mut self, turn: &TurnRef) -> Result<(), BackendError> {
        match self {
            Self::Acp(backend) => backend.cancel(turn),
            Self::Http(backend) => backend.cancel(turn),
        }
    }

    fn shutdown(&mut self, grace: Duration) -> Result<(), BackendError> {
        match self {
            Self::Acp(backend) => backend.shutdown(grace),
            Self::Http(backend) => backend.shutdown(grace),
        }
    }
}

/// Shared handle to the backend. The worker owns one clone; the host keeps another so it can end
/// the driver's process tree after the drain (RFC-0009 0009-059).
#[derive(Clone)]
pub(crate) struct SharedBackend(Arc<Mutex<AnyBackend>>);

impl SharedBackend {
    /// Wraps a driver instance.
    pub(crate) fn new(backend: AnyBackend) -> Self {
        Self(Arc::new(Mutex::new(backend)))
    }

    fn with<R>(&self, f: impl FnOnce(&mut AnyBackend) -> R) -> R {
        // A poisoned lock only means an earlier call panicked; the driver state is still usable
        // for shutdown, so the guard is recovered rather than propagating the poison.
        let mut guard = match self.0.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        f(&mut guard)
    }
}

impl Backend for SharedBackend {
    fn capabilities(&self) -> Capabilities {
        self.with(|backend| backend.capabilities())
    }

    fn health(&self) -> Health {
        self.with(|backend| backend.health())
    }

    fn can_start(&self) -> bool {
        self.with(|backend| backend.can_start())
    }

    fn attach(&mut self, sink: Box<dyn BackendSink>) {
        self.with(|backend| backend.attach(sink));
    }

    fn start(&mut self, cmd: StartTurn) -> Result<(), BackendError> {
        self.with(|backend| backend.start(cmd))
    }

    fn resolve_permission(
        &mut self,
        request_id: &RequestId,
        answer: PermissionAnswer,
    ) -> Result<(), BackendError> {
        self.with(|backend| backend.resolve_permission(request_id, answer))
    }

    fn cancel(&mut self, turn: &TurnRef) -> Result<(), BackendError> {
        self.with(|backend| backend.cancel(turn))
    }

    fn shutdown(&mut self, grace: Duration) -> Result<(), BackendError> {
        self.with(|backend| backend.shutdown(grace))
    }
}

/// Builds the environment of the agent's child process (RFC-0009, RFC-0012 0012-062).
///
/// Host variables are inherited only when `passthrough_all` is set, and then never provider keys.
/// Provider keys are otherwise dropped unless named in `env_allow`. `A2AB_*` variables are never
/// inherited; the supervisor sets `A2AB_AGENT` and `A2AB_TENANT`. Every `env_require` name must
/// be present in the result.
pub(crate) fn child_env(
    spec: &AgentSpec,
    tenant: &str,
    host: &[(String, String)],
) -> Result<Vec<(String, String)>, HostError> {
    let mut out = BTreeMap::new();
    if spec.passthrough_all {
        for (key, value) in host {
            if !key.starts_with(IDENTITY_PREFIX) && !config::is_provider_key(key) {
                out.insert(key.clone(), value.clone());
            }
        }
    }
    for name in &spec.env_allow {
        reject_reserved(spec, name)?;
        if let Some((_, value)) = host.iter().find(|(key, _)| key == name) {
            out.insert(name.clone(), value.clone());
        }
    }
    for (key, value) in &spec.env {
        reject_reserved(spec, key)?;
        if config::is_provider_key(key) {
            return Err(HostError::Config(format!(
                "agent {}: env may not set provider key {key}",
                spec.id
            )));
        }
        out.insert(key.clone(), value.clone());
    }
    out.insert(format!("{IDENTITY_PREFIX}AGENT"), spec.id.clone());
    out.insert(format!("{IDENTITY_PREFIX}TENANT"), tenant.to_owned());
    for name in &spec.env_require {
        if !out.contains_key(name) {
            return Err(HostError::Config(format!(
                "agent {}: required environment variable {name} is not set",
                spec.id
            )));
        }
    }
    Ok(out.into_iter().collect())
}

fn reject_reserved(spec: &AgentSpec, name: &str) -> Result<(), HostError> {
    if name.starts_with(IDENTITY_PREFIX) {
        return Err(HostError::Config(format!(
            "agent {}: {name} is a supervisor identity variable",
            spec.id
        )));
    }
    Ok(())
}

/// Maps the agent's approval mode to the worker's (RFC-0012 0012-051). The mode is the configured
/// one or, when none is set, the backend default (RFC-0006 0006-004). `ask-all` asks for every
/// permission; any other mode asks for execution requests.
pub(crate) fn approval_mode(spec: &AgentSpec) -> ApprovalMode {
    let mode = spec
        .approval_mode
        .as_deref()
        .or_else(|| spec.backend.map(SpecBackend::default_approval))
        .unwrap_or(DEFAULT_APPROVAL);
    match mode {
        DEFAULT_APPROVAL => ApprovalMode::AskAll,
        _ => ApprovalMode::AskExec,
    }
}

/// Builds the presence card of the worker (RFC-0004). The card names the agent, advertises one
/// bus interface and omits signing (the card is served over the bus only).
pub(crate) fn agent_card(tenant: &TenantId, spec: &AgentSpec) -> AgentCard {
    let description = spec.description.clone().unwrap_or_else(|| spec.id.clone());
    AgentCard {
        name: spec.id.clone(),
        description: description.clone(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        supported_interfaces: vec![AgentInterface {
            url: format!("bus://{}/{}", tenant.as_str(), spec.id),
            protocol_binding: BINDING.to_owned(),
            tenant: tenant.as_str().to_owned(),
            protocol_version: "1.0".to_owned(),
        }],
        capabilities: Some(AgentCapabilities {
            streaming: Some(true),
            push_notifications: Some(false),
            extended_agent_card: Some(false),
            ..Default::default()
        }),
        default_input_modes: vec!["text/plain".to_owned()],
        default_output_modes: vec!["text/plain".to_owned()],
        // A card must declare at least one skill (presence.rs). Each worker offers one general
        // text skill named after the agent; its text is untrusted (RFC-0004 0004-040).
        skills: vec![AgentSkill {
            id: spec.id.clone(),
            name: spec.id.clone(),
            description,
            tags: vec!["text".to_owned()],
            input_modes: vec!["text/plain".to_owned()],
            output_modes: vec!["text/plain".to_owned()],
            ..Default::default()
        }],
        ..Default::default()
    }
}

/// Working directory of the backend process. A worker with a workspace runs inside its workspace
/// root, never in the operator's checkout (RFC-0012 0012-030). Without one it runs in its own
/// private directory under the state directory.
pub(crate) fn working_directory(state_dir: &Path, tenant: &TenantId, spec: &AgentSpec) -> PathBuf {
    if spec.workspace.is_some() {
        state_dir.join("ws").join(tenant.as_str()).join(&spec.id)
    } else {
        state_dir.join("agents").join(&spec.id).join("work")
    }
}

/// Workspace binding of a worker with a `workspace` policy (RFC-0012 0012-038, 0012-039).
pub(crate) fn workspace_binding(
    state_dir: &Path,
    tenant: &TenantId,
    spec: &AgentSpec,
    policy: &WorkspacePolicy,
) -> Result<WorkspaceBinding, HostError> {
    let repo = spec
        .repo
        .clone()
        .ok_or_else(|| HostError::Config(format!("agent {}: workspace requires repo", spec.id)))?;
    let mut config = WorkspaceConfig::new(
        state_dir.join("ws"),
        state_dir.join("artifacts"),
        tenant.as_str(),
        policy.mode,
    );
    config.base_ref = policy.base_ref.clone();
    config.non_git = policy.non_git;
    Ok(WorkspaceBinding {
        config,
        source: repo,
        gc_interval_ms: WORKSPACE_GC_MS,
    })
}

/// Chooses and builds the driver of a worker (RFC-0009). Secret values are read from the host
/// environment by name (`bearer_token_env`, `context_secret_env`) and are never stored.
pub(crate) fn build_backend(
    spec: &AgentSpec,
    agent_dir: &Path,
    work_dir: &Path,
    env: &[(String, String)],
) -> Result<AnyBackend, HostError> {
    let Some(kind) = spec.backend.as_ref() else {
        return Err(HostError::Config(format!(
            "agent {}: worker has no backend",
            spec.id
        )));
    };
    match kind {
        SpecBackend::AcpOmp | SpecBackend::AcpClaude => {
            let profile = if matches!(kind, SpecBackend::AcpOmp) {
                Profile::Omp
            } else {
                Profile::Claude
            };
            let (program, args) = spec.command.split_first().ok_or_else(|| {
                HostError::Config(format!(
                    "agent {}: command is required for backend {}",
                    spec.id,
                    kind.as_str()
                ))
            })?;
            let mut config = AcpDriverConfig::new(
                profile,
                PathBuf::from(program),
                work_dir.to_path_buf(),
                agent_dir.join("acp"),
            );
            config.program_args = args.to_vec();
            config.approval_mode = spec
                .approval_mode
                .clone()
                .unwrap_or_else(|| profile.default_approval_mode().to_owned());
            config.acknowledge_dangerous = spec.dangerous;
            if let Some(timeout) = spec.permission_timeout {
                config.permission_timeout = timeout;
            }
            config.env = env.to_vec();
            AcpDriver::new(config)
                .map(AnyBackend::Acp)
                .map_err(|err| HostError::Backend(format!("agent {}: {err}", spec.id)))
        }
        SpecBackend::A2aHttp => {
            let peer = spec.peer.as_ref().ok_or_else(|| {
                HostError::Config(format!("agent {}: a2a-http requires [peer]", spec.id))
            })?;
            let config = PeerConfig {
                agent_id: spec.id.clone(),
                base_url: peer.base_url.clone(),
                bearer_token_env: peer.bearer_token_env.clone(),
                context_secret_env: peer.context_secret_env.clone(),
                tenant: peer.tenant.clone(),
                timeout_secs: peer.timeout_secs,
                streaming: peer.streaming,
                task_map_path: agent_dir.join("peer-tasks.json"),
            };
            let settings = config
                .validate(|name| std::env::var(name).ok())
                .map_err(|err| HostError::Config(format!("agent {}: {err}", spec.id)))?;
            HttpBackend::connect(settings)
                .map(AnyBackend::Http)
                .map_err(|err| HostError::Backend(format!("agent {}: {err}", spec.id)))
        }
    }
}

/// Delegation endpoint of one run (RFC-0015 0015-004): a named pipe on Windows and a Unix socket
/// elsewhere. The process id makes the name unique per run.
fn delegation_endpoint(run_dir: &Path, tenant: &TenantId, agent: &str) -> PathBuf {
    let pid = std::process::id();
    if cfg!(windows) {
        PathBuf::from(format!(
            r"\\.\pipe\a2abus-{}-{agent}-{pid}",
            tenant.as_str()
        ))
    } else {
        run_dir.join(format!("delegate-{pid}.sock"))
    }
}

fn default_shim_path() -> Result<PathBuf, HostError> {
    let exe = std::env::current_exe()?;
    Ok(exe.with_file_name(format!("{SHIM_NAME}{}", std::env::consts::EXE_SUFFIX)))
}

/// Delegation configuration of a worker (RFC-0015). The endpoint is attached only when the agent
/// names peers; an agent without a delegation table gets no endpoint (RFC-0015 0015-050).
pub(crate) fn delegation_config(
    run_dir: &Path,
    tenant: &TenantId,
    spec: &AgentSpec,
) -> Result<Option<DelegationConfig>, HostError> {
    let table = &spec.delegation;
    if table.peers.is_empty() {
        return Ok(None);
    }
    let peers = table
        .peers
        .iter()
        .map(|peer| {
            AgentId::parse(peer).map_err(|err| {
                HostError::Config(format!(
                    "agent {}: delegation peer `{peer}`: {err}",
                    spec.id
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let shim = match &table.shim_path {
        Some(path) => path.clone(),
        None => default_shim_path()?,
    };
    let mut config =
        DelegationConfig::new(delegation_endpoint(run_dir, tenant, &spec.id), shim, peers);
    config.max_inflight = u32::from(table.max_inflight);
    config.max_sends_per_min = u32::from(table.max_sends_per_min);
    config.max_calls_per_min = u32::from(table.max_calls_per_min);
    Ok(Some(config))
}

/// Builds the worker builder: identity, card, approval, audit sink, working directory and an
/// optional workspace (RFC-0004, RFC-0012, RFC-0013).
pub(crate) fn worker_builder(
    config: &Config,
    spec: &AgentSpec,
    work_dir: &Path,
    sink: Arc<Mutex<dyn AuditRecorder>>,
) -> Result<WorkerBuilder, HostError> {
    let tenant = config.tenant().clone();
    let agent = AgentId::parse(&spec.id)
        .map_err(|err| HostError::Config(format!("agent id `{}`: {err}", spec.id)))?;
    let principal = Principal::named(&spec.id)
        .map_err(|err| HostError::Config(format!("agent id `{}`: {err}", spec.id)))?;
    let tasks = task::Config {
        owner: spec.id.clone(),
        agent_deadline_ms: AGENT_DEADLINE_MS,
        default_budget: Allowance::default(),
        metering: Metering {
            tokens: true,
            cost: true,
        },
    };
    let mode = spec
        .workspace
        .as_ref()
        .map_or(WorkspaceMode::PerSession, |policy| policy.mode);
    let mut builder = WorkerBuilder::new(tenant.clone(), agent, principal, tasks)
        .approval(approval_mode(spec))
        .publication(Publication {
            card: agent_card(&tenant, spec),
            signing: CardSigning::Disabled,
            timing: Timing::default(),
        })
        .audit_sink(sink)
        .working_directory(work_dir.to_path_buf(), mode);
    if let Some(policy) = &spec.workspace {
        builder = builder.workspace(workspace_binding(
            config.state_dir(),
            &tenant,
            spec,
            policy,
        )?);
    }
    Ok(builder)
}

/// One audit record handed from the pipeline to the bus bridge thread.
struct AuditJob {
    msg_id: String,
    payload: Vec<u8>,
    reply: SyncSender<Result<(), String>>,
}

/// Audit transport (RFC-0013 0013-009). `publish` returns only after the bridge thread reports
/// that the bus accepted the message, as the pipeline requires.
struct BridgePublisher {
    jobs: Sender<AuditJob>,
}

impl Publisher for BridgePublisher {
    fn publish(&mut self, event: &AuditEvent, payload: &[u8]) -> Result<(), PublishError> {
        let stopped = || PublishError("audit bridge stopped".to_owned());
        let (reply, answer) = mpsc::sync_channel(1);
        self.jobs
            .send(AuditJob {
                msg_id: event.event_id.clone(),
                payload: payload.to_vec(),
                reply,
            })
            .map_err(|_| stopped())?;
        answer.recv().map_err(|_| stopped())?.map_err(PublishError)
    }
}

/// Connection parameters of the audit bridge. The bridge keeps its own NATS connection under the
/// agent's identity.
struct AuditRoute {
    url: String,
    tenant: TenantId,
    agent: String,
    seed_path: PathBuf,
}

/// Starts the audit bridge thread and waits until its bus connection is up.
fn spawn_bridge(route: AuditRoute) -> Result<BridgePublisher, HostError> {
    let (jobs, inbox) = mpsc::channel();
    let (ready, started) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name(format!("audit-{}", route.agent))
        .spawn(move || run_bridge(route, inbox, ready))?;
    started
        .recv()
        .map_err(|_| HostError::Bus("audit bridge exited before starting".to_owned()))?
        .map_err(HostError::Bus)?;
    Ok(BridgePublisher { jobs })
}

/// Bus resources of the audit bridge.
struct BridgeLink {
    runtime: Runtime,
    bus: NatsBus,
    principal: Principal,
    stream: StreamName,
    subject: Subject,
}

fn open_link(route: &AuditRoute) -> Result<BridgeLink, HostError> {
    let runtime = hosts::runtime()?;
    let seed = hosts::read_seed(&route.seed_path)?;
    let bus = runtime.block_on(hosts::connect_bus(
        route.url.clone(),
        &route.agent,
        seed,
        route.tenant.clone(),
    ))?;
    let principal = Principal::named(&route.agent)
        .map_err(|err| HostError::Config(format!("audit identity: {err}")))?;
    let stream = StreamName::parse(&layout::audit_stream(&route.tenant))
        .map_err(|err| HostError::Config(format!("audit stream: {err}")))?;
    let subject = Subject::parse(&layout::tenant_subject(
        &route.tenant,
        &format!("audit.{}", route.agent),
    ))
    .map_err(|err| HostError::Config(format!("audit subject: {err}")))?;
    Ok(BridgeLink {
        runtime,
        bus,
        principal,
        stream,
        subject,
    })
}

/// Drops `value` while `runtime` is entered. async-nats spawns its unsubscribe work from `Drop`,
/// which needs a reactor; dropping after `block_on` returned panicked with `there is no reactor
/// running`.
fn drop_in_runtime<T>(runtime: &tokio::runtime::Runtime, value: T) {
    runtime.block_on(async move { drop(value) });
}

/// Bridge thread body: connects, reports readiness, then appends each job until the pipeline is
/// dropped. Blocking receives are avoided so the bus connection's tasks keep running.
fn run_bridge(route: AuditRoute, inbox: Receiver<AuditJob>, ready: SyncSender<Result<(), String>>) {
    let link = match open_link(&route) {
        Ok(link) => link,
        Err(err) => {
            let _ = ready.send(Err(err.to_string()));
            return;
        }
    };
    let _ = ready.send(Ok(()));
    let BridgeLink {
        runtime,
        bus,
        principal,
        stream,
        subject,
    } = link;
    runtime.block_on(async {
        loop {
            match inbox.try_recv() {
                Ok(AuditJob {
                    msg_id,
                    payload,
                    reply,
                }) => {
                    let result =
                        append(&bus, &principal, &stream, &subject, &msg_id, payload).await;
                    let _ = reply.send(result);
                }
                Err(TryRecvError::Empty) => tokio::time::sleep(AUDIT_POLL).await,
                Err(TryRecvError::Disconnected) => break,
            }
        }
    });
    drop_in_runtime(&runtime, bus);
}

async fn append(
    bus: &NatsBus,
    principal: &Principal,
    stream: &StreamName,
    subject: &Subject,
    msg_id: &str,
    payload: Vec<u8>,
) -> Result<(), String> {
    let msg_id = MsgId::parse(msg_id).map_err(|err| err.to_string())?;
    match bus
        .append(
            principal,
            stream,
            subject,
            &Headers::new(),
            Bytes::from(payload),
            &msg_id,
        )
        .await
    {
        // A repeated message id is the same record already accepted (RFC-0013 0013-010).
        Ok(_) | Err(BusError::Duplicate { .. }) => Ok(()),
        Err(err) => Err(err.to_string()),
    }
}

/// Opens the audit pipeline of the worker (RFC-0013). Records are sealed locally, spooled and
/// published through the bridge.
fn open_pipeline(
    config: &Config,
    agent: &str,
    publisher: BridgePublisher,
) -> Result<Arc<Mutex<Pipeline>>, HostError> {
    let identity = EmitterIdentity::new(config.tenant().as_str(), agent, EmitterKind::Worker)
        .map_err(|err| HostError::Config(format!("audit identity: {err}")))?;
    let pipeline = Pipeline::open(
        PipelineConfig {
            identity,
            state_dir: config.state_dir().to_path_buf(),
            spool_cap_bytes: SPOOL_CAP_BYTES,
            clock: Arc::new(AuditClock),
            signer: None,
            redactor: Redactor::new(),
        },
        Box::new(publisher),
    )
    .map_err(|err| HostError::Config(format!("audit pipeline: {err}")))?;
    Ok(Arc::new(Mutex::new(pipeline)))
}

fn flush(pipeline: &Mutex<Pipeline>) -> Result<(), HostError> {
    let mut guard = pipeline
        .lock()
        .map_err(|_| HostError::Bus("audit pipeline lock poisoned".to_owned()))?;
    guard
        .flush()
        .map(|_| ())
        .map_err(|err| HostError::Bus(format!("audit: {err}")))
}

/// One pass of the serve loop: serves one request or waits for the next backend wakeup, then pumps.
async fn step<B: Bus, D: Backend, S: task::TaskStore>(
    worker: &mut Worker<B, D, S>,
) -> Result<(), HostError> {
    let now = SystemClock
        .now_ms()
        .ok_or_else(|| HostError::Config("system clock is unavailable".to_owned()))?;
    let wait = worker.next_wakeup(now).map_or(POLL, |at| {
        Duration::from_millis(at.saturating_sub(now)).min(POLL)
    });
    let served = tokio::select! {
        result = worker.serve_one() => {
            result.map_err(|err| HostError::Bus(err.to_string()))?;
            true
        }
        () = tokio::time::sleep(wait) => false,
    };
    if !served {
        worker
            .pump()
            .await
            .map_err(|err| HostError::Bus(err.to_string()))?;
    }
    Ok(())
}

/// Serves until the stop flag is set, then drains for `drain` (RFC-0012 4.5): new requests are
/// refused with a draining reply while tasks in flight finish.
async fn drive<B: Bus, D: Backend, S: task::TaskStore>(
    worker: &mut Worker<B, D, S>,
    control: &HostControl,
    pipeline: &Mutex<Pipeline>,
    drain: Duration,
    reporter: &mut HealthPublisher<'_>,
) -> Result<(), HostError> {
    let mut draining_since: Option<Instant> = None;
    let mut last_flush = Instant::now();
    loop {
        if draining_since.is_none() && control.stop_requested() {
            worker.set_draining(true);
            draining_since = Some(Instant::now());
        }
        if draining_since.is_some_and(|since| since.elapsed() >= drain) {
            return Ok(());
        }
        step(worker).await?;
        reporter.publish(worker);
        // Audit records reach the pipeline through the sink; the buffered notes are not needed.
        let _ = worker.take_audit();
        if last_flush.elapsed() >= FLUSH_EVERY {
            flush(pipeline)?;
            last_flush = Instant::now();
        }
    }
}

/// One step of the readiness wait (RFC-0004 0004-015, RFC-0012 0012-009).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LiveWait {
    Live,
    Stopped,
    TimedOut,
    Poll,
}

/// Decides the next readiness-wait step. Liveness wins over a stop request, so a live backend is
/// reported ready and then drained normally.
fn live_wait(live: bool, stop: bool, elapsed: Duration, timeout: Duration) -> LiveWait {
    if live {
        LiveWait::Live
    } else if stop {
        LiveWait::Stopped
    } else if elapsed >= timeout {
        LiveWait::TimedOut
    } else {
        LiveWait::Poll
    }
}

/// Pumps the worker until its backend reports live, so the ready marker and presence only follow a
/// live backend. Requests are not served meanwhile. Returns `Ok(false)` when the stop flag is set
/// first and an error when the backend stays down past `timeout`.
async fn await_backend_live<B: Bus, D: Backend, S: task::TaskStore>(
    worker: &mut Worker<B, D, S>,
    backend: &SharedBackend,
    control: &HostControl,
    timeout: Duration,
    reporter: &mut HealthPublisher<'_>,
) -> Result<bool, HostError> {
    let started = Instant::now();
    loop {
        reporter.publish(worker);
        let health = backend.health();
        match live_wait(
            health.live,
            control.stop_requested(),
            started.elapsed(),
            timeout,
        ) {
            LiveWait::Live => return Ok(true),
            LiveWait::Stopped => return Ok(false),
            LiveWait::TimedOut => {
                let reason = health
                    .reason
                    .unwrap_or_else(|| "no reason reported".to_owned());
                return Err(HostError::Backend(format!(
                    "backend not live after {timeout:?}: {reason}"
                )));
            }
            LiveWait::Poll => {}
        }
        worker
            .pump()
            .await
            .map_err(|err| HostError::Bus(err.to_string()))?;
        tokio::time::sleep(POLL).await;
    }
}

/// Publishes the driver health to the supervisor, writing only when it changes (RFC-0004
/// 0004-015). The supervisor keeps the agent out of `ready` while the backend is not live.
struct HealthPublisher<'a> {
    control: &'a HostControl,
    last: Option<HostHealth>,
}

impl HealthPublisher<'_> {
    /// Writes the driver health unless it equals the last value written. A failed write is logged
    /// and retried on the next pass; the supervisor keeps its last value meanwhile.
    fn publish<B: Bus, D: Backend, S: task::TaskStore>(&mut self, worker: &Worker<B, D, S>) {
        let health = worker.backend_health();
        let report = HostHealth {
            live: health.live,
            reason: health.reason,
        };
        if self.last.as_ref() == Some(&report) {
            return;
        }
        match self.control.report_health(&report) {
            Ok(()) => self.last = Some(report),
            Err(err) => tracing::warn!(error = %err, "could not publish driver health"),
        }
    }
}

/// Runs one configured worker until the host stops (RFC-0012 4.5).
///
/// Connects under the agent's identity, opens the worker, reports readiness, serves until the stop
/// flag is set, drains, and ends the backend's process tree within the configured grace period.
pub fn run_worker(
    config: &Config,
    agent: &AgentId,
    control: &HostControl,
) -> Result<(), HostError> {
    let spec = config
        .agent(agent.as_str())
        .filter(|spec| spec.is_worker())
        .ok_or_else(|| {
            HostError::Config(format!("`{}` is not a configured worker", agent.as_str()))
        })?;
    let state = config.state_dir();
    hosts::logging::init(state, agent.as_str(), config.log())?;
    let tenant = config.tenant().clone();
    let agent_dir = state.join("agents").join(&spec.id);
    let run_dir = state.join("run").join(&spec.id);
    let work_dir = working_directory(state, &tenant, spec);
    for dir in [&agent_dir, &run_dir, &work_dir] {
        fs::create_dir_all(dir)?;
    }

    let host_env: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .collect();
    let env = child_env(spec, tenant.as_str(), &host_env)?;
    let mut backend = SharedBackend::new(build_backend(spec, &agent_dir, &work_dir, &env)?);

    let seed_path = creds::seed_path(state, &spec.id);
    let url = broker::nats_url(config);
    let publisher = spawn_bridge(AuditRoute {
        url: url.clone(),
        tenant: tenant.clone(),
        agent: spec.id.clone(),
        seed_path: seed_path.clone(),
    })?;
    let pipeline = open_pipeline(config, &spec.id, publisher)?;
    let sink: Arc<Mutex<dyn AuditRecorder>> = pipeline.clone();

    let runtime = hosts::runtime()?;
    let delegation = delegation_config(&run_dir, &tenant, spec)?;
    let claim_deadline = Instant::now() + CLAIM_RETRY_WINDOW;
    let mut worker = loop {
        let seed = hosts::read_seed(&seed_path)?;
        let bus = runtime.block_on(hosts::connect_bus(
            url.clone(),
            &spec.id,
            seed,
            tenant.clone(),
        ))?;
        let store = FileStore::open(agent_dir.join("tasks.log"))
            .map_err(|err| HostError::Config(format!("task store: {err}")))?;
        let builder = worker_builder(config, spec, &work_dir, sink.clone())?;
        match runtime.block_on(builder.build(bus, backend.clone(), store, Box::new(SystemClock))) {
            Ok(worker) => break worker,
            Err(WorkerError::Config(SERVED_ELSEWHERE)) if Instant::now() < claim_deadline => {
                tracing::info!(
                    agent = %spec.id,
                    "previous presence record still live; retrying claim"
                );
                std::thread::sleep(CLAIM_RETRY_EVERY);
            }
            Err(err) => return Err(HostError::Bus(err.to_string())),
        }
    };
    if let Some(delegation) = delegation {
        worker
            .attach_delegation(delegation)
            .map_err(|err| HostError::Bus(err.to_string()))?;
    }
    let mut reporter = HealthPublisher {
        control,
        last: None,
    };
    let served = match runtime.block_on(await_backend_live(
        &mut worker,
        &backend,
        control,
        spec.readiness.timeout,
        &mut reporter,
    )) {
        Ok(true) => control
            .report_ready()
            .map_err(HostError::from)
            .and_then(|()| {
                runtime.block_on(drive(
                    &mut worker,
                    control,
                    &pipeline,
                    config.drain(),
                    &mut reporter,
                ))
            }),
        Ok(false) => Ok(()),
        Err(err) => Err(err),
    };

    worker.shutdown_delegation();
    drop_in_runtime(&runtime, worker);
    let stopped = backend.shutdown(config.grace());
    let flushed = flush(&pipeline);
    served?;
    stopped.map_err(|err| HostError::Backend(format!("agent {}: {err}", spec.id)))?;
    flushed
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Stands in for an async-nats subscriber: its `Drop` spawns a task, which needs a reactor.
    struct SpawnOnDrop;

    impl Drop for SpawnOnDrop {
        fn drop(&mut self) {
            drop(tokio::spawn(async {}));
        }
    }

    /// Regression: the worker's bus must be dropped inside the runtime, so its `Drop` finds a reactor.
    #[test]
    fn drop_in_runtime_gives_drop_a_reactor() {
        let runtime = hosts::runtime().expect("runtime");
        drop_in_runtime(&runtime, SpawnOnDrop);
    }

    /// Guards the premise of the fix: a drop that spawns panics outside a runtime.
    #[test]
    fn dropping_outside_runtime_panics_without_reactor() {
        assert!(std::panic::catch_unwind(|| drop(SpawnOnDrop)).is_err());
    }

    const SHA: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    fn config_with(state: &Path, agent_body: &str) -> Config {
        let text = format!(
            "config_version = 1\n[bus]\nsha256 = \"{SHA}\"\n\
             [[agent]]\nid = \"omp\"\nkind = \"worker\"\nbackend = \"acp-omp\"\n\
             command = [\"omp\", \"--mode\", \"acp\"]\nrepo = '{}'\n{agent_body}",
            state.join("repo").display()
        );
        Config::parse(&text, state.to_path_buf()).expect("fixture parses")
    }

    fn spec_of(state: &Path) -> AgentSpec {
        config_with(state, "").agent("omp").expect("agent").clone()
    }

    fn value<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
        env.iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    fn host() -> Vec<(String, String)> {
        vec![
            ("PATH".to_owned(), "/bin".to_owned()),
            ("ANTHROPIC_API_KEY".to_owned(), "sk-test".to_owned()),
            ("A2AB_TENANT".to_owned(), "evil".to_owned()),
        ]
    }

    #[test]
    fn req_0012_009_ready_waits_for_live_backend() {
        let timeout = Duration::from_secs(30);
        let early = Duration::from_secs(1);
        assert_eq!(live_wait(false, false, early, timeout), LiveWait::Poll);
        assert_eq!(live_wait(true, false, early, timeout), LiveWait::Live);
        assert_eq!(live_wait(true, true, early, timeout), LiveWait::Live);
        assert_eq!(live_wait(false, true, early, timeout), LiveWait::Stopped);
        assert_eq!(
            live_wait(false, false, timeout, timeout),
            LiveWait::TimedOut
        );
    }

    // req_0012_062: provider keys stay out of the child environment unless allowed.
    #[test]
    fn req_0012_062_provider_keys_are_dropped_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let spec = spec_of(dir.path());
        assert!(config::is_provider_key("ANTHROPIC_API_KEY"));
        let env = child_env(&spec, "acme", &host()).unwrap();
        assert_eq!(value(&env, "ANTHROPIC_API_KEY"), None);
        assert_eq!(value(&env, "PATH"), None);
    }

    // req_0012_062: env_allow forwards a named provider key only.
    #[test]
    fn req_0012_062_env_allow_forwards_named_key() {
        let dir = tempfile::tempdir().unwrap();
        let mut spec = spec_of(dir.path());
        spec.env_allow = vec!["ANTHROPIC_API_KEY".to_owned()];
        let env = child_env(&spec, "acme", &host()).unwrap();
        assert_eq!(value(&env, "ANTHROPIC_API_KEY"), Some("sk-test"));
    }

    // req_0012_062: identity variables are set by the supervisor, never inherited.
    #[test]
    fn req_0012_062_identity_variables_are_supervisor_owned() {
        let dir = tempfile::tempdir().unwrap();
        let mut spec = spec_of(dir.path());
        spec.passthrough_all = true;
        let env = child_env(&spec, "acme", &host()).unwrap();
        assert_eq!(value(&env, "A2AB_TENANT"), Some("acme"));
        assert_eq!(value(&env, "A2AB_AGENT"), Some("omp"));
        assert_eq!(value(&env, "PATH"), Some("/bin"));
        assert_eq!(value(&env, "ANTHROPIC_API_KEY"), None);
    }

    // req_0012_062: an env_require name missing from the result fails the start.
    #[test]
    fn req_0012_062_missing_required_env_fails() {
        let dir = tempfile::tempdir().unwrap();
        let mut spec = spec_of(dir.path());
        spec.env_require = vec!["NEEDED_VAR".to_owned()];
        assert!(matches!(
            child_env(&spec, "acme", &host()),
            Err(HostError::Config(_))
        ));
        spec.env.insert("NEEDED_VAR".to_owned(), "1".to_owned());
        assert_eq!(
            value(&child_env(&spec, "acme", &host()).unwrap(), "NEEDED_VAR"),
            Some("1")
        );
    }

    // req_0012_062: the agent table may not inject provider keys or identity variables.
    #[test]
    fn req_0012_062_reserved_names_in_env_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut spec = spec_of(dir.path());
        spec.env
            .insert("ANTHROPIC_API_KEY".to_owned(), "x".to_owned());
        assert!(child_env(&spec, "acme", &host()).is_err());
        spec.env.clear();
        spec.env.insert("A2AB_AGENT".to_owned(), "x".to_owned());
        assert!(child_env(&spec, "acme", &host()).is_err());
    }

    // req_0012_051: an explicit mode is mapped; `ask-all` asks for everything, others ask for exec.
    #[test]
    fn req_0012_051_approval_mode_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let mut spec = spec_of(dir.path());
        spec.approval_mode = Some("ask-all".to_owned());
        assert!(matches!(approval_mode(&spec), ApprovalMode::AskAll));
        spec.approval_mode = Some("ask-exec".to_owned());
        assert!(matches!(approval_mode(&spec), ApprovalMode::AskExec));
    }

    // req_0006_004: with no configured mode, omp starts with ask-exec and other backends ask-all.
    #[test]
    fn req_0006_004_backend_default_approval() {
        let dir = tempfile::tempdir().unwrap();
        let mut spec = spec_of(dir.path());
        spec.approval_mode = None;
        spec.backend = Some(SpecBackend::AcpOmp);
        assert!(matches!(approval_mode(&spec), ApprovalMode::AskExec));
        spec.backend = Some(SpecBackend::AcpClaude);
        assert!(matches!(approval_mode(&spec), ApprovalMode::AskAll));
    }

    // req_0004_001: the card names the agent and advertises the bus interface.
    #[test]
    fn req_0004_001_card_names_agent_and_bus_interface() {
        let dir = tempfile::tempdir().unwrap();
        let spec = spec_of(dir.path());
        let tenant = TenantId::parse("acme").unwrap();
        let card = agent_card(&tenant, &spec);
        assert_eq!(card.name, "omp");
        assert_eq!(card.description, "omp");
        let interface = &card.supported_interfaces[0];
        assert_eq!(interface.url, "bus://acme/omp");
        assert_eq!(interface.protocol_binding, BINDING);
        // presence.rs rejects a card without skills; every worker must declare one.
        assert_eq!(card.skills.len(), 1);
        assert_eq!(card.skills[0].id, "omp");
        assert_eq!(card.skills[0].input_modes, vec!["text/plain".to_owned()]);
    }

    // req_0009_011: a worker needs a command for its ACP driver.
    #[test]
    fn req_0009_011_acp_worker_requires_command() {
        let dir = tempfile::tempdir().unwrap();
        let mut spec = spec_of(dir.path());
        spec.command.clear();
        assert!(matches!(
            build_backend(&spec, dir.path(), dir.path(), &[]),
            Err(HostError::Config(_))
        ));
    }

    // req_0009_011: an a2a-http worker needs its [peer] table.
    #[test]
    fn req_0009_011_http_worker_requires_peer() {
        let dir = tempfile::tempdir().unwrap();
        let mut spec = spec_of(dir.path());
        spec.backend = Some(SpecBackend::A2aHttp);
        spec.peer = None;
        assert!(matches!(
            build_backend(&spec, dir.path(), dir.path(), &[]),
            Err(HostError::Config(_))
        ));
    }

    // req_0015_050: no delegation peers means no delegation endpoint.
    #[test]
    fn req_0015_050_no_peers_no_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let spec = spec_of(dir.path());
        let tenant = TenantId::parse("acme").unwrap();
        assert!(
            delegation_config(dir.path(), &tenant, &spec)
                .unwrap()
                .is_none()
        );
    }

    // req_0015_004: the endpoint is created per run and the limits follow the table.
    #[test]
    fn req_0015_004_endpoint_per_run_with_limits() {
        let dir = tempfile::tempdir().unwrap();
        let mut spec = spec_of(dir.path());
        spec.delegation.peers = vec!["bob".to_owned()];
        spec.delegation.shim_path = Some(dir.path().join("shim"));
        spec.delegation.max_inflight = 3;
        spec.delegation.max_sends_per_min = 7;
        spec.delegation.max_calls_per_min = 9;
        let tenant = TenantId::parse("acme").unwrap();
        let config = delegation_config(dir.path(), &tenant, &spec)
            .unwrap()
            .expect("endpoint configured");
        assert_eq!(config.max_inflight, 3);
        assert_eq!(config.max_sends_per_min, 7);
        assert_eq!(config.max_calls_per_min, 9);
        let endpoint = delegation_endpoint(dir.path(), &tenant, "omp");
        let text = endpoint.to_string_lossy().into_owned();
        assert!(text.contains(&std::process::id().to_string()));
    }

    // req_0012_030: a worker with a workspace runs in its workspace, not in the repository.
    #[test]
    fn req_0012_030_workspace_worker_runs_in_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let mut spec = spec_of(dir.path());
        // ACP backends default to a workspace (config.rs); clear it to exercise the private path.
        spec.workspace = None;
        let tenant = TenantId::parse("acme").unwrap();
        let private = working_directory(dir.path(), &tenant, &spec);
        assert!(private.starts_with(dir.path().join("agents")));
        spec.workspace = Some(WorkspacePolicy {
            mode: WorkspaceMode::PerSession,
            base_ref: "main".to_owned(),
            non_git: a2a_workspace::NonGitPolicy::Refuse,
        });
        let workspace = working_directory(dir.path(), &tenant, &spec);
        assert!(workspace.starts_with(dir.path().join("ws")));
        assert_ne!(workspace, dir.path().join("repo"));
    }

    // req_0012_038: the workspace binding roots workspaces under the state directory.
    #[test]
    fn req_0012_038_workspace_binding_roots_under_state() {
        let dir = tempfile::tempdir().unwrap();
        let spec = spec_of(dir.path());
        let tenant = TenantId::parse("acme").unwrap();
        let policy = WorkspacePolicy {
            mode: WorkspaceMode::PerTask,
            base_ref: "dev".to_owned(),
            non_git: a2a_workspace::NonGitPolicy::Copy,
        };
        let binding = workspace_binding(dir.path(), &tenant, &spec, &policy).unwrap();
        assert_eq!(binding.config.base_ref, "dev");
        assert_eq!(binding.config.non_git, a2a_workspace::NonGitPolicy::Copy);
        assert_eq!(binding.source, dir.path().join("repo"));
    }

    // req_0013_009: the audit bridge rejects a stopped pipeline rather than hanging.
    #[test]
    fn req_0013_009_publisher_reports_stopped_bridge() {
        let (jobs, inbox) = mpsc::channel::<AuditJob>();
        drop(inbox);
        let mut publisher = BridgePublisher { jobs };
        let event = AuditEvent {
            event_id: "e1".to_owned(),
            ..Default::default()
        };
        assert!(publisher.publish(&event, b"payload").is_err());
    }

    // Live smoke: needs a running bus and a configured worker. Run with `cargo test -- --ignored`.
    #[test]
    #[ignore = "needs a running NATS bus; set A2AB_LIVE_CONFIG and A2AB_LIVE_AGENT"]
    fn live_worker_host_smoke() {
        let path = std::env::var("A2AB_LIVE_CONFIG").expect("A2AB_LIVE_CONFIG names the config");
        let agent_name =
            std::env::var("A2AB_LIVE_AGENT").expect("A2AB_LIVE_AGENT names the worker");
        let config = Config::load(Path::new(&path)).expect("config loads");
        let agent = AgentId::parse(&agent_name).expect("agent id parses");
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let control = HostControl::new(
            config.state_dir().to_path_buf(),
            agent.as_str(),
            Arc::clone(&stop),
        );
        let stopper = Arc::clone(&stop);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(10));
            stopper.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        run_worker(&config, &agent, &control).expect("worker runs until stop and drains");
    }
}
