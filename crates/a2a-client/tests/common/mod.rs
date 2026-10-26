//! Shared fixture: a bus-mem bus with a virtual clock, a fake worker, and grants.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use a2a_client::{Client, Config};
use a2a_core::envelope::{self, Clock, Inbound, ReplyOptions};
use a2a_core::headers as hdr;
use a2a_core::subject::{self, Op};
use a2a_security::envelope::Envelope;
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
pub const NOW_MS: u64 = 1_700_000_000_000;

/// Virtual clock shared by the client, the fake worker, and the assertions.
#[derive(Debug)]
pub struct VirtualClock(AtomicU64);

impl VirtualClock {
    pub fn new(now_ms: u64) -> Arc<Self> {
        Arc::new(Self(AtomicU64::new(now_ms)))
    }

    pub fn set(&self, now_ms: u64) {
        self.0.store(now_ms, Ordering::SeqCst);
    }

    pub fn advance(&self, by_ms: u64) {
        self.0.fetch_add(by_ms, Ordering::SeqCst);
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
    pub client_id: AgentId,
    pub worker_id: AgentId,
    pub worker: Principal,
}

impl Fixture {
    pub fn new() -> Self {
        let tenant = TenantId::parse(TENANT).expect("tenant");
        let client_id = AgentId::parse("cli").expect("client id");
        let worker_id = AgentId::parse("worker-a").expect("worker id");
        let cards = Bucket::new(&tenant, "cards").expect("cards bucket");
        let presence = Bucket::new(&tenant, "presence").expect("presence bucket");
        let rpc_pattern = SubjectPattern::parse("a2a.v1.t1.rpc.>").expect("rpc pattern");
        let worker = Principal::named("worker-a").expect("worker principal");
        // bus-mem authorizes serve by exact pattern equality, so grant each op's subject.
        let serve: Vec<SubjectPattern> = [Op::Send, Op::Stream, Op::Get, Op::Subscribe]
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
            client_id,
            worker_id,
            worker,
        }
    }

    /// Client sending as `cli` (or any sender whose principal is granted).
    pub fn client(&self) -> Client<Shared> {
        self.client_for(self.client_id.clone())
    }

    pub fn client_for(&self, sender: AgentId) -> Client<Shared> {
        Client::new(
            Shared(self.bus.clone()),
            self.clock.clone(),
            Config::default(),
            self.tenant.clone(),
            sender,
        )
        .expect("client")
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

    /// Receives one request and decodes it. Returns the inbound and the raw request.
    pub async fn next_inbound(&self, requests: &mut <MemBus as Bus>::Requests) -> Inbound {
        let request: Request = requests.next().await.expect("request");
        envelope::receive(&envelope::Config::default(), self.clock.as_ref(), request)
            .expect("worker accepts request")
    }
    /// Receives one request and decodes it, keeping the signed envelope and payload for
    /// verification.
    pub async fn next_signed(
        &self,
        requests: &mut <MemBus as Bus>::Requests,
    ) -> (Inbound, Envelope, Bytes) {
        let request: Request = requests.next().await.expect("request");
        let envelope = envelope_of(request.subject.as_str(), &request.headers);
        let payload = request.payload.clone();
        let inbound = envelope::receive(&envelope::Config::default(), self.clock.as_ref(), request)
            .expect("worker accepts request");
        (inbound, envelope, payload)
    }

    /// Replies to an inbound request with `message`.
    pub fn reply<M: envelope::WireMessage>(&self, inbound: Inbound, message: &M) {
        inbound
            .reply(message, &ReplyOptions::default())
            .expect("reply");
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

    /// Consumes the subject name type to keep imports used in every test binary.
    pub fn stream_name(&self) -> StreamName {
        StreamName::parse("A2AB_EV_t1").expect("stream name")
    }
}

impl Default for Fixture {
    fn default() -> Self {
        Self::new()
    }
}
/// Rebuilds the security envelope of a received request from its raw headers ([0007-034]).
fn envelope_of(subject: &str, headers: &Headers) -> Envelope {
    let value = |name: &str| headers.get(name).unwrap_or_default().to_owned();
    let block = |name: &str| headers.get(name).map(str::to_owned);
    Envelope {
        subject: subject.to_owned(),
        msg_id: value(hdr::MSG_ID),
        sender: value(hdr::SENDER),
        version: value(hdr::A2A_VERSION),
        extensions: value(hdr::A2A_EXTENSIONS),
        origin: value(hdr::ORIGIN),
        ttl_ms: value(hdr::TTL_MS),
        causation_id: value(hdr::CAUSATION_ID),
        payload_type: value(hdr::PAYLOAD_TYPE),
        after_seq: value(hdr::AFTER_SEQ),
        content_type: value(hdr::CONTENT_TYPE),
        semantic: headers
            .iter()
            .filter_map(|(name, text)| {
                name.strip_prefix(hdr::SP_PREFIX)
                    .map(|key| (key.to_owned(), text.to_owned()))
            })
            .collect(),
        kid: block(hdr::KID),
        ts: block(hdr::TS),
        nonce: block(hdr::NONCE),
        signature: block(hdr::SIG),
    }
}
