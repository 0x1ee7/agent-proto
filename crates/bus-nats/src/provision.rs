//! Supervisor-side provisioning of the streams, KV buckets, object store and the
//! archiver consumer (RFC-0010 sections 4.5-4.9). Idempotent: an object that already
//! exists is left as it is, so a repeat run never re-reserves storage.

use std::fmt::Display;
use std::future::Future;
use std::time::Duration;

use async_nats::jetstream::consumer::{AckPolicy, DeliverPolicy, ReplayPolicy, pull};
use async_nats::jetstream::object_store;
use async_nats::jetstream::stream::{self, DiscardPolicy, RetentionPolicy, StorageType};
use async_nats::jetstream::{self, kv};

use crate::connection::NatsBus;
use crate::layout::{
    ARCHIVER_CONSUMER, CARDS, CLAIMS, CLAIMS_MAX_VALUE_BYTES, DEDUPE_WINDOW, KV_MAX_VALUE_BYTES,
    PRESENCE, PRESENCE_MAX_VALUE_BYTES, artifact_bucket, audit_stream, ev_stream, kv_bucket,
    kv_stream, tenant_subject,
};

/// The only identity that provisions (RFC-0010 section 4.2).
pub const SUPERVISOR: &str = "supervisor";

const GIB: i64 = 1024 * 1024 * 1024;
const MIB: i32 = 1024 * 1024;

/// Retention of the events stream: at least max(7 d, T_task + 24 h) (0010-027).
const EV_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Retention of the audit stream (0010-025).
const AUDIT_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Claims retention: T_task + 24 h (0010-035).
const CLAIMS_MAX_AGE: Duration = Duration::from_secs(48 * 60 * 60);
/// Artifact retention: T_task + 48 h (0010-036).
const ARTIFACTS_MAX_AGE: Duration = Duration::from_secs(72 * 60 * 60);
/// Archiver redelivery schedule; its first entry is the ack wait (0010-055/056).
const ARCHIVER_BACKOFF: [Duration; 4] = [
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(120),
    Duration::from_secs(300),
];
/// Lifetime of a presence delete marker. The RFC does not fix the value (see
/// RFC feedback in the crate notes); it bounds how long a deletion stays visible.
const PRESENCE_MARKER_TTL: Duration = Duration::from_secs(60);

/// Failure to provision the deployment.
#[derive(Debug, thiserror::Error)]
pub enum ProvisionError {
    /// The connection identity is not the supervisor.
    #[error("only the supervisor identity may provision")]
    NotSupervisor,
    /// One JetStream object could not be created or updated.
    #[error("provisioning {what} failed: {detail}")]
    Step {
        /// The object being provisioned.
        what: &'static str,
        /// The server or client error text.
        detail: String,
    },
}

/// Maps a JetStream error to a [`ProvisionError::Step`] naming `what`.
fn step<E: Display>(what: &'static str) -> impl FnOnce(E) -> ProvisionError {
    move |err| ProvisionError::Step {
        what,
        detail: err.to_string(),
    }
}

/// Creates or updates every object of the tenant (RFC-0010 sections 4.5-4.9).
///
/// # Errors
/// Returns [`ProvisionError::NotSupervisor`] for any other identity, and
/// [`ProvisionError::Step`] for the first JetStream object that refuses.
pub async fn provision(bus: &NatsBus) -> Result<(), ProvisionError> {
    if bus.identity() != SUPERVISOR {
        return Err(ProvisionError::NotSupervisor);
    }
    let js = bus.js();
    let tenant = bus.tenant();

    let ev = ev_stream(tenant);
    ensure(
        js,
        &ev,
        "events stream",
        js.create_stream(event_stream_config(&ev, tenant)),
    )
    .await?;
    let audit = audit_stream(tenant);
    ensure(
        js,
        &audit,
        "audit stream",
        js.create_stream(audit_stream_config(&audit, tenant)),
    )
    .await?;

    let cards = kv_bucket(tenant, CARDS);
    ensure(
        js,
        &kv_stream(&cards),
        "cards bucket",
        js.create_key_value(kv_config(
            &cards,
            "agent cards",
            KV_MAX_VALUE_BYTES,
            None,
            Duration::ZERO,
        )),
    )
    .await?;

    let presence = kv_bucket(tenant, PRESENCE);
    ensure(
        js,
        &kv_stream(&presence),
        "presence bucket",
        js.create_key_value(kv_config(
            &presence,
            "presence leases",
            PRESENCE_MAX_VALUE_BYTES,
            Some(PRESENCE_MARKER_TTL),
            Duration::ZERO,
        )),
    )
    .await?;

    let claims = kv_bucket(tenant, CLAIMS);
    ensure(
        js,
        &kv_stream(&claims),
        "claims bucket",
        js.create_key_value(kv_config(
            &claims,
            "submission claims",
            CLAIMS_MAX_VALUE_BYTES,
            None,
            CLAIMS_MAX_AGE,
        )),
    )
    .await?;

    let artifacts = artifact_bucket(tenant);
    ensure(
        js,
        &format!("OBJ_{artifacts}"),
        "artifact store",
        js.create_object_store(object_store::Config {
            bucket: artifacts.clone(),
            description: Some("artifacts, content addressed".to_owned()),
            max_age: ARTIFACTS_MAX_AGE,
            max_bytes: 2 * GIB,
            storage: StorageType::File,
            num_replicas: 1,
            compression: false,
            placement: None,
        }),
    )
    .await?;

    // The durable archiver is idempotent by name; re-creating it is harmless.
    js.create_consumer_on_stream(archiver_config(tenant), audit_stream(tenant))
        .await
        .map_err(step("audit archiver consumer"))?;

    Ok(())
}

