//! Shared fixture: a bus-mem bus with a virtual clock, a fake worker, grants, and an
//! HTTP call helper that drives the gateway router without a socket.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use a2a_core::envelope::{self, Clock, Inbound, ReplyOptions};
use a2a_core::subject::{self, Op};
use a2a_types::ids::{AgentId, TenantId};
use a2a_types::proto::a2a as pb;
use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::ConnectInfo;
use axum::http::{HeaderMap, Request, StatusCode};
use bus_api::bus::{Bus, RequestStream};
use bus_api::message::Request as BusRequest;
use bus_api::name::{Bucket, Key, Principal, Subject, SubjectPattern};
use bus_mem::{Grant, MemBus, MemConfig, m1_capabilities};
use bytes::Bytes;
use gateway_http::{GatewayConfig, HttpGateway, PeerConfig, router};
use serde_json::{Value, json};
use tower::ServiceExt;

pub const TENANT: &str = "local";
pub const HOST: &str = "127.0.0.1:9910";
pub const NOW_MS: u64 = 1_700_000_000_000;
pub const LOOPBACK: [u8; 4] = [127, 0, 0, 1];
pub const REMOTE: [u8; 4] = [10, 0, 0, 5];

/// Virtual clock shared by the gateway, the fake worker, and the assertions.
#[derive(Debug)]
pub struct VirtualClock(AtomicU64);

impl VirtualClock {
    pub fn new(now_ms: u64) -> Arc<Self> {
        Arc::new(Self(AtomicU64::new(now_ms)))
    }
}

impl Clock for VirtualClock {
    fn now_ms(&self) -> Option<u64> {
        Some(self.0.load(Ordering::SeqCst))
    }
}

/// Bus, clock, and identities for one test.
pub struct Fixture {
    pub bus: Arc<MemBus>,
    pub clock: Arc<VirtualClock>,
    pub tenant: TenantId,
    pub worker_id: AgentId,
    pub worker: Principal,
}

impl Fixture {
    pub fn new() -> Self {
        let tenant = TenantId::parse(TENANT).expect("tenant");
        let worker_id = AgentId::parse("worker-a").expect("worker id");
        let worker = Principal::named("worker-a").expect("worker principal");
        let cards = Bucket::new(&tenant, "cards").expect("cards bucket");
        let presence = Bucket::new(&tenant, "presence").expect("presence bucket");
        let gateway_pattern = SubjectPattern::parse("a2a.v1.local.rpc.>").expect("pattern");
        let serve: Vec<SubjectPattern> = [
            Op::Send,
            Op::Stream,
            Op::Get,
            Op::List,
            Op::Cancel,
            Op::Subscribe,
            Op::PushCreate,
        ]
        .into_iter()
        .map(|op| {
            let subject = subject::rpc(&tenant, &worker_id, op).expect("rpc subject");
            SubjectPattern::exact(&subject)
        })
        .collect();
        let config = MemConfig {
            seed: 7,
            capabilities: m1_capabilities(),
            streams: Vec::new(),
            kv_buckets: vec![cards, presence],
            grants: vec![
                Grant {
                    principal: Principal::named("gateway").expect("gateway principal"),
                    tenant: tenant.clone(),
                    publish: vec![gateway_pattern],
                    serve: Vec::new(),
                    streams: Vec::new(),
                },
                Grant {
                    principal: worker.clone(),
                    tenant: tenant.clone(),
                    publish: Vec::new(),
                    serve,
                    streams: Vec::new(),
                },
            ],
            ack_wait: Duration::from_secs(30),
            overload_retry_after: Duration::from_secs(1),
            blob_tenant_quota: 1 << 20,
        };
        Self {
            bus: Arc::new(MemBus::new(config).expect("mem bus")),
            clock: VirtualClock::new(NOW_MS),
            tenant,
            worker_id,
            worker,
        }
    }

    /// Configuration exposing the fake worker on loopback with no peers.
    pub fn config(&self) -> GatewayConfig {
        GatewayConfig {
            expose: vec![self.worker_id.clone()],
            ..GatewayConfig::default()
        }
    }

    pub fn gateway(&self, config: GatewayConfig) -> HttpGateway<Arc<MemBus>> {
        HttpGateway::new(
            self.bus.clone(),
            config,
            a2a_client::Config::default(),
            self.clock.clone(),
        )
        .expect("gateway")
    }

    /// Gateway plus router, with the directory refreshed from the seeded KV records.
    pub async fn app(&self, config: GatewayConfig) -> (HttpGateway<Arc<MemBus>>, Router) {
        let gateway = self.gateway(config);
        gateway.refresh().await.expect("refresh");
        let app = router(gateway.clone());
        (gateway, app)
    }

    /// Seeds the worker's card and presence records.
    pub async fn seed_worker(&self) {
        self.kv_put("cards", "worker-a", &card_json("1.0.0")).await;
        self.kv_put(
            "presence",
            "worker-a",
            &presence_json("PRESENCE_STATE_READY"),
        )
        .await;
    }

    /// Writes a value into the cards or presence bucket as the worker.
    pub async fn kv_put(&self, bucket: &str, key: &str, value: &[u8]) {
        let bucket = Bucket::new(&self.tenant, bucket).expect("bucket");
        let key = Key::parse(key).expect("key");
        self.bus
            .kv_put(
                &self.worker,
                &bucket,
                &key,
                Bytes::copy_from_slice(value),
                None,
            )
            .await
            .expect("kv_put");
    }

    pub fn rpc_subject(&self, op: Op) -> Subject {
        subject::rpc(&self.tenant, &self.worker_id, op).expect("rpc subject")
    }

