//! Stream, bucket, object-store and subject names, and fixed limits (RFC-0010
//! sections 4.5-4.7 and 0010-015).

use std::time::Duration;

use a2a_types::ids::TenantId;

/// Body limit per message (0010-015).
pub const MAX_BODY_BYTES: usize = 524_288;
/// Headers plus body limit per message, the server `max_payload` (0010-015).
pub const MAX_WIRE_BYTES: usize = 1_048_576;
/// Largest KV value (RFC-0010 section 4.6, cards bucket limit).
pub const KV_MAX_VALUE_BYTES: usize = 262_144;
/// Largest presence value (RFC-0010 section 4.6).
pub const PRESENCE_MAX_VALUE_BYTES: usize = 4_096;
/// Largest claim value (RFC-0010 section 4.6).
pub const CLAIMS_MAX_VALUE_BYTES: usize = 1_024;
/// Largest artifact accepted by `blob_put` (RFC-0010 section 4.7).
pub const BLOB_MAX_BYTES: usize = 64 * 1024 * 1024;
/// Dedupe window of the event and audit streams (RFC-0010 section 4.5).
pub const DEDUPE_WINDOW: Duration = Duration::from_secs(3630);
/// Ack wait of bus consumers.
pub const ACK_WAIT: Duration = Duration::from_secs(30);
/// Delivery attempts of bus consumers before the server stops redelivery.
pub const MAX_DELIVER: i64 = 10;
/// Unacknowledged deliveries allowed per consumer.
pub const MAX_ACK_PENDING: i64 = 256;
/// `expires` of `allow_responses` for responders (0010-061).
pub const RESPONSE_EXPIRES: &str = "2m";
/// Durable archiver consumer of the audit stream (RFC-0010 section 4.5).
pub const ARCHIVER_CONSUMER: &str = "audit-archiver";

/// KV bucket names within a tenant (RFC-0010 section 4.6).
pub const CARDS: &str = "cards";
pub const PRESENCE: &str = "presence";
pub const CLAIMS: &str = "claims";
/// Object-store bucket name within a tenant (RFC-0010 section 4.7).
pub const ARTIFACTS: &str = "artifacts";

/// Event stream of a tenant, for example `A2AB_EV_local`.
#[must_use]
pub fn ev_stream(tenant: &TenantId) -> String {
    format!("A2AB_EV_{}", tenant.as_str())
}

/// Audit stream of a tenant, for example `A2A_AUDIT_local`.
#[must_use]
pub fn audit_stream(tenant: &TenantId) -> String {
    format!("A2A_AUDIT_{}", tenant.as_str())
}

/// NATS KV bucket of a tenant bucket, for example `a2ab-local-cards`.
#[must_use]
pub fn kv_bucket(tenant: &TenantId, name: &str) -> String {
    format!("a2ab-{}-{name}", tenant.as_str())
}

/// JetStream stream that backs a KV bucket.
#[must_use]
pub fn kv_stream(bucket: &str) -> String {
    format!("KV_{bucket}")
}

/// Object-store bucket of a tenant, for example `a2ab-local-artifacts`.
#[must_use]
pub fn artifact_bucket(tenant: &TenantId) -> String {
    format!("a2ab-{}-{ARTIFACTS}", tenant.as_str())
}

/// JetStream stream that backs an object-store bucket.
#[must_use]
pub fn object_stream(bucket: &str) -> String {
    format!("OBJ_{bucket}")
}

/// Subject of a KV key in a bucket (`$KV.<bucket>.<key>`).
#[must_use]
pub fn kv_subject(bucket: &str, key: &str) -> String {
    format!("$KV.{bucket}.{key}")
}

/// Subject of the event or audit stream prefix of a tenant (`a2a.v1.<tenant>.<kind>`).
#[must_use]
pub fn tenant_subject(tenant: &TenantId, kind: &str) -> String {
    format!("a2a.v1.{}.{kind}", tenant.as_str())
}