/// Creates the JetStream object behind `name` only when it is missing.
///
/// The server reserves a stream's byte limit again on every create, so a repeat
/// create can exceed the account's storage even with an identical config. A
/// present stream is therefore left as provisioned.
async fn ensure<T, E: Display>(
    js: &jetstream::Context,
    name: &str,
    what: &'static str,
    create: impl Future<Output = Result<T, E>>,
) -> Result<(), ProvisionError> {
    if js.get_stream(name).await.is_ok() {
        return Ok(());
    }
    create.await.map(drop).map_err(step(what))
}

/// Configuration of the events stream (0010-024).
fn event_stream_config(name: &str, tenant: &a2a_types::ids::TenantId) -> stream::Config {
    stream::Config {
        name: name.to_owned(),
        subjects: vec![format!("{}.>", tenant_subject(tenant, "ev"))],
        retention: RetentionPolicy::Limits,
        storage: StorageType::File,
        max_messages: -1,
        max_messages_per_subject: -1,
        max_consumers: -1,
        max_bytes: GIB,
        max_age: EV_MAX_AGE,
        max_message_size: MIB,
        discard: DiscardPolicy::New,
        num_replicas: 1,
        duplicate_window: DEDUPE_WINDOW,
        allow_direct: true,
        deny_delete: true,
        deny_purge: true,
        ..Default::default()
    }
}

/// Configuration of the audit stream (0010-025).
fn audit_stream_config(name: &str, tenant: &a2a_types::ids::TenantId) -> stream::Config {
    stream::Config {
        name: name.to_owned(),
        subjects: vec![format!("{}.>", tenant_subject(tenant, "audit"))],
        retention: RetentionPolicy::Limits,
        storage: StorageType::File,
        max_messages: -1,
        max_messages_per_subject: -1,
        max_consumers: -1,
        max_bytes: 512 * 1024 * 1024,
        max_age: AUDIT_MAX_AGE,
        max_message_size: 64 * 1024,
        discard: DiscardPolicy::New,
        num_replicas: 1,
        duplicate_window: Duration::from_secs(15 * 60),
        deny_delete: true,
        deny_purge: true,
        ..Default::default()
    }
}

/// Configuration of one KV bucket: `history` 1 and direct reads (0010-030..035).
/// `marker_ttl` enables per-message TTL and delete-marker expiry (0010-031).
fn kv_config(
    bucket: &str,
    description: &str,
    max_value_size: usize,
    marker_ttl: Option<Duration>,
    max_age: Duration,
) -> kv::Config {
    kv::Config {
        bucket: bucket.to_owned(),
        description: description.to_owned(),
        history: 1,
        max_age,
        max_bytes: -1,
        max_value_size: i32::try_from(max_value_size).unwrap_or(i32::MAX),
        storage: StorageType::File,
        num_replicas: 1,
        limit_markers: marker_ttl,
        ..Default::default()
    }
}

