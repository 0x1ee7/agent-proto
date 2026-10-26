//! Configuration of an in-memory bus: capabilities, streams, buckets, grants.

use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::time::Duration;

use a2a_types::ids::TenantId;
use bus_api::capabilities::{Capabilities, M1_MIN_DEDUPE_WINDOW, M1_MIN_MESSAGE_SIZE, Persistence};
use bus_api::name::{Bucket, Principal, StreamName, SubjectPattern};
use thiserror::Error;

/// Capabilities of a complete M1 in-memory binding. Every boolean flag is
/// declared; limits sit exactly at the profile minimums where they have one.
#[must_use]
pub fn m1_capabilities() -> Capabilities {
    Capabilities {
        headers: true,
        request_reply: true,
        no_responder_signal: true,
        durable_streams: true,
        dedupe_window: M1_MIN_DEDUPE_WINDOW,
        replay_from_seq: true,
        ordered_per_subject: true,
        max_message_size: M1_MIN_MESSAGE_SIZE,
        max_pending: 1024,
        kv: true,
        kv_ttl: true,
        kv_snapshot_end: true,
        kv_max_value_size: 262_144,
        blob_store: true,
        blob_max_size: 16 * 1024 * 1024,
        partition_tolerant: true,
        persistence: Persistence::Memory,
    }
}

/// A durable stream: the subjects it captures and its retention limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamConfig {
    /// Stream name.
    pub name: StreamName,
    /// Subject patterns captured by the stream. A subject stored in the stream
    /// must match at least one of them.
    pub subjects: Vec<SubjectPattern>,
    /// Keep at most this many messages; `None` keeps everything.
    pub retention_messages: Option<NonZeroUsize>,
}

/// Permissions of one principal. Default deny: anything not granted fails with
/// `PermissionDenied`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// The principal this grant applies to. Never anonymous.
    pub principal: Principal,
    /// The only tenant whose buckets and blobs this principal may touch.
    pub tenant: TenantId,
    /// Subject patterns this principal may publish and request on.
    pub publish: Vec<SubjectPattern>,
    /// Exact subject patterns this principal may serve. `open_server` matches by
    /// equality, not coverage: a grant of `a.>` does not allow serving `a.b`.
    pub serve: Vec<SubjectPattern>,
    /// Streams this principal may consume from and append to.
    pub streams: Vec<StreamName>,
}

/// Full configuration of a [`crate::MemBus`].
#[derive(Debug, Clone, PartialEq)]
pub struct MemConfig {
    /// Seed of the fault and reorder RNG. Equal seeds give equal traces.
    pub seed: u64,
    /// Declared capabilities. The dedupe window and message limits are read from here.
    pub capabilities: Capabilities,
    /// Durable streams.
    pub streams: Vec<StreamConfig>,
    /// KV buckets.
    pub kv_buckets: Vec<Bucket>,
    /// Principal grants.
    pub grants: Vec<Grant>,
    /// How long an unsettled delivery stays invisible before redelivery.
    pub ack_wait: Duration,
    /// Retry hint carried by `Overloaded` errors.
    pub overload_retry_after: Duration,
    /// Total blob bytes one tenant may store.
    pub blob_tenant_quota: usize,
}

/// Reasons a configuration is rejected by [`crate::MemBus::new`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ConfigError {
    /// Two streams share a name.
    #[error("duplicate stream `{0}`")]
    DuplicateStream(String),
    /// A stream captures no subjects.
    #[error("stream `{0}` captures no subjects")]
    NoSubjects(String),
    /// Two buckets share a tenant and name.
    #[error("duplicate bucket `{0}`")]
    DuplicateBucket(String),
    /// Two grants name the same principal.
    #[error("duplicate grant for principal `{0}`")]
    DuplicateGrant(String),
    /// A grant names the anonymous principal.
    #[error("grant for anonymous principal")]
    AnonymousGrant,
    /// `max_pending` is zero, which would block every delivery.
    #[error("max_pending must be at least 1")]
    ZeroMaxPending,
    /// `ack_wait` is zero, so an unsettled delivery would be redelivered forever without waiting.
    #[error("ack_wait must be greater than zero")]
    ZeroAckWait,
    /// A grant names a stream that is not configured.
    #[error("grant references unknown stream `{0}`")]
    UnknownStream(String),
}

impl MemConfig {
    /// Validates the configuration before a bus is built.
    ///
    /// # Errors
    /// Returns the first [`ConfigError`] found.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.capabilities.max_pending == 0 {
            return Err(ConfigError::ZeroMaxPending);
        }
        if self.ack_wait.is_zero() {
            return Err(ConfigError::ZeroAckWait);
        }
        let mut stream_names = BTreeSet::new();
        for stream in &self.streams {
            if !stream_names.insert(stream.name.clone()) {
                return Err(ConfigError::DuplicateStream(stream.name.to_string()));
            }
            if stream.subjects.is_empty() {
                return Err(ConfigError::NoSubjects(stream.name.to_string()));
            }
        }
        let mut buckets = BTreeSet::new();
        for bucket in &self.kv_buckets {
            if !buckets.insert(bucket.clone()) {
                return Err(ConfigError::DuplicateBucket(bucket.name().to_owned()));
            }
        }
        let mut principals = BTreeSet::new();
        for grant in &self.grants {
            let Some(name) = grant.principal.name() else {
                return Err(ConfigError::AnonymousGrant);
            };
            if !principals.insert(grant.principal.clone()) {
                return Err(ConfigError::DuplicateGrant(name.to_owned()));
            }
            for stream in &grant.streams {
                if !stream_names.contains(stream) {
                    return Err(ConfigError::UnknownStream(stream.to_string()));
                }
            }
        }
        Ok(())
    }
}