    /// Opens the worker's request stream for `op`. Open it before the request is sent.
    pub async fn serve(&self, op: Op) -> <MemBus as Bus>::Requests {
        self.bus
            .serve(&self.worker, &SubjectPattern::exact(&self.rpc_subject(op)))
            .await
            .expect("serve")
    }

    /// Receives one request and decodes it.
    pub async fn next_inbound(&self, requests: &mut <MemBus as Bus>::Requests) -> Inbound {
        let request: BusRequest = requests.next().await.expect("request");
        envelope::receive(&envelope::Config::default(), self.clock.as_ref(), request)
            .expect("worker accepts request")
    }

    pub fn reply<M: envelope::WireMessage>(&self, inbound: Inbound, message: &M) {
        inbound
            .reply(message, &ReplyOptions::default())
            .expect("reply");
    }
}

impl Default for Fixture {
    fn default() -> Self {
        Self::new()
    }
}

/// A peer that routes to `worker-a` with the given bearer token.
pub fn peer(name: &str, token: &str) -> PeerConfig {
    PeerConfig::new(
        name,
        token,
        vec![AgentId::parse("worker-a").expect("worker id")],
    )
    .expect("peer")
}

/// Bus card for `worker-a` at the given version.
pub fn card_json(version: &str) -> Vec<u8> {
    serde_json::to_vec(&card_value(version, None)).expect("card json")
}

/// Bus card value; `capabilities` (when given) becomes the card's capabilities object.
pub fn card_value(version: &str, capabilities: Option<Value>) -> Value {
    let mut card = json!({
        "name": "Worker A",
        "version": version,
        "skills": [{"id": "echo", "name": "Echo", "tags": ["demo"]}],
    });
    if let Some(caps) = capabilities {
        card["capabilities"] = caps;
    }
    card
}

pub fn presence_json(state: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "agentId": "worker-a", "tenant": TENANT, "instanceId": "i-1",
        "cardVersion": "1.0.0", "state": state, "activeTasks": 0
    }))
    .expect("presence json")
}

/// Task reply from the fake worker.
pub fn task(id: &str, state: pb::TaskState) -> pb::Task {
    pb::Task {
        id: id.to_owned(),
        context_id: "ctx-1".to_owned(),
        status: Some(pb::TaskStatus {
            state: state as i32,
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// `SendMessage` reply carrying one task.
pub fn send_reply(task: pb::Task) -> pb::SendMessageResponse {
    pb::SendMessageResponse {
        payload: Some(pb::send_message_response::Payload::Task(task)),
    }
}

/// A v1.0 user message with one text part.
pub fn message(text: &str) -> Value {
    json!({"messageId": "m-1", "role": "ROLE_USER", "parts": [{"text": text}]})
}

/// One HTTP request to the gateway router.
#[derive(Debug, Clone)]
pub struct Call {
    method: &'static str,
    uri: String,
    host: String,
    content_type: Option<String>,
    version: Option<String>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    source: IpAddr,
}

impl Call {
    /// POST with a JSON body and the default v1.0 headers.
    pub fn post(uri: &str, body: &Value) -> Self {
        Self::new("POST", uri).body(serde_json::to_vec(body).expect("body"))
    }

    pub fn get(uri: &str) -> Self {
        Self::new("GET", uri)
    }

    pub fn new(method: &'static str, uri: &str) -> Self {
        Self {
            method,
            uri: uri.to_owned(),
            host: HOST.to_owned(),
            content_type: Some("application/json".to_owned()),
            version: Some("1.0".to_owned()),
            headers: Vec::new(),
            body: Vec::new(),
            source: IpAddr::from(LOOPBACK),
        }
    }

    pub fn body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    pub fn host(mut self, host: &str) -> Self {
        self.host = host.to_owned();
        self
    }

    pub fn content_type(mut self, content_type: Option<&str>) -> Self {
        self.content_type = content_type.map(str::to_owned);
        self
    }

    pub fn version(mut self, version: Option<&str>) -> Self {
        self.version = version.map(str::to_owned);
        self
    }

    pub fn bearer(self, token: &str) -> Self {
        self.header("authorization", &format!("Bearer {token}"))
    }

    pub fn from(mut self, source: [u8; 4]) -> Self {
        self.source = IpAddr::from(source);
        self
    }

    fn build(self) -> Request<Body> {
        let mut builder = Request::builder()
            .method(self.method)
            .uri(&self.uri)
            .header("host", &self.host);
        if let Some(version) = &self.version {
            builder = builder.header("a2a-version", version);
        }
        if let Some(content_type) = &self.content_type {
            builder = builder.header("content-type", content_type);
        }
        for (name, value) in &self.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        let source = SocketAddr::new(self.source, 40_000);
        builder
            .extension(ConnectInfo(source))
            .body(Body::from(self.body))
            .expect("request")
    }
}

/// Response of one call, with the body collected.
#[derive(Debug)]
pub struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Reply {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("json body")
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn error_code(&self) -> i64 {
        self.json()["error"]["code"].as_i64().expect("error code")
    }

    pub fn error_reason(&self) -> String {
        self.json()["error"]["data"][0]["reason"]
            .as_str()
            .expect("error reason")
            .to_owned()
    }

    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .get(name)
            .map(|value| value.to_str().expect("ascii header").to_owned())
    }
}

/// Sends one call through the router.
pub async fn send(app: &Router, call: Call) -> Reply {
    let response = app
        .clone()
        .oneshot(call.build())
        .await
        .expect("infallible router");
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), 4 << 20)
        .await
        .expect("body");
    Reply {
        status,
        headers,
        body,
    }
}
