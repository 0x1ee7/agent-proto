//! Runtime integration tests: a real [`Worker`] over `bus-mem` with a virtual clock and the
//! testkit [`MockBackend`]. Requirement tests live in the `req_*` scenario files; this module
//! holds the harness they share.
//!
//! The executor is a hand-written poll loop: each iteration polls the scenario, then one
//! `serve_one`, then `pump`. Nothing sleeps; time moves only through [`MemControl::advance`].
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod peer;
mod req_0002;
mod req_0005;
mod req_0006;
mod req_0014;
mod req_0015;

use std::collections::HashMap;
use std::pin::{Pin, pin};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use a2a_core::approval::{ALLOW_ONCE_ID, REJECT_ONCE_ID};
use a2a_core::budget::{Allowance, Metering};
use a2a_core::envelope::{self, Clock, ReplyDecodeError, RequestBody, RequestParts, WireMessage};
use a2a_core::subject::Op;
use a2a_core::task::{self, StoreError, TaskStore};
use a2a_types::ids::{AgentId, MessageId, TenantId};
use a2a_types::proto::a2a as pb;
use a2a_types::proto::a2abus;
use a2a_types::vocab::{FAILURE_KEY, ext::APPROVAL};
use bus_api::bus::{Bus, Consumer};
use bus_api::control::BusControl;
use bus_api::error::BusError;
use bus_api::headers::Headers;
use bus_api::message::{Entry, PublishAck, Reply, StartPosition};
use bus_api::name::{
    Bucket, ConsumerName, Key, KeyPrefix, MsgId, Principal, StreamName, Subject, SubjectPattern,
};
use bus_api::value::{Digest, Revision};
use bus_mem::{Grant, MemBus as BusMem, MemConfig, MemControl, StreamConfig, m1_capabilities};
use bytes::Bytes;
use pbjson_types::{Struct, Value, value::Kind};

use crate::backend::PermissionAnswer;
use crate::backend::{Capabilities, Protocol, UsageSource};
use crate::config::{ApprovalMode, WorkerBuilder};
use crate::mock::{Command, MockBackend, MockControl, Step};
use crate::worker::Worker;
use a2a_workspace::WorkspaceMode;

pub(crate) const TENANT: &str = "acme";
pub(crate) const WORKER: &str = "worker";
pub(crate) const REQUESTER: &str = "planner";
pub(crate) const OPERATOR: &str = "operator";
pub(crate) const OBSERVER: &str = "observer";
/// Configured working directory approvals report (0006-006); never the process cwd.
pub(crate) const WORK_DIR: &str = "agent-work";
const EVENT_STREAM: &str = "A2AB_EV_acme";
const TTL_MS: u64 = 60_000;
const MAX_POLLS: usize = 500_000;
/// Default approval expiry (RFC-0006, `DEFAULT_EXPIRY_MS`).
pub(crate) const APPROVAL_EXPIRY_MS: u64 = 300_000;
/// Cancel grace before the Worker kills the backend (0005-037, `CANCEL_GRACE_MS`).
pub(crate) const CANCEL_GRACE_MS: u64 = 15_000;

pub(crate) type TestWorker = Worker<MemBus, MockBackend, SharedStore>;

/// Shared handle onto one bus-mem bus. The Worker owns one clone and the harness keeps the
/// other, so requests and the Worker observe the same state.
#[derive(Clone)]
pub(crate) struct MemBus(Arc<BusMem>);

impl MemBus {
    pub(crate) fn control(&self) -> MemControl {
        self.0.control()
    }
}

impl Bus for MemBus {
    type Requests = <BusMem as Bus>::Requests;
    type Consumer = <BusMem as Bus>::Consumer;
    type Watch = <BusMem as Bus>::Watch;

    fn capabilities(&self) -> &bus_api::capabilities::Capabilities {
        self.0.capabilities()
    }

    fn publish(
        &self,
        who: &Principal,
        subject: &Subject,
        headers: &Headers,
        payload: Bytes,
    ) -> impl Future<Output = Result<PublishAck, BusError>> + Send {
        self.0.publish(who, subject, headers, payload)
    }

    fn request(
        &self,
        who: &Principal,
        subject: &Subject,
        headers: &Headers,
        payload: Bytes,
        timeout: Duration,
    ) -> impl Future<Output = Result<Reply, BusError>> + Send {
        self.0.request(who, subject, headers, payload, timeout)
    }

