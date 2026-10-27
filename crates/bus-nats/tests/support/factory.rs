//! The NATS-backed `BusFactory` for the conformance suite (real clock), and the
//! test-side provisioning of the suite's streams and buckets.

use std::collections::BTreeMap;
use std::future::Future;
use std::time::{Duration, Instant};

use a2a_conformance::bus_contract::{BusFactory, DeliveryPrincipal};
use a2a_conformance::driver::Clock;
use a2a_conformance::fixtures::Fixtures;
use a2a_conformance::report::BindingInfo;
use a2a_types::ids::TenantId;
use async_nats::jetstream::stream::{Config as StreamConfig, StorageType};
use async_nats::jetstream::{kv, object_store};
use bus_api::bus::Bus;
use bus_api::capabilities::Capabilities;
use bus_api::control::{BusControl, Fault};
use bus_api::error::BusError;
use bus_api::headers::Headers;
use bus_api::message::{Entry, PublishAck, Reply, StartPosition};
use bus_api::name::{
    Bucket, ConsumerName, Key, KeyPrefix, MsgId, Principal, StreamName, Subject, SubjectPattern,
};
use bus_api::value::{Digest, Revision};
use bus_nats::config::{NatsConfig, Seed};
use bus_nats::kv::NatsWatch;
use bus_nats::layout::{KV_MAX_VALUE_BYTES, artifact_bucket, kv_bucket, kv_stream, object_stream};
use bus_nats::stream::NatsConsumer;
use bus_nats::{NatsBus, NatsRequests, connect};
use bytes::Bytes;

use super::server::TestServer;

/// Presence-style markers stay for a minute after their TTL expires, so the
/// re-create path (0010-031) sees the marker and not a missing bucket.
const LIMIT_MARKERS: Duration = Duration::from_secs(60);

/// Artifact store age limit; the byte limit follows the suite's blob quota.
const ARTIFACT_MAX_AGE: Duration = Duration::from_secs(72 * 60 * 60);
/// Object-store bytes beyond the blob quota: per-object chunk metadata.
const OBJECT_STORE_OVERHEAD: i64 = 64 * 1024;

/// The suite's artifact stream byte limit: the blob quota plus object-store overhead,
/// so the stream admits what the quota admits (0011-070).
fn artifact_max_bytes(fx: &Fixtures) -> i64 {
    i64::try_from(fx.blob_quota)
        .unwrap_or(i64::MAX)
        .saturating_add(OBJECT_STORE_OVERHEAD)
}

/// Resets the suite's streams, KV buckets and tenant artifact stores on `server`
/// to empty, as its supervisor. Existing objects are deleted and created again,
/// so every binding the suite creates starts from fresh state.
///
/// # Errors
/// Returns a message when the supervisor cannot connect or a resource cannot
/// be created.
pub async fn provision_suite(server: &TestServer, fx: &Fixtures) -> Result<(), String> {
    let seed = server.seed(bus_nats::provision::SUPERVISOR)?;
    let client = async_nats::ConnectOptions::with_nkey(seed)
        .name(bus_nats::provision::SUPERVISOR)
        .custom_inbox_prefix(format!("_INBOX.{}", bus_nats::provision::SUPERVISOR))
        .connect(server.url())
        .await
        .map_err(|error| error.to_string())?;
    let js = async_nats::jetstream::new(client);
    let tenants = [&fx.local, &fx.other];
    for spec in &fx.streams {
        delete_stream(&js, spec.name.as_str()).await;
    }
    for bucket in &fx.buckets {
        delete_stream(&js, &kv_stream(&kv_bucket(bucket.tenant(), bucket.name()))).await;
    }
    for tenant in tenants {
        delete_stream(&js, &object_stream(&artifact_bucket(tenant))).await;
    }
    for spec in &fx.streams {
        let config = StreamConfig {
            name: spec.name.as_str().to_owned(),
            subjects: spec
                .subjects
                .iter()
                .map(|pattern| pattern.as_str().to_owned())
                .collect(),
            max_messages: spec
                .retention
                .map_or(-1, |limit| i64::try_from(limit.get()).unwrap_or(i64::MAX)),
            storage: StorageType::File,
            ..Default::default()
        };
        js.create_stream(config)
            .await
            .map_err(|error| format!("stream {}: {error}", spec.name))?;
    }
    for bucket in &fx.buckets {
        let config = kv::Config {
            bucket: kv_bucket(bucket.tenant(), bucket.name()),
            history: 1,
            max_value_size: i32::try_from(KV_MAX_VALUE_BYTES).unwrap_or(i32::MAX),
            storage: StorageType::File,
            num_replicas: 1,
            limit_markers: Some(LIMIT_MARKERS),
            ..Default::default()
        };
        js.create_key_value(config)
            .await
            .map_err(|error| format!("bucket {}: {error}", bucket.name()))?;
    }
    for tenant in tenants {
        let artifacts = artifact_bucket(tenant);
        js.create_object_store(object_store::Config {
            bucket: artifacts.clone(),
            description: Some("artifacts, content addressed".to_owned()),
            max_age: ARTIFACT_MAX_AGE,
            max_bytes: artifact_max_bytes(fx),
            storage: StorageType::File,
            num_replicas: 1,
            compression: false,
            placement: None,
        })
        .await
        .map_err(|error| format!("artifact store {artifacts}: {error}"))?;
    }
    Ok(())
}