/// The durable archiver on the audit stream (0010-054..056).
fn archiver_config(tenant: &a2a_types::ids::TenantId) -> pull::Config {
    pull::Config {
        durable_name: Some(ARCHIVER_CONSUMER.to_owned()),
        description: Some("supervisor-owned audit archiver".to_owned()),
        deliver_policy: DeliverPolicy::All,
        ack_policy: AckPolicy::Explicit,
        ack_wait: Duration::from_secs(30),
        max_deliver: 10,
        backoff: ARCHIVER_BACKOFF.to_vec(),
        max_ack_pending: 256,
        replay_policy: ReplayPolicy::Instant,
        filter_subject: format!("{}.>", tenant_subject(tenant, "audit")),
        ..Default::default()
    }
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use super::*;

    fn tenant() -> a2a_types::ids::TenantId {
        a2a_types::ids::TenantId::parse("local").expect("valid tenant")
    }

    #[test]
    fn req_0010_055_ack_wait_is_first_backoff_entry_and_max_deliver_is_bounded() {
        let config = archiver_config(&tenant());
        assert_eq!(config.ack_wait, ARCHIVER_BACKOFF[0]);
        assert_eq!(config.ack_wait, Duration::from_secs(30));
        assert_eq!(config.max_deliver, 10);
        assert_eq!(config.backoff, ARCHIVER_BACKOFF.to_vec());
    }

    #[test]
    fn req_0010_054_archiver_is_durable_explicit_ack_on_audit_subjects() {
        let config = archiver_config(&tenant());
        assert_eq!(config.durable_name.as_deref(), Some(ARCHIVER_CONSUMER));
        assert_eq!(config.filter_subject, "a2a.v1.local.audit.>");
        assert_eq!(config.ack_policy, AckPolicy::Explicit);
        assert_eq!(config.replay_policy, ReplayPolicy::Instant);
        assert_eq!(config.max_ack_pending, 256);
    }

    #[test]
    fn req_0010_024_events_stream_is_limits_file_with_discard_new() {
        let config = event_stream_config("A2AB_EV_local", &tenant());
        assert_eq!(config.name, "A2AB_EV_local");
        assert_eq!(config.subjects, vec!["a2a.v1.local.ev.>".to_owned()]);
        assert_eq!(config.retention, RetentionPolicy::Limits);
        assert_eq!(config.storage, StorageType::File);
        assert_eq!(config.discard, DiscardPolicy::New);
        assert_eq!(config.max_age, EV_MAX_AGE);
        assert!(config.max_age >= Duration::from_secs(7 * 24 * 60 * 60));
        assert!(config.allow_direct);
        assert!(config.deny_delete && config.deny_purge);
        assert_eq!(config.duplicate_window, DEDUPE_WINDOW);
    }

    #[test]
    fn req_0010_025_audit_stream_has_bounded_messages_and_dedupe_window() {
        let config = audit_stream_config("A2A_AUDIT_local", &tenant());
        assert_eq!(config.name, "A2A_AUDIT_local");
        assert_eq!(config.subjects, vec!["a2a.v1.local.audit.>".to_owned()]);
        assert_eq!(config.max_message_size, 64 * 1024);
        assert_eq!(config.duplicate_window, Duration::from_secs(15 * 60));
        assert_eq!(config.discard, DiscardPolicy::New);
        assert!(config.deny_delete && config.deny_purge);
    }

    #[test]
    fn req_0010_031_presence_bucket_enables_marker_ttl_and_cards_do_not() {
        let presence = kv_config(
            "a2ab-local-presence",
            "presence",
            PRESENCE_MAX_VALUE_BYTES,
            Some(PRESENCE_MARKER_TTL),
            Duration::ZERO,
        );
        assert_eq!(presence.history, 1);
        assert_eq!(presence.limit_markers, Some(PRESENCE_MARKER_TTL));
        assert_eq!(presence.max_value_size, 4096);

        let cards = kv_config(
            "a2ab-local-cards",
            "cards",
            KV_MAX_VALUE_BYTES,
            None,
            Duration::ZERO,
        );
        assert_eq!(cards.history, 1);
        assert_eq!(cards.limit_markers, None);
        assert_eq!(cards.max_value_size, 262_144);
    }

    #[test]
    fn req_0010_035_claims_bucket_is_bounded_by_48_hours() {
        let claims = kv_config(
            "a2ab-local-claims",
            "claims",
            CLAIMS_MAX_VALUE_BYTES,
            None,
            CLAIMS_MAX_AGE,
        );
        assert_eq!(claims.max_age, Duration::from_secs(48 * 60 * 60));
        assert_eq!(claims.max_value_size, 1024);
    }

    #[test]
    fn req_0010_064_provision_failure_names_the_object() {
        let err = step::<&str>("events stream")("refused");
        assert_eq!(
            err.to_string(),
            "provisioning events stream failed: refused"
        );
    }
}
