//! Operation dispatch: JSON-RPC methods onto bus client calls (RFC-0008 4.4, 4.6, 4.7, 4.8).

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use a2a_client::stream::EventStream;
use a2a_client::{CallOptions, Client, Outbound, Outcome, SendReply, StreamStart};
use a2a_core::budget::Allowance;
use a2a_core::errors::WireError;
use a2a_types::ids::{AgentId, ContextId, MessageId, TaskId};
use a2a_types::proto::a2a as pb;
use a2a_types::proto::a2a::TaskState;
use bus_api::bus::Bus;
use http::StatusCode;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::auth::Caller;
use crate::cards::{self, Lookup};
use crate::config::{GatewayConfig, LOOPBACK_ORIGIN};
use crate::error::{GwError, INVALID_PARAMS, METHOD_NOT_FOUND, UNAUTHORIZED};
use crate::gateway::Shared;
use crate::ingress;
use crate::jsonrpc::{self, Method, Request};

/// Default `ListTasks` page size (0008-037).
const DEFAULT_PAGE_SIZE: i32 = 50;
/// Largest `ListTasks` page size (0008-037).
const MAX_PAGE_SIZE: i32 = 100;
/// Push configurations allowed per task per origin (0008-064).
const PUSH_CONFIG_LIMIT: usize = 5;
/// Page size used when counting existing push configurations.
const PUSH_PAGE_SIZE: i32 = 100;
/// Frames buffered between a streaming job and its HTTP response.
const STREAM_BUFFER: usize = 16;

/// One streamed frame: a JSON-RPC response, or the error that ends the stream.
pub type Frame = Result<Value, GwError>;

/// Frames of an open streaming operation.
pub type Frames = mpsc::Receiver<Frame>;

/// Who a request acts for, resolved from the authenticated caller (RFC-0008 4.9).
#[derive(Debug, Clone)]
pub struct Principal {
    /// Effective origin: the peer name, or `loopback` (0008-040).
    pub origin: String,
    /// Whether the caller is an authenticated peer (not loopback-anonymous).
    pub peer: bool,
    /// Ingress budget ceiling (0008-033).
    pub ingress: Allowance,
    /// Hosts a push URL may name (0008-060).
    pub push_allow: Vec<String>,
    /// Whether `http` push URLs to loopback hosts are allowed (0008-060).
    pub push_loopback_http: bool,
}

impl Principal {
    /// Resolves the principal for an authenticated caller; `None` for an unknown peer index.
    #[must_use]
    pub fn resolve(config: &GatewayConfig, caller: Caller) -> Option<Self> {
        match caller {
            Caller::Loopback => Some(Self {
                origin: LOOPBACK_ORIGIN.to_owned(),
                peer: false,
                ingress: Allowance::default(),
                push_allow: Vec::new(),
                push_loopback_http: false,
            }),
            Caller::Peer(index) => config.peers.get(index).map(|peer| Self {
                origin: peer.name().to_owned(),
                peer: true,
                ingress: peer.ingress_budget,
                push_allow: peer.push_allow.clone(),
                push_loopback_http: peer.push_loopback_http,
            }),
        }
    }
}

/// Whether a method answers with a stream (RFC-0008 4.7).
#[must_use]
pub fn is_streaming(method: Method) -> bool {
    matches!(
        method,
        Method::SendStreamingMessage | Method::SubscribeToTask
    )
}

/// `NOT_FOUND` for agents that are unexposed, unknown, or not reachable by the caller (0008-004).
#[must_use]
pub fn agent_not_found() -> GwError {
    GwError::http(
        StatusCode::NOT_FOUND,
        METHOD_NOT_FOUND,
        "AGENT_NOT_FOUND",
        "agent not found",
    )
}