    fn serve(
        &self,
        who: &Principal,
        pattern: &SubjectPattern,
    ) -> impl Future<Output = Result<Self::Requests, BusError>> + Send {
        self.0.serve(who, pattern)
    }

    fn append(
        &self,
        who: &Principal,
        stream: &StreamName,
        subject: &Subject,
        headers: &Headers,
        payload: Bytes,
        msg_id: &MsgId,
    ) -> impl Future<Output = Result<PublishAck, BusError>> + Send {
        self.0
            .append(who, stream, subject, headers, payload, msg_id)
    }

    fn consume(
        &self,
        who: &Principal,
        stream: &StreamName,
        consumer: &ConsumerName,
        filter: &SubjectPattern,
        start: StartPosition,
    ) -> impl Future<Output = Result<Self::Consumer, BusError>> + Send {
        self.0.consume(who, stream, consumer, filter, start)
    }

    fn kv_put(
        &self,
        who: &Principal,
        bucket: &Bucket,
        key: &Key,
        value: Bytes,
        ttl: Option<Duration>,
    ) -> impl Future<Output = Result<Revision, BusError>> + Send {
        self.0.kv_put(who, bucket, key, value, ttl)
    }

    fn kv_get(
        &self,
        who: &Principal,
        bucket: &Bucket,
        key: &Key,
    ) -> impl Future<Output = Result<Entry, BusError>> + Send {
        self.0.kv_get(who, bucket, key)
    }

    fn kv_create(
        &self,
        who: &Principal,
        bucket: &Bucket,
        key: &Key,
        value: Bytes,
        ttl: Option<Duration>,
    ) -> impl Future<Output = Result<Revision, BusError>> + Send {
        self.0.kv_create(who, bucket, key, value, ttl)
    }

    fn kv_update(
        &self,
        who: &Principal,
        bucket: &Bucket,
        key: &Key,
        value: Bytes,
        expected: Revision,
        ttl: Option<Duration>,
    ) -> impl Future<Output = Result<Revision, BusError>> + Send {
        self.0.kv_update(who, bucket, key, value, expected, ttl)
    }

    fn kv_delete(
        &self,
        who: &Principal,
        bucket: &Bucket,
        key: &Key,
        expected: Option<Revision>,
    ) -> impl Future<Output = Result<Revision, BusError>> + Send {
        self.0.kv_delete(who, bucket, key, expected)
    }

    fn kv_watch(
        &self,
        who: &Principal,
        bucket: &Bucket,
        prefix: &KeyPrefix,
    ) -> impl Future<Output = Result<Self::Watch, BusError>> + Send {
        self.0.kv_watch(who, bucket, prefix)
    }

    fn blob_put(
        &self,
        who: &Principal,
        tenant: &TenantId,
        bytes: Bytes,
    ) -> impl Future<Output = Result<Digest, BusError>> + Send {
        self.0.blob_put(who, tenant, bytes)
    }

    fn blob_get(
        &self,
        who: &Principal,
        tenant: &TenantId,
        digest: &Digest,
    ) -> impl Future<Output = Result<Bytes, BusError>> + Send {
        self.0.blob_get(who, tenant, digest)
    }

    fn close(&self) -> impl Future<Output = Result<(), BusError>> + Send {
        self.0.close()
    }
}

/// [`TaskStore`] whose records stay readable by the test after the Worker takes ownership.
#[derive(Clone, Default)]
pub(crate) struct SharedStore(Arc<Mutex<Vec<Vec<u8>>>>);

impl TaskStore for SharedStore {
    fn append(&mut self, record: &[u8]) -> Result<(), StoreError> {
        self.0.lock().unwrap().push(record.to_vec());
        Ok(())
    }

    fn records(&self) -> Result<Vec<Vec<u8>>, StoreError> {
        Ok(self.0.lock().unwrap().clone())
    }
}

/// Virtual wall clock: the bus-mem virtual time, in milliseconds.
struct VirtualClock(MemControl);

impl Clock for VirtualClock {
    fn now_ms(&self) -> Option<u64> {
        self.0
            .now()
            .ok()
            .and_then(|now| u64::try_from(now.as_millis()).ok())
    }
}

