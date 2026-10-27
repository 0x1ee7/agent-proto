//! Connection setup: one NKey-authenticated NATS connection, its JetStream context
//! and the capability set this binding declares (RFC-0010 section 4, RFC-0011).

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use a2a_types::ids::{AgentId, TenantId};
use async_nats::Client;
use async_nats::ConnectOptions;
use async_nats::jetstream::{self, Context};
use async_nats::{Event, ServerError};
use bus_api::capabilities::{A2A_BUS_M1, Capabilities, Persistence, ProfileError};
use bus_api::error::BusError;
use bus_api::name::Principal;
use tokio::sync::broadcast;

use crate::config::NatsConfig;
use crate::denial::{CHANNEL_CAPACITY, Denials};
use crate::layout::{
    BLOB_MAX_BYTES, DEDUPE_WINDOW, KV_MAX_VALUE_BYTES, MAX_ACK_PENDING, MAX_BODY_BYTES,
};
use crate::principal::inbox_prefix;

/// Failure to bring up the binding. No partial connection is returned.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// The identity is not a valid agent identifier.
    #[error("identity is not a valid agent identifier")]
    Identity,
    /// The URL is not a `nats://` URL.
    #[error("NATS URL must use the nats:// scheme")]
    Url,
    /// The NATS server refused or was unreachable.
    #[error("NATS connection failed: {0}")]
    Connect(#[source] async_nats::ConnectError),
    /// The JetStream account query failed; JetStream is not usable.
    #[error("JetStream account query failed: {0}")]
    Account(String),
    /// The declared capabilities do not satisfy the A2A-Bus M1 profile.
    #[error(transparent)]
    Profile(#[from] ProfileError),
}

/// One connection of one identity. Cheap to clone; all clones share the connection.
#[derive(Clone)]
pub struct NatsBus {
    client: Client,
    js: Context,
    identity: String,
    tenant: TenantId,
    pub(crate) caps: Capabilities,
    ack_wait: Duration,
    /// Set by `Bus::close`; consumers opened from this connection then fail `next`.
    closed: Arc<AtomicBool>,
    /// Server permission reports (see `denial`), fed by the connection's event callback.
    denials: Denials,
    /// Retry hint carried by `Overloaded` errors (see [`Self::with_overload_retry_after`]).
    overload_retry_after: Duration,
}

impl fmt::Debug for NatsBus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NatsBus")
            .field("identity", &self.identity)
            .field("tenant", &self.tenant.as_str())
            .finish_non_exhaustive()
    }
}

impl NatsBus {
    pub(crate) fn client(&self) -> &Client {
        &self.client
    }

    pub(crate) fn js(&self) -> &Context {
        &self.js
    }

    pub(crate) fn identity(&self) -> &str {
        &self.identity
    }

    pub(crate) fn ack_wait(&self) -> Duration {
        self.ack_wait
    }

    pub(crate) fn closed(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.closed)
    }

    pub(crate) fn denials(&self) -> &Denials {
        &self.denials
    }

    /// Retry hint carried by the `Overloaded` errors this binding returns for blob quota.
    pub(crate) fn overload_retry_after(&self) -> Duration {
        self.overload_retry_after
    }

    /// Sets the retry hint carried by `Overloaded` errors (default
    /// [`crate::errors::OVERLOAD_RETRY_AFTER`]). A harness that scales time sets it to match.
    #[must_use]
    pub fn with_overload_retry_after(mut self, retry_after: Duration) -> Self {
        self.overload_retry_after = retry_after;
        self
    }

    pub(crate) fn tenant(&self) -> &TenantId {
        &self.tenant
    }

    /// Buckets and blobs of another tenant are not reachable through this connection.
    pub(crate) fn check_tenant(&self, tenant: &TenantId) -> Result<(), BusError> {
        if tenant == &self.tenant {
            Ok(())
        } else {
            Err(BusError::PermissionDenied)
        }
    }

    /// Every operation takes the caller's principal; only the connection identity
    /// may act through this connection (0010-078). Anything else is denied.
    pub(crate) fn check_who(&self, who: &Principal) -> Result<(), BusError> {
        if who.name() == Some(self.identity.as_str()) {
            Ok(())
        } else {
            Err(BusError::PermissionDenied)
        }
    }
}

/// The capabilities this binding declares. Every value is backed by the server
/// configuration of RFC-0010 section 4.5-4.7 (see `provision`).
#[must_use]
pub fn capabilities() -> Capabilities {
    Capabilities {
        headers: true,
        request_reply: true,
        no_responder_signal: true,
        durable_streams: true,
        dedupe_window: DEDUPE_WINDOW,
        replay_from_seq: true,
        ordered_per_subject: true,
        max_message_size: MAX_BODY_BYTES,
        max_pending: usize::try_from(MAX_ACK_PENDING).unwrap_or(usize::MAX),
        kv: true,
        kv_ttl: true,
        kv_snapshot_end: true,
        kv_max_value_size: KV_MAX_VALUE_BYTES,
        blob_store: true,
        blob_max_size: BLOB_MAX_BYTES,
        partition_tolerant: false,
        persistence: Persistence::Disk,
    }
}

/// Connects as `config.identity` and verifies that JetStream answers.
///
/// # Errors
/// Returns [`ConnectError`] for an invalid identity or URL, an unreachable server,
/// an unanswered JetStream account query, or capabilities below the M1 profile.
pub async fn connect(config: NatsConfig) -> Result<NatsBus, ConnectError> {
    if !config.url.starts_with("nats://") {
        return Err(ConnectError::Url);
    }
    AgentId::parse(&config.identity).map_err(|_| ConnectError::Identity)?;

    let (denials, _) = broadcast::channel(CHANNEL_CAPACITY);
    let reports = denials.clone();
    let client = ConnectOptions::with_nkey(config.seed.expose().to_owned())
        .name(config.identity.clone())
        .custom_inbox_prefix(inbox_prefix(&config.identity))
        .connection_timeout(config.request_timeout)
        .request_timeout(Some(config.request_timeout))
        .event_callback(move |event| {
            let reports = reports.clone();
            async move {
                if let Event::ServerError(ServerError::Other(text)) = event {
                    // With no listener the report is dropped: nothing awaits it then.
                    let _ = reports.send(text);
                }
            }
        })
        .connect(config.url.as_str())
        .await
        .map_err(ConnectError::Connect)?;

    let js = jetstream::new(client.clone());
    js.query_account()
        .await
        .map_err(|err| ConnectError::Account(err.to_string()))?;

    let caps = capabilities();
    A2A_BUS_M1.check(&caps)?;

    Ok(NatsBus {
        client,
        js,
        identity: config.identity,
        tenant: config.tenant,
        ack_wait: config.ack_wait,
        closed: Arc::new(AtomicBool::new(false)),
        overload_retry_after: crate::errors::OVERLOAD_RETRY_AFTER,
        denials,
        caps,
    })
}