/// Runs a unary method and returns its JSON-RPC `result`.
///
/// # Errors
/// Returns the gateway error the method maps to (RFC-0008 4.6).
pub async fn unary<B: Bus + Send + Sync + 'static>(
    shared: &Shared<B>,
    agent: &AgentId,
    principal: &Principal,
    request: Request,
) -> Result<Value, GwError> {
    let Request {
        method,
        alias,
        params,
        ..
    } = request;
    let client = &shared.client;
    let opts = options(principal, None);
    match method {
        Method::SendMessage => send_message(shared, agent, principal, alias, params).await,
        Method::GetTask => {
            let request: pb::GetTaskRequest = decode(params)?;
            if request.history_length.is_some_and(|length| length < 0) {
                return Err(GwError::invalid_params(
                    "historyLength must not be negative",
                ));
            }
            let id = task_id(&request.id)?;
            let reply = client
                .get_task_with_history(agent, &id, request.history_length, &opts)
                .await
                .map_err(client_err)?;
            egress(&reply.value)
        }
        Method::ListTasks => {
            let mut request: pb::ListTasksRequest = decode(params)?;
            let size = request.page_size.unwrap_or(DEFAULT_PAGE_SIZE);
            if !(1..=MAX_PAGE_SIZE).contains(&size) {
                return Err(GwError::invalid_params(
                    "pageSize must be between 1 and 100",
                ));
            }
            request.page_size = Some(size);
            let reply = client
                .list_tasks(agent, request, &opts)
                .await
                .map_err(client_err)?;
            egress(&reply.value)
        }
        Method::CancelTask => {
            let request: pb::CancelTaskRequest = decode(params)?;
            let id = task_id(&request.id)?;
            let reply = client
                .cancel_task(agent, &id, &opts)
                .await
                .map_err(client_err)?;
            egress(&reply.value)
        }
        Method::CreatePushConfig => {
            require_push(shared)?;
            let config: pb::TaskPushNotificationConfig = decode(params)?;
            if config.authentication.is_some() || !config.token.is_empty() {
                return Err(GwError::new(
                    INVALID_PARAMS,
                    "PUSH_CREDENTIALS_NOT_SUPPORTED",
                    "push notification credentials are not supported",
                ));
            }
            check_push_url(&config.url, principal)?;
            enforce_push_limit(shared, agent, principal, &config.task_id).await?;
            let reply = client
                .push_create(agent, config, &opts)
                .await
                .map_err(client_err)?;
            egress(&reply.value)
        }
        Method::GetPushConfig => {
            require_push(shared)?;
            let request: pb::GetTaskPushNotificationConfigRequest = decode(params)?;
            let reply = client
                .push_get(agent, request, &opts)
                .await
                .map_err(client_err)?;
            egress(&reply.value)
        }
        Method::ListPushConfigs => {
            require_push(shared)?;
            let request: pb::ListTaskPushNotificationConfigsRequest = decode(params)?;
            let reply = client
                .push_list(agent, request, &opts)
                .await
                .map_err(client_err)?;
            egress(&reply.value)
        }
        Method::DeletePushConfig => {
            require_push(shared)?;
            let request: pb::DeleteTaskPushNotificationConfigRequest = decode(params)?;
            client
                .push_delete(agent, request, &opts)
                .await
                .map_err(client_err)?;
            Ok(json!({}))
        }
        Method::GetExtendedCard => extended_card(shared, agent, principal).await,
        Method::SendStreamingMessage | Method::SubscribeToTask => {
            Err(GwError::wire(WireError::UnsupportedOperation))
        }
    }
}

/// Opens a streaming method and returns its frame channel.
///
/// Validation errors return immediately, before any response bytes. Errors from the bus
/// arrive as the first frame (so the caller can still answer with a JSON-RPC error) or
/// mid-stream.
///
/// # Errors
/// Returns the gateway error of request validation.
pub fn open_stream<B: Bus + Send + Sync + 'static>(
    shared: &Arc<Shared<B>>,
    agent: AgentId,
    principal: &Principal,
    request: Request,
) -> Result<Frames, GwError> {
    let Request {
        id,
        method,
        alias,
        params,
    } = request;
    let job = match method {
        Method::SendStreamingMessage => Job::Send(Box::new(outbound(principal, alias, params)?)),
        Method::SubscribeToTask => {
            let request: pb::SubscribeToTaskRequest = decode(params)?;
            Job::Subscribe(task_id(&request.id)?)
        }
        _ => return Err(GwError::wire(WireError::UnsupportedOperation)),
    };
    let (tx, rx) = mpsc::channel(STREAM_BUFFER);
    let client = Arc::clone(&shared.client);
    let opts = options(principal, None);
    tokio::spawn(async move {
        match job {
            Job::Send(outbound) => run_send(client, agent, *outbound, opts, id, tx).await,
            Job::Subscribe(task) => run_subscribe(client, agent, task, opts, id, tx).await,
        }
    });
    Ok(rx)
}

enum Job {
    Send(Box<Outbound>),
    Subscribe(TaskId),
}

fn options(principal: &Principal, timeout: Option<Duration>) -> CallOptions {
    CallOptions {
        origin: Some(principal.origin.clone()),
        timeout,
        ..CallOptions::default()
    }
}

fn decode<T: DeserializeOwned>(params: Value) -> Result<T, GwError> {
    if let Some(root) = params.as_object() {
        ingress::check_tenant(root)?;
    }
    serde_json::from_value(params)
        .map_err(|_| GwError::invalid_params("invalid request parameters"))
}

fn egress<T: Serialize>(value: &T) -> Result<Value, GwError> {
    let mut json = serde_json::to_value(value).map_err(|_| GwError::internal())?;
    ingress::strip_sequence(&mut json);
    Ok(json)
}

