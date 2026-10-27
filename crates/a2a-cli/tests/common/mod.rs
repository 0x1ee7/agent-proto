//! Shared fixture: a bus-mem bus, a fake worker, grants, and captured CLI streams.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::io::Cursor;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use a2a_cli::commands::Session;
use a2a_cli::config::Settings;
use a2a_cli::output::Io;
use a2a_client::signing::envelope_hook;
use a2a_client::{Client, Config};
use a2a_core::envelope::{self, Clock, Inbound, ReplyOptions};
use a2a_core::subject::{self, Op};
use a2a_security::identity::Ed25519Signer;
use a2a_types::ids::{AgentId, TenantId};
use bus_api::bus::{Bus, RequestStream};
use bus_api::capabilities::Capabilities;
use bus_api::error::BusError;
use bus_api::headers::Headers;
use bus_api::message::Entry;
use bus_api::message::{PublishAck, Reply, Request};
use bus_api::name::{
    Bucket, ConsumerName, Key, KeyPrefix, Principal, StreamName, Subject, SubjectPattern,
};
use bus_api::value::{Digest, Revision};
use bus_mem::{Grant, MemBus, MemConfig, m1_capabilities};
use bytes::Bytes;

pub const TENANT: &str = "t1";
/// 2023-11-14T22:13:20Z.
pub const NOW_MS: u64 = 1_700_000_000_000;

/// Virtual clock shared by the client, the fake worker, and the assertions.
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

/// Forwards every `Bus` call to one shared `MemBus`, so the client and the fake
/// worker can both use it (`Client` takes its bus by value).
#[derive(Debug, Clone)]
pub struct Shared(pub Arc<MemBus>);

impl Bus for Shared {
    type Requests = <MemBus as Bus>::Requests;
    type Consumer = <MemBus as Bus>::Consumer;
    type Watch = <MemBus as Bus>::Watch;

    fn capabilities(&self) -> &Capabilities {
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
        msg_id: &bus_api::name::MsgId,
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
        start: bus_api::message::StartPosition,
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

/// Bus, clock, and identities for one test.
pub struct Fixture {
    pub bus: Arc<MemBus>,
    pub clock: Arc<VirtualClock>,
    pub tenant: TenantId,
    pub operator: AgentId,
    pub worker_id: AgentId,
    pub worker: Principal,
}

impl Fixture {
    pub fn new() -> Self {
        let tenant = TenantId::parse(TENANT).expect("tenant");
        let operator = AgentId::parse("cli").expect("operator id");
        let worker_id = AgentId::parse("worker-a").expect("worker id");
        let cards = Bucket::new(&tenant, "cards").expect("cards bucket");
        let presence = Bucket::new(&tenant, "presence").expect("presence bucket");
        let rpc_pattern = SubjectPattern::parse("a2a.v1.t1.rpc.>").expect("rpc pattern");
        let worker = Principal::named("worker-a").expect("worker principal");
        // bus-mem authorizes serve by exact pattern equality, so grant each op's subject.
        let serve: Vec<SubjectPattern> = [
            Op::Send,
            Op::Stream,
            Op::Get,
            Op::List,
            Op::Cancel,
            Op::Subscribe,
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
                    principal: Principal::named("cli").expect("cli principal"),
                    tenant: tenant.clone(),
                    publish: vec![rpc_pattern.clone()],
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
        let bus = MemBus::new(config).expect("mem bus");
        Self {
            bus: Arc::new(bus),
            clock: VirtualClock::new(NOW_MS),
            tenant,
            operator,
            worker_id,
            worker,
        }
    }

    /// Client sending as the operator (`cli`).
    pub fn client(&self) -> Client<Shared> {
        Client::new(
            Shared(self.bus.clone()),
            self.clock.clone(),
            Config::default(),
            self.tenant.clone(),
            self.operator.clone(),
        )
        .expect("client")
    }

    /// Settings naming the worker as a configured agent.
    pub fn settings(&self, agents: Vec<AgentId>) -> Settings {
        Settings {
            bus_url: "nats://test.invalid:4222".to_owned(),
            seed_file: None,
            tenant: self.tenant.clone(),
            operator: self.operator.clone(),
            security_dir: PathBuf::from("security-unused"),
            trust_file: PathBuf::from("trust-unused.toml"),
            agents,
            timeout: Some(Duration::from_secs(5)),
        }
    }

    /// Session over a fresh client. When `signed`, decisions carry an operator signature from a
    /// fixed test key.
    pub fn session(&self, agents: Vec<AgentId>, signed: bool) -> Session<Shared> {
        let operator = signed.then(|| {
            let signer = Ed25519Signer::from_seed(&self.operator, &[9; 32]);
            envelope_hook(Arc::new(signer), self.clock.clone())
        });
        Session {
            client: self.client(),
            settings: self.settings(agents),
            operator,
        }
    }

    /// Subject of the rpc operation addressed to the worker.
    pub fn rpc_subject(&self, op: Op) -> Subject {
        subject::rpc(&self.tenant, &self.worker_id, op).expect("rpc subject")
    }

    /// Opens the worker's request stream for `op`.
    pub async fn serve(&self, op: Op) -> <MemBus as Bus>::Requests {
        self.bus
            .serve(&self.worker, &SubjectPattern::exact(&self.rpc_subject(op)))
            .await
            .expect("serve")
    }

    /// Receives one raw request, as the bus delivers it.
    pub async fn next_request(&self, requests: &mut <MemBus as Bus>::Requests) -> Request {
        requests.next().await.expect("request")
    }

    /// Decodes a raw request as the worker does.
    pub fn decode(&self, request: Request) -> Inbound {
        envelope::receive(&envelope::Config::default(), self.clock.as_ref(), request)
            .expect("worker accepts request")
    }

    /// Receives one request and decodes it.
    pub async fn next_inbound(&self, requests: &mut <MemBus as Bus>::Requests) -> Inbound {
        let request = self.next_request(requests).await;
        self.decode(request)
    }

    /// Replies to an inbound request with `message`.
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

/// Captured CLI streams: stdout, stderr, and the input the command reads.
pub struct Streams {
    pub out: Vec<u8>,
    pub err: Vec<u8>,
    pub input: Cursor<Vec<u8>>,
}

impl Streams {
    pub fn new(input: &str) -> Self {
        Self {
            out: Vec::new(),
            err: Vec::new(),
            input: Cursor::new(input.as_bytes().to_vec()),
        }
    }

    pub fn io(&mut self, interactive: bool, json: bool) -> Io<'_> {
        Io {
            out: &mut self.out,
            err: &mut self.err,
            input: &mut self.input,
            interactive,
            json,
            color: false,
        }
    }

    pub fn out_text(&self) -> String {
        String::from_utf8(self.out.clone()).expect("utf-8 stdout")
    }

    pub fn err_text(&self) -> String {
        String::from_utf8(self.err.clone()).expect("utf-8 stderr")
    }
}