/// Deletes the stream `name` when present; an absent stream is already reset.
async fn delete_stream(js: &async_nats::jetstream::Context, name: &str) {
    js.delete_stream(name).await.ok();
}

/// Error for an operation the live binding does not simulate.
fn unsupported(capability: &'static str) -> BusError {
    BusError::Unsupported { capability }
}

/// The live bus under test: one NATS connection per fixture principal, routed
/// by the caller's principal. Principals without a connection (the denied and
/// anonymous ones) are refused with `PermissionDenied`.
pub struct SuiteBus {
    buses: BTreeMap<String, NatsBus>,
    caps: Capabilities,
}

impl SuiteBus {
    fn route(&self, who: &Principal) -> Result<&NatsBus, BusError> {
        who.name()
            .and_then(|name| self.buses.get(name))
            .ok_or(BusError::PermissionDenied)
    }
}

impl Bus for SuiteBus {
    type Requests = NatsRequests;
    type Consumer = NatsConsumer;
    type Watch = NatsWatch;

    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    async fn publish(
        &self,
        who: &Principal,
        subject: &Subject,
        headers: &Headers,
        payload: Bytes,
    ) -> Result<PublishAck, BusError> {
        self.route(who)?
            .publish(who, subject, headers, payload)
            .await
    }

    async fn request(
        &self,
        who: &Principal,
        subject: &Subject,
        headers: &Headers,
        payload: Bytes,
        timeout: Duration,
    ) -> Result<Reply, BusError> {
        self.route(who)?
            .request(who, subject, headers, payload, timeout)
            .await
    }

    async fn serve(
        &self,
        who: &Principal,
        pattern: &SubjectPattern,
    ) -> Result<NatsRequests, BusError> {
        self.route(who)?.serve(who, pattern).await
    }

    async fn append(
        &self,
        who: &Principal,
        stream: &StreamName,
        subject: &Subject,
        headers: &Headers,
        payload: Bytes,
        msg_id: &MsgId,
    ) -> Result<PublishAck, BusError> {
        self.route(who)?
            .append(who, stream, subject, headers, payload, msg_id)
            .await
    }

    async fn consume(
        &self,
        who: &Principal,
        stream: &StreamName,
        consumer: &ConsumerName,
        filter: &SubjectPattern,
        start: StartPosition,
    ) -> Result<NatsConsumer, BusError> {
        self.route(who)?
            .consume(who, stream, consumer, filter, start)
            .await
    }

    async fn kv_put(
        &self,
        who: &Principal,
        bucket: &Bucket,
        key: &Key,
        value: Bytes,
        ttl: Option<Duration>,
    ) -> Result<Revision, BusError> {
        self.route(who)?.kv_put(who, bucket, key, value, ttl).await
    }

    async fn kv_get(&self, who: &Principal, bucket: &Bucket, key: &Key) -> Result<Entry, BusError> {
        self.route(who)?.kv_get(who, bucket, key).await
    }

    async fn kv_create(
        &self,
        who: &Principal,
        bucket: &Bucket,
        key: &Key,
        value: Bytes,
        ttl: Option<Duration>,
    ) -> Result<Revision, BusError> {
        self.route(who)?
            .kv_create(who, bucket, key, value, ttl)
            .await
    }

    async fn kv_update(
        &self,
        who: &Principal,
        bucket: &Bucket,
        key: &Key,
        value: Bytes,
        expected: Revision,
        ttl: Option<Duration>,
    ) -> Result<Revision, BusError> {
        self.route(who)?
            .kv_update(who, bucket, key, value, expected, ttl)
            .await
    }

    async fn kv_delete(
        &self,
        who: &Principal,
        bucket: &Bucket,
        key: &Key,
        expected: Option<Revision>,
    ) -> Result<Revision, BusError> {
        self.route(who)?.kv_delete(who, bucket, key, expected).await
    }

    async fn kv_watch(
        &self,
        who: &Principal,
        bucket: &Bucket,
        prefix: &KeyPrefix,
    ) -> Result<NatsWatch, BusError> {
        self.route(who)?.kv_watch(who, bucket, prefix).await
    }

    async fn blob_put(
        &self,
        who: &Principal,
        tenant: &TenantId,
        bytes: Bytes,
    ) -> Result<Digest, BusError> {
        if !self.caps.blob_store {
            return Err(unsupported("blob_store"));
        }
        self.route(who)?.blob_put(who, tenant, bytes).await
    }