fn client_err(error: a2a_client::ClientError) -> GwError {
    GwError::from_client(&error)
}

fn task_id(raw: &str) -> Result<TaskId, GwError> {
    TaskId::parse(raw).map_err(|_| GwError::invalid_params("invalid task id"))
}

fn require_push<B: Bus>(shared: &Shared<B>) -> Result<(), GwError> {
    if shared.config.push_notifications {
        Ok(())
    } else {
        Err(GwError::wire(WireError::PushNotificationNotSupported))
    }
}

/// Applies the inbound rules and builds the outbound message (0008-030..036).
fn outbound(principal: &Principal, alias: bool, mut params: Value) -> Result<Outbound, GwError> {
    ingress::prepare_send(&mut params, &principal.origin, &principal.ingress, alias)?;
    let request: pb::SendMessageRequest = decode(params)?;
    let message = request
        .message
        .ok_or_else(|| GwError::invalid_params("message is required"))?;
    let mut outbound = Outbound::new(message.parts);
    if !message.message_id.is_empty() {
        outbound.message_id = MessageId::parse(&message.message_id)
            .map_err(|_| GwError::invalid_params("invalid message id"))?;
    }
    if !message.task_id.is_empty() {
        outbound.task_id = Some(task_id(&message.task_id)?);
    }
    if !message.context_id.is_empty() {
        outbound.context_id = Some(
            ContextId::parse(&message.context_id)
                .map_err(|_| GwError::invalid_params("invalid context id"))?,
        );
    }
    outbound.metadata = message.metadata;
    outbound.return_immediately = request
        .configuration
        .is_some_and(|configuration| configuration.return_immediately);
    Ok(outbound)
}

/// `SendMessage`: blocking emulation, or returnImmediately (0008-050..052).
async fn send_message<B: Bus + Send + Sync + 'static>(
    shared: &Shared<B>,
    agent: &AgentId,
    principal: &Principal,
    alias: bool,
    params: Value,
) -> Result<Value, GwError> {
    let outbound = outbound(principal, alias, params)?;
    let client = &shared.client;
    if outbound.return_immediately {
        let opts = options(principal, None);
        let reply = client
            .send(agent, &outbound, &opts)
            .await
            .map_err(client_err)?;
        return match reply.value {
            SendReply::Task(task) => Ok(json!({"task": egress(&task)?})),
            SendReply::Message(message) => Ok(json!({"message": egress(&message)?})),
        };
    }
    let opts = options(principal, Some(shared.config.blocking_wait));
    let outcome = client
        .send_and_wait(agent, &outbound, &opts)
        .await
        .map_err(client_err)?;
    match outcome {
        Outcome::Message(message) => Ok(json!({"message": egress(&message)?})),
        Outcome::Terminal(task) | Outcome::Interrupted(task) | Outcome::Pending(task) => {
            Ok(json!({"task": egress(&task)?}))
        }
    }
}

/// `GetExtendedAgentCard`: peers on the allowlist only (0008-019).
async fn extended_card<B: Bus + Send + Sync + 'static>(
    shared: &Shared<B>,
    agent: &AgentId,
    principal: &Principal,
) -> Result<Value, GwError> {
    if !principal.peer {
        return Err(GwError::http(
            StatusCode::UNAUTHORIZED,
            UNAUTHORIZED,
            "UNAUTHORIZED",
            "authentication required",
        )
        .challenged());
    }
    let view = match shared.cards.lookup(agent.as_str()) {
        Lookup::Found(view) => view,
        Lookup::NotReady => return Err(GwError::unavailable()),
        Lookup::Missing => return Err(agent_not_found()),
    };
    if !view.extended {
        return Err(GwError::wire(WireError::UnsupportedOperation));
    }
    let opts = options(principal, None);
    let reply = shared
        .client
        .extended_card(agent, &opts)
        .await
        .map_err(client_err)?;
    let mut card = egress(&reply.value)?;
    if let Some(object) = card.as_object_mut() {
        object.remove("signatures");
        object.insert(
            "supportedInterfaces".to_owned(),
            json!([cards::interface(&shared.config.public_base_url, agent)]),
        );
    }
    Ok(card)
}

/// Checks a push URL against the scheme and host policy (0008-060).
fn check_push_url(raw: &str, principal: &Principal) -> Result<(), GwError> {
    let denied = || {
        GwError::new(
            INVALID_PARAMS,
            "PUSH_URL_NOT_PERMITTED",
            "push notification URL is not permitted",
        )
    };
    let url = url::Url::parse(raw).map_err(|_| denied())?;
    let host = url.host_str().ok_or_else(denied)?;
    let scheme_allowed = match url.scheme() {
        "https" => true,
        "http" => principal.push_loopback_http && is_loopback_host(host),
        _ => false,
    };
    if !scheme_allowed {
        return Err(denied());
    }
    if principal
        .push_allow
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(host))
    {
        Ok(())
    } else {
        Err(denied())
    }
}

fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}

/// Refuses a sixth push configuration for the task under this origin (0008-064).
async fn enforce_push_limit<B: Bus + Send + Sync + 'static>(
    shared: &Shared<B>,
    agent: &AgentId,
    principal: &Principal,
    task: &str,
) -> Result<(), GwError> {
    let opts = options(principal, None);
    let mut count = 0usize;
    let mut token = String::new();
    loop {
        let request = pb::ListTaskPushNotificationConfigsRequest {
            task_id: task.to_owned(),
            page_size: PUSH_PAGE_SIZE,
            page_token: token.clone(),
            ..Default::default()
        };
        let reply = shared
            .client
            .push_list(agent, request, &opts)
            .await
            .map_err(client_err)?;
        count += reply.value.configs.len();
        if count >= PUSH_CONFIG_LIMIT {
            return Err(GwError::new(
                INVALID_PARAMS,
                "PUSH_CONFIG_LIMIT",
                "push configuration limit reached for this task",
            ));
        }
        let next = reply.value.next_page_token;
        if next.is_empty() {
            return Ok(());
        }
        if next == token {
            return Err(GwError::internal());
        }
        token = next;
    }
}

/// Runs `SendStreamingMessage` and forwards its frames.
async fn run_send<B: Bus + Send + Sync + 'static>(
    client: Arc<Client<B>>,
    agent: AgentId,
    outbound: Outbound,
    opts: CallOptions,
    id: Value,
    tx: mpsc::Sender<Frame>,
) {
    let started = match client.stream(&agent, &outbound, &opts).await {
        Ok(started) => started,
        Err(error) => {
            let _ = tx.send(Err(client_err(error))).await;
            return;
        }
    };
    match started {
        StreamStart::Message(message) => {
            let frame =
                egress(&message).map(|value| jsonrpc::success(&id, json!({"message": value})));
            let _ = tx.send(frame).await;
        }
        StreamStart::Done(task) => {
            let frame = egress(&task).map(|value| jsonrpc::success(&id, json!({"task": value})));
            let _ = tx.send(frame).await;
        }
        StreamStart::Follow { task, mut events } => {
            let frame = egress(&task).map(|value| jsonrpc::success(&id, json!({"task": value})));
            if tx.send(frame).await.is_err() {
                return;
            }
            pump(&mut events, &id, &tx).await;
        }
    }
}

/// Runs `SubscribeToTask` and forwards its frames (0008-057).
async fn run_subscribe<B: Bus + Send + Sync + 'static>(
    client: Arc<Client<B>>,
    agent: AgentId,
    task: TaskId,
    opts: CallOptions,
    id: Value,
    tx: mpsc::Sender<Frame>,
) {
    let (snapshot, mut events) = match client.subscribe(&agent, &task, None, &opts).await {
        Ok(opened) => opened,
        Err(error) => {
            let _ = tx.send(Err(client_err(error))).await;
            return;
        }
    };
    if is_terminal(&snapshot) {
        let _ = tx
            .send(Err(GwError::wire(WireError::UnsupportedOperation)))
            .await;
        return;
    }
    let frame = egress(&snapshot).map(|value| jsonrpc::success(&id, json!({"task": value})));
    if tx.send(frame).await.is_err() {
        return;
    }
    pump(&mut events, &id, &tx).await;
}

/// Forwards events in order until the stream end, dropping replayed sequence numbers (0008-056).
async fn pump<B: Bus>(events: &mut EventStream<'_, B>, id: &Value, tx: &mpsc::Sender<Frame>) {
    let mut last: Option<u64> = None;
    loop {
        let next = tokio::select! {
            biased;
            () = tx.closed() => return,
            next = events.next() => next,
        };
        match next {
            Ok(Some(event)) => {
                if event.seq != 0 {
                    if last.is_some_and(|previous| event.seq <= previous) {
                        continue;
                    }
                    last = Some(event.seq);
                }
                let ends = event.stream_end;
                let frame = egress(&event.response).map(|value| jsonrpc::success(id, value));
                if tx.send(frame).await.is_err() || ends {
                    return;
                }
            }
            Ok(None) => return,
            Err(error) => {
                let _ = tx.send(Err(client_err(error))).await;
                return;
            }
        }
    }
}

/// Whether a snapshot task is in a terminal state.
fn is_terminal(task: &pb::Task) -> bool {
    task.status.as_ref().is_some_and(|status| {
        TaskState::from_wire(status.state).is_ok_and(|state| state.is_terminal())
    })
}
