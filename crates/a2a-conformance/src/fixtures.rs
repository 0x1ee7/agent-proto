//! Fixture topology shared by every check: tenants, principals, streams,
//! KV buckets, grants and limits. A binding factory turns this into its own
//! configuration; the suite never depends on a binding's config type.

use std::num::NonZeroUsize;
use std::time::Duration;

use crate::driver::Clock;
use a2a_types::ids::TenantId;
use bus_api::name::{Bucket, ConsumerName, Principal, StreamName, SubjectPattern};

/// Stream capturing `suite.>`; retains everything.
pub const STREAM: &str = "SUITE";
/// Stream capturing `ret.>`; retains the last [`RETAINED`] messages.
pub const RETAINED_STREAM: &str = "SUITE_RET";
/// Consumer name used by most checks.
pub const CONSUMER: &str = "suite-c1";
/// Retention of [`RETAINED_STREAM`].
pub const RETAINED: usize = 2;
/// Unacked redelivery delay.
pub const ACK_WAIT: Duration = Duration::from_secs(5);
/// Retry hint returned with `Overloaded`.
pub const OVERLOAD_RETRY_AFTER: Duration = Duration::from_secs(1);
/// Per-tenant blob quota.
pub const BLOB_QUOTA: usize = 1024 * 1024;
/// Bucket name used in both tenants.
pub const KV_BUCKET: &str = "suite-kv";

/// A stream to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamSpec {
    /// Stream name.
    pub name: StreamName,
    /// Subjects the stream captures.
    pub subjects: Vec<SubjectPattern>,
    /// Retained message count; `None` keeps everything.
    pub retention: Option<NonZeroUsize>,
}

/// A grant to create. A principal with no grant is denied everywhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantSpec {
    /// Principal the grant belongs to.
    pub principal: Principal,
    /// Tenant the principal may act in.
    pub tenant: TenantId,
    /// Subjects the principal may publish to.
    pub publish: Vec<SubjectPattern>,
    /// Subjects the principal may serve.
    pub serve: Vec<SubjectPattern>,
    /// Streams the principal may consume from.
    pub streams: Vec<StreamName>,
}

/// The fixture topology.
#[derive(Debug, Clone, PartialEq)]
pub struct Fixtures {
    /// Tenant the suite's principals live in.
    pub local: TenantId,
    /// A second tenant for isolation checks.
    pub other: TenantId,
    /// Publishes and consumes; grant in `local`.
    pub client: Principal,
    /// Serves `rpc.echo` and `rpc.silent`; grant in `local`.
    pub server: Principal,
    /// Has no grant anywhere.
    pub denied: Principal,
    /// Grant in `other` with no publish, serve or stream rights.
    pub outsider: Principal,
    /// Streams to create.
    pub streams: Vec<StreamSpec>,
    /// Grants to create.
    pub grants: Vec<GrantSpec>,
    /// KV buckets to create: one per tenant.
    pub buckets: Vec<Bucket>,
    /// Bucket in `local`.
    pub bucket_local: Bucket,
    /// Bucket in `other`.
    pub bucket_other: Bucket,
    /// Consumer name used by most checks.
    pub consumer: ConsumerName,
    /// Stream capturing `suite.>`.
    pub stream: StreamName,
    /// Stream capturing `ret.>` with retention.
    pub retained: StreamName,
    /// Unacked redelivery delay.
    pub ack_wait: Duration,
    /// Retry hint for `Overloaded`.
    pub overload_retry_after: Duration,
    /// Per-tenant blob quota in bytes.
    pub blob_quota: usize,
    /// Deadline for suite requests, scaled to the clock.
    pub request_timeout: Duration,
    /// Clock the nominal durations were scaled to.
    pub clock: Clock,
}

impl Fixtures {
    /// Converts a nominal suite duration to the clock this run uses.
    #[must_use]
    pub fn scaled(&self, nominal: Duration) -> Duration {
        self.clock.scaled(nominal)
    }

    /// Builds the fixture topology on `clock`.
    ///
    /// # Errors
    /// Returns a message when a fixture constant fails validation (a bug in
    /// the suite, reported rather than panicking).
    pub fn new(clock: Clock) -> Result<Self, String> {
        let local = tenant("local")?;
        let other = tenant("other")?;
        let client = principal("suite-client")?;
        let server = principal("suite-server")?;
        let denied = principal("suite-denied")?;
        let outsider = principal("suite-outsider")?;
        let stream = stream_name(STREAM)?;
        let retained = stream_name(RETAINED_STREAM)?;
        let retention = NonZeroUsize::new(RETAINED).ok_or("retention must be non-zero")?;
        let bucket_local = bucket(&local)?;
        let bucket_other = bucket(&other)?;
        let consumer = ConsumerName::parse(CONSUMER).map_err(|e| e.to_string())?;

        let streams = vec![
            StreamSpec {
                name: stream.clone(),
                subjects: vec![pattern("suite.>")?],
                retention: None,
            },
            StreamSpec {
                name: retained.clone(),
                subjects: vec![pattern("ret.>")?],
                retention: Some(retention),
            },
        ];
        let grants = vec![
            GrantSpec {
                principal: client.clone(),
                tenant: local.clone(),
                publish: vec![
                    pattern("suite.>")?,
                    pattern("ret.>")?,
                    pattern("rpc.echo")?,
                    pattern("rpc.silent")?,
                    pattern("rpc.nobody")?,
                ],
                serve: Vec::new(),
                streams: vec![stream.clone(), retained.clone()],
            },
            GrantSpec {
                principal: server.clone(),
                tenant: local.clone(),
                publish: Vec::new(),
                serve: vec![pattern("rpc.echo")?, pattern("rpc.silent")?],
                streams: Vec::new(),
            },
            GrantSpec {
                principal: outsider.clone(),
                tenant: other.clone(),
                publish: Vec::new(),
                serve: Vec::new(),
                streams: Vec::new(),
            },
        ];

        Ok(Self {
            buckets: vec![bucket_local.clone(), bucket_other.clone()],
            local,
            other,
            client,
            server,
            denied,
            outsider,
            streams,
            grants,
            bucket_local,
            bucket_other,
            consumer,
            stream,
            retained,
            ack_wait: clock.scaled(ACK_WAIT),
            overload_retry_after: clock.scaled(OVERLOAD_RETRY_AFTER),
            request_timeout: clock.scaled(ACK_WAIT),
            blob_quota: BLOB_QUOTA,
            clock,
        })
    }
}

fn tenant(value: &str) -> Result<TenantId, String> {
    TenantId::parse(value).map_err(|e| format!("tenant {value}: {e}"))
}

fn principal(value: &str) -> Result<Principal, String> {
    Principal::named(value).map_err(|e| format!("principal {value}: {e}"))
}

fn stream_name(value: &str) -> Result<StreamName, String> {
    StreamName::parse(value).map_err(|e| format!("stream {value}: {e}"))
}

fn pattern(value: &str) -> Result<SubjectPattern, String> {
    SubjectPattern::parse(value).map_err(|e| format!("pattern {value}: {e}"))
}

fn bucket(tenant: &TenantId) -> Result<Bucket, String> {
    Bucket::new(tenant, KV_BUCKET).map_err(|e| format!("bucket {KV_BUCKET}: {e}"))
}