    async fn blob_get(
        &self,
        who: &Principal,
        tenant: &TenantId,
        digest: &Digest,
    ) -> Result<Bytes, BusError> {
        if !self.caps.blob_store {
            return Err(unsupported("blob_store"));
        }
        self.route(who)?.blob_get(who, tenant, digest).await
    }

    async fn close(&self) -> Result<(), BusError> {
        for bus in self.buses.values() {
            bus.close().await?;
        }
        Ok(())
    }
}

/// The control surface of the live binding. Only the seed and the real clock
/// are available; fault injection needs the in-memory binding.
pub struct SuiteControl {
    seed: u64,
    started: Instant,
}

impl BusControl for SuiteControl {
    fn seed(&self) -> u64 {
        self.seed
    }

    fn now(&self) -> Result<Duration, BusError> {
        Ok(self.started.elapsed())
    }

    fn advance(&self, _by: Duration) -> Result<(), BusError> {
        Err(unsupported("advance"))
    }

    fn set_fault(&self, _fault: Fault) -> Result<(), BusError> {
        Err(unsupported("set_fault"))
    }

    fn clear_faults(&self) -> Result<(), BusError> {
        Err(unsupported("clear_faults"))
    }

    fn fault_trace(&self) -> Result<Vec<String>, BusError> {
        Err(unsupported("fault_trace"))
    }

    fn restart_bus(&self) -> Result<(), BusError> {
        Err(unsupported("restart_bus"))
    }

    fn crash_consumer(
        &self,
        _stream: &StreamName,
        _consumer: &ConsumerName,
    ) -> Result<bool, BusError> {
        Err(unsupported("crash_consumer"))
    }

    fn corrupt_blob(&self, _tenant: &TenantId, _digest: &Digest) -> Result<bool, BusError> {
        Err(unsupported("corrupt_blob"))
    }
}

/// Builds [`SuiteBus`] values against a running server.
pub struct NatsFactory<'a> {
    server: &'a TestServer,
    handle: tokio::runtime::Handle,
    clock: Clock,
}

impl<'a> NatsFactory<'a> {
    /// A factory for `server`. `handle` must belong to a runtime that stays
    /// alive for the factory's lifetime.
    #[must_use]
    pub const fn new(server: &'a TestServer, handle: tokio::runtime::Handle, clock: Clock) -> Self {
        Self {
            server,
            handle,
            clock,
        }
    }

    /// Runs `future` to completion on the factory's runtime from a helper
    /// thread, so the calling thread may hold a runtime context (the suite's
    /// busy-poll driver does) without nesting `block_on`.
    fn block_on<T, F>(&self, future: F) -> Result<T, BusError>
    where
        F: Future<Output = Result<T, BusError>> + Send,
        T: Send,
    {
        std::thread::scope(|scope| {
            scope
                .spawn(|| self.handle.block_on(future))
                .join()
                .map_err(|_| BusError::Internal)?
        })
    }
}

impl BusFactory for NatsFactory<'_> {
    type Bus = SuiteBus;
    type Control = SuiteControl;

    fn binding(&self) -> BindingInfo {
        BindingInfo {
            name: "bus-nats",
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    /// Stream deliveries carry the publisher only on owner-token subjects (`ev`, `audit`).
    fn delivery_principal(&self) -> DeliveryPrincipal {
        DeliveryPrincipal::OwnerToken
    }

    fn clock(&self) -> Clock {
        self.clock
    }

    fn create(
        &self,
        fx: &Fixtures,
        seed: u64,
        withhold: Option<&'static str>,
    ) -> Result<(SuiteBus, SuiteControl), BusError> {
        self.block_on(async {
            provision_suite(self.server, fx)
                .await
                .map_err(|_| BusError::Internal)
        })?;
        let mut buses = BTreeMap::new();
        let mut caps = None;
        for grant in &fx.grants {
            let identity = grant
                .principal
                .name()
                .ok_or(BusError::PermissionDenied)?
                .to_owned();
            let secret = self
                .server
                .seed(&identity)
                .map_err(|_| BusError::Internal)?;
            let config = NatsConfig {
                url: self.server.url().to_owned(),
                identity: identity.clone(),
                seed: Seed::new(secret),
                tenant: grant.tenant.clone(),
                request_timeout: fx.request_timeout,
                ack_wait: fx.ack_wait,
            };
            let bus = self.block_on(async move {
                connect(config).await.map_err(|_| BusError::Unavailable {
                    reason: "CONNECT_FAILED",
                })
            })?;
            let bus = bus.with_overload_retry_after(fx.overload_retry_after);
            caps.get_or_insert_with(|| bus.capabilities().clone());
            buses.insert(identity, bus);
        }
        let base = caps.ok_or(BusError::Internal)?;
        let caps = match withhold {
            None => base,
            Some(flag) => base
                .clone()
                .with_flag(flag, false)
                .ok_or(BusError::Internal)?,
        };
        Ok((
            SuiteBus { buses, caps },
            SuiteControl {
                seed,
                started: Instant::now(),
            },
        ))
    }
}