pub(crate) struct Rig {
    pub(crate) bus: MemBus,
    pub(crate) mock: MockControl,
    pub(crate) store: SharedStore,
    pub(crate) worker: TestWorker,
}

/// A rig with the default agent deadline and `AskAll` approvals.
pub(crate) fn rig() -> Rig {
    rig_with(ApprovalMode::AskAll, 3_600_000)
}

pub(crate) fn rig_with(approval: ApprovalMode, agent_deadline_ms: u64) -> Rig {
    rig_with_quota(
        approval,
        agent_deadline_ms,
        crate::config::DEFAULT_MAX_WAITING_PER_CONTEXT,
    )
}

/// A rig whose per-context waiting quota is `quota` (0005-028).
pub(crate) fn rig_with_quota(approval: ApprovalMode, agent_deadline_ms: u64, quota: usize) -> Rig {
    let bus = MemBus(Arc::new(
        BusMem::new(MemConfig {
            seed: 7,
            capabilities: m1_capabilities(),
            streams: vec![StreamConfig {
                name: stream(),
                subjects: vec![SubjectPattern::parse("a2a.v1.*.ev.>").unwrap()],
                retention_messages: None,
            }],
            kv_buckets: Vec::new(),
            grants: grants(),
            ack_wait: Duration::from_secs(30),
            overload_retry_after: Duration::from_secs(1),
            blob_tenant_quota: 1 << 20,
        })
        .unwrap(),
    ));
    let (backend, mock) = MockBackend::new(Capabilities {
        protocol: Protocol::Acp,
        resume: false,
        cancel: true,
        usage_sources: vec![UsageSource::DriverCounted],
        approval_modes: vec!["default".to_owned()],
        streaming_text: true,
    });
    let store = SharedStore::default();
    let tasks = task::Config {
        owner: WORKER.to_owned(),
        agent_deadline_ms,
        default_budget: Allowance::default(),
        metering: Metering {
            tokens: true,
            cost: true,
        },
    };
    let builder = WorkerBuilder::new(tenant(), agent(WORKER), principal(WORKER), tasks)
        .approval(approval)
        .max_waiting_per_context(quota)
        .working_directory(WORK_DIR, WorkspaceMode::PerSession);
    let worker = block_on(builder.build(
        bus.clone(),
        backend,
        store.clone(),
        Box::new(VirtualClock(bus.control())),
    ))
    .unwrap();
    Rig {
        bus,
        mock,
        store,
        worker,
    }
}

fn grants() -> Vec<Grant> {
    let rpc = SubjectPattern::parse("a2a.v1.acme.rpc.worker.>").unwrap();
    let ev = SubjectPattern::parse("a2a.v1.acme.ev.worker.>").unwrap();
    let requester = |name: &str| Grant {
        principal: principal(name),
        tenant: tenant(),
        publish: vec![rpc.clone()],
        serve: Vec::new(),
        streams: vec![stream()],
    };
    vec![
        Grant {
            principal: principal(WORKER),
            tenant: tenant(),
            publish: vec![ev],
            serve: vec![rpc.clone()],
            streams: vec![stream()],
        },
        requester(REQUESTER),
        requester(OPERATOR),
        Grant {
            principal: principal(OBSERVER),
            tenant: tenant(),
            publish: Vec::new(),
            serve: Vec::new(),
            streams: vec![stream()],
        },
    ]
}

fn stream() -> StreamName {
    StreamName::parse(EVENT_STREAM).unwrap()
}

fn tenant() -> TenantId {
    TenantId::parse(TENANT).unwrap()
}

fn agent(name: &str) -> AgentId {
    AgentId::parse(name).unwrap()
}

fn principal(name: &str) -> Principal {
    Principal::named(name).unwrap()
}

fn poll_once<F: Future + ?Sized>(fut: Pin<&mut F>) -> Poll<F::Output> {
    let mut cx = Context::from_waker(Waker::noop());
    fut.poll(&mut cx)
}

/// Polls a future to completion with no concurrent work.
pub(crate) fn block_on<T>(fut: impl Future<Output = T>) -> T {
    let mut fut = pin!(fut);
    for _ in 0..MAX_POLLS {
        if let Poll::Ready(value) = poll_once(fut.as_mut()) {
            return value;
        }
    }
    panic!("future did not complete within {MAX_POLLS} polls");
}

/// Runs `scenario` while the Worker serves requests and pumps its state machine.
///
/// Each iteration polls the scenario, then one `serve_one`, then `pump`. The scenario is the
/// only place time advances, so the Worker's deadline and timer handling observe the same
/// virtual clock the scenario moves.
pub(crate) fn drive<T>(worker: &mut TestWorker, scenario: impl Future<Output = T>) -> T {
    let mut scenario = pin!(scenario);
    for _ in 0..MAX_POLLS {
        if let Poll::Ready(value) = poll_once(scenario.as_mut()) {
            return value;
        }
        {
            let mut serve = pin!(worker.serve_one());
            if let Poll::Ready(result) = poll_once(serve.as_mut()) {
                result.expect("serve_one failed");
            }
        }
        block_on(worker.pump()).expect("pump failed");
    }
    panic!("scenario did not complete within {MAX_POLLS} polls");
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

pub(crate) fn now_ms(bus: &MemBus) -> u64 {
    let now = bus.control().now().unwrap();
    u64::try_from(now.as_millis()).unwrap()
}

pub(crate) fn advance(bus: &MemBus, ms: u64) {
    bus.control().advance(Duration::from_millis(ms)).unwrap();
}

/// Sends one RPC to the worker as `sender` and returns the raw reply.
pub(crate) async fn call(
    bus: &MemBus,
    sender: &str,
    op: Op,
    body: RequestBody,
    extensions: Vec<String>,
) -> Reply {
    call_ttl(bus, sender, op, body, extensions, TTL_MS).await
}

/// As [`call`], with the request's time-to-live set to `ttl_ms`.
pub(crate) async fn call_ttl(
    bus: &MemBus,
    sender: &str,
    op: Op,
    body: RequestBody,
    extensions: Vec<String>,
    ttl_ms: u64,
) -> Reply {
    let parts = RequestParts {
        tenant: tenant(),
        agent: agent(WORKER),
        msg_id: MessageId::generate(),
        sender: agent(sender),
        origin: None,
        ts_ms: now_ms(bus),
        ttl_ms,
        traceparent: None,
        tracestate: None,
        extensions,
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

fn text_part(text: &str) -> pb::Part {
    pb::Part {
        content: Some(pb::part::Content::Text(text.to_owned())),
        ..Default::default()
    }
}

fn message(context_id: &str, task_id: &str, parts: Vec<pb::Part>) -> pb::Message {
    pb::Message {
        message_id: MessageId::generate().as_str().to_owned(),
        context_id: context_id.to_owned(),
        task_id: task_id.to_owned(),
        role: pb::Role::User as i32,
        parts,
        ..Default::default()
    }
}

/// Starts a new task (empty context and task ids) and returns the reply payload.
pub(crate) async fn submit(bus: &MemBus, text: &str) -> pb::Task {
    let reply = send_text(bus, "", "", text).await.unwrap();
    match reply.payload {
        Some(pb::send_message_response::Payload::Task(task)) => task,
        other => panic!("expected Task reply, got {other:?}"),
    }
}

/// Starts a new task in an existing context (empty task id) and returns the reply payload.
pub(crate) async fn submit_in(bus: &MemBus, context_id: &str, text: &str) -> pb::Task {
    let reply = send_text(bus, context_id, "", text).await.unwrap();
    match reply.payload {
        Some(pb::send_message_response::Payload::Task(task)) => task,
        other => panic!("expected Task reply, got {other:?}"),
    }
}

/// Sends a text message as the requester; the raw decoded reply is returned.
pub(crate) async fn send_text(
    bus: &MemBus,
    context_id: &str,
    task_id: &str,
    text: &str,
) -> Result<pb::SendMessageResponse, ReplyDecodeError> {
    let body = RequestBody::SendMessage(pb::SendMessageRequest {
        tenant: TENANT.to_owned(),
        message: Some(message(context_id, task_id, vec![text_part(text)])),
        configuration: Some(pb::SendMessageConfiguration {
            return_immediately: true,
            ..Default::default()
        }),
        ..Default::default()
    });
    let reply = call(bus, REQUESTER, Op::Send, body, Vec::new()).await;
    envelope::decode_reply::<pb::SendMessageResponse>(&reply)
}

pub(crate) async fn get(bus: &MemBus, id: &str) -> pb::Task {
    let body = RequestBody::GetTask(pb::GetTaskRequest {
        tenant: TENANT.to_owned(),
        id: id.to_owned(),
        history_length: None,
    });
    let reply = call(bus, REQUESTER, Op::Get, body, Vec::new()).await;
    envelope::decode_reply::<pb::Task>(&reply).unwrap()
}

pub(crate) async fn cancel(bus: &MemBus, id: &str) -> Result<pb::Task, ReplyDecodeError> {
    let body = RequestBody::CancelTask(pb::CancelTaskRequest {
        tenant: TENANT.to_owned(),
        id: id.to_owned(),
        metadata: None,
    });
    let reply = call(bus, REQUESTER, Op::Cancel, body, Vec::new()).await;
    envelope::decode_reply::<pb::Task>(&reply)
}

/// Polls `GetTask` until the task reaches `want`; panics after a bounded number of yields.
pub(crate) async fn wait_for_state(bus: &MemBus, id: &str, want: pb::TaskState) -> pb::Task {
    for _ in 0..10_000 {
        let task = get(bus, id).await;
        if state(&task) == want as i32 {
            return task;
        }
        yield_now().await;
    }
    panic!("task {id} never reached {want:?}");
}

pub(crate) fn state(task: &pb::Task) -> i32 {
    task.status.as_ref().map_or(0, |status| status.state)
}

/// Failure code the Worker attached to a FAILED task (`a2abus.failure` metadata).
pub(crate) fn failure_code(task: &pb::Task) -> Option<String> {
    let meta = task.status.as_ref()?.message.as_ref()?.metadata.as_ref()?;
    let Kind::StructValue(failure) = meta.fields.get(FAILURE_KEY)?.kind.as_ref()? else {
        return None;
    };
    match failure.fields.get("code")?.kind.as_ref()? {
        Kind::StringValue(code) => Some(code.clone()),
        _ => None,
    }
}

/// Detail text the Worker attached to a FAILED task (`a2abus.failure.detail.message`).
pub(crate) fn failure_detail(task: &pb::Task) -> Option<String> {
    let meta = task.status.as_ref()?.message.as_ref()?.metadata.as_ref()?;
    let Kind::StructValue(failure) = meta.fields.get(FAILURE_KEY)?.kind.as_ref()? else {
        return None;
    };
    let Kind::StructValue(detail) = failure.fields.get("detail")?.kind.as_ref()? else {
        return None;
    };
    match detail.fields.get("message")?.kind.as_ref()? {
        Kind::StringValue(text) => Some(text.clone()),
        _ => None,
    }
}

pub(crate) async fn observer(bus: &MemBus, name: &str) -> <MemBus as Bus>::Consumer {
    bus.consume(
        &principal(OBSERVER),
        &stream(),
        &ConsumerName::parse(name).unwrap(),
        &SubjectPattern::parse("a2a.v1.acme.ev.>").unwrap(),
        StartPosition::FromFirst,
    )
    .await
    .unwrap()
}

/// One event observed on the event subject of the worker.
pub(crate) struct Ev {
    pub(crate) task: String,
    pub(crate) response: pb::StreamResponse,
}

pub(crate) async fn next_event(consumer: &mut <MemBus as Bus>::Consumer) -> Ev {
    let delivery = consumer.next().await.unwrap();
    let task = delivery
        .subject()
        .as_str()
        .rsplit('.')
        .next()
        .unwrap()
        .to_owned();
    let response = <pb::StreamResponse as WireMessage>::decode_wire(delivery.payload(), false)
        .expect("event payload decodes");
    delivery.ack().unwrap();
    Ev { task, response }
}

/// The task state carried by a status or task event.
pub(crate) fn event_state(ev: &Ev) -> Option<i32> {
    match &ev.response.payload {
        Some(pb::stream_response::Payload::StatusUpdate(update)) => {
            update.status.as_ref().map(|status| status.state)
        }
        Some(pb::stream_response::Payload::Task(task)) => Some(state(task)),
        _ => None,
    }
}

/// The `ApprovalRequest` carried by an INPUT_REQUIRED event (0002-065).
pub(crate) fn approval_of(ev: &Ev) -> Option<a2abus::ApprovalRequest> {
    let Some(pb::stream_response::Payload::StatusUpdate(update)) = &ev.response.payload else {
        return None;
    };
    let meta = update
        .status
        .as_ref()?
        .message
        .as_ref()?
        .metadata
        .as_ref()?;
    let Kind::StructValue(data) = meta.fields.get(APPROVAL)?.kind.as_ref()? else {
        return None;
    };
    serde_json::from_value(serde_json::to_value(data).ok()?).ok()
}

/// Durable approval record for `request_id`, read from the latest store snapshot holding it.
pub(crate) fn approval_record(store: &SharedStore, request_id: &str) -> Option<serde_json::Value> {
    store
        .records()
        .unwrap()
        .iter()
        .rev()
        .filter_map(|raw| serde_json::from_slice::<serde_json::Value>(raw).ok())
        .find_map(|value| {
            value
                .get("approvals")?
                .get("records")?
                .get(request_id)
                .cloned()
        })
}

/// Digest the Worker emitted for `request_id` (0006-051), needed to sign a decision.
pub(crate) fn approval_digest(store: &SharedStore, request_id: &str) -> String {
    approval_record(store, request_id)
        .and_then(|record| record.get("digest")?.as_str().map(str::to_owned))
        .expect("approval digest is durable")
}

/// Builds a decision the way an operator client does: the `Data` part carries the decision
/// and the part marks the APPROVAL extension (RFC-0006).
pub(crate) fn decision(
    request: &a2abus::ApprovalRequest,
    option_id: &str,
    digest: &str,
) -> a2abus::ApprovalDecision {
    a2abus::ApprovalDecision {
        request_id: request.request_id.clone(),
        task_id: request.task_id.clone(),
        option_id: option_id.to_owned(),
        nonce: request.nonce.clone(),
        decided_by: String::new(),
        decided_at: None,
        request_sha256: digest.to_owned(),
    }
}

/// Sends a decision on the task's request subject with the APPROVAL extension activated.
pub(crate) async fn decide(
    bus: &MemBus,
    task: &str,
    context: &str,
    decision: &a2abus::ApprovalDecision,
) -> Result<pb::SendMessageResponse, ReplyDecodeError> {
    let data: Value = serde_json::from_value(serde_json::to_value(decision).unwrap()).unwrap();
    let part = pb::Part {
        content: Some(pb::part::Content::Data(data)),
        metadata: Some(Struct {
            fields: HashMap::from([(
                APPROVAL.to_owned(),
                Value {
                    kind: Some(Kind::StructValue(Struct::default())),
                },
            )]),
        }),
        ..Default::default()
    };
    let body = RequestBody::SendMessage(pb::SendMessageRequest {
        tenant: TENANT.to_owned(),
        message: Some(message(context, task, vec![part])),
        ..Default::default()
    });
    let reply = call(bus, OPERATOR, Op::Send, body, vec![APPROVAL.to_owned()]).await;
    envelope::decode_reply::<pb::SendMessageResponse>(&reply)
}

/// Returns the next INPUT_REQUIRED approval event for `task`, skipping other events.
pub(crate) async fn next_approval(
    consumer: &mut <MemBus as Bus>::Consumer,
    task: &str,
) -> (Ev, a2abus::ApprovalRequest) {
    loop {
        let ev = next_event(consumer).await;
        if ev.task == task {
            if let Some(request) = approval_of(&ev) {
                return (ev, request);
            }
        }
    }
}

pub(crate) fn permission(request_id: &str, tool_kind: &str) -> Step {
    use crate::backend::{PermissionOption, PermissionRequest, RequestId};
    Step::Permission(PermissionRequest {
        request_id: RequestId::new(request_id),
        tool_kind: tool_kind.to_owned(),
        tool_title: format!("{tool_kind} file"),
        options: vec![
            PermissionOption {
                option_id: ALLOW_ONCE_ID.to_owned(),
                kind: ALLOW_ONCE_ID.to_owned(),
            },
            PermissionOption {
                option_id: REJECT_ONCE_ID.to_owned(),
                kind: REJECT_ONCE_ID.to_owned(),
            },
        ],
    })
}

pub(crate) fn completed() -> Step {
    Step::Terminal(crate::backend::Outcome::Completed)
}

/// Counts backend commands matching `pred`.
pub(crate) fn count_commands(
    mock: &MockControl,
    pred: impl Fn(&crate::mock::Command) -> bool,
) -> usize {
    mock.commands().iter().filter(|c| pred(c)).count()
}
