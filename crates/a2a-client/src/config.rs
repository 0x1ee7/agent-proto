//! Client configuration, per-call options, and the signing and verification hooks.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use a2a_core::envelope::{self, Outgoing};
use a2a_security::trust::TrustStore;
use bus_api::headers::Headers;
use bytes::Bytes;

use crate::error::ClientError;

/// Requester configuration. Signing and verification are disabled in M1 (RFC-0007).
pub struct Config {
    /// Wire settings shared with the receive path (JSON acceptance, inline limit, skew).
    pub wire: envelope::Config,
    /// Extension URIs sent in `A2A-Extensions`.
    pub extensions: Vec<String>,
    /// Send JSON bodies instead of protobuf binary.
    pub json: bool,
    /// Request lifetime used when a call does not override it (1..=3600000 ms).
    pub default_ttl_ms: u64,
    /// Bus request timeout used when a call does not override it.
    pub request_timeout: Duration,
    /// Upper bound on blocking waits when the task budget sets no wall limit (RFC-0005 0005-040).
    pub max_task_wall: Duration,
    /// Pinned card trust store. `None` accepts cards without signature checks (M1 default).
    pub trust: Option<Arc<TrustStore>>,
    /// Signing hook applied to every outgoing request. `None` disables signing (M1).
    pub signing: Option<Arc<dyn SigningHook>>,
    /// Verification hook applied to every reply. `None` disables verification (M1).
    pub verify: Option<Arc<dyn VerifyHook>>,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("extensions", &self.extensions)
            .field("json", &self.json)
            .field("default_ttl_ms", &self.default_ttl_ms)
            .field("request_timeout", &self.request_timeout)
            .field("max_task_wall", &self.max_task_wall)
            .field("trust", &self.trust.is_some())
            .field("signing", &self.signing.is_some())
            .field("verify", &self.verify.is_some())
            .finish_non_exhaustive()
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            wire: envelope::Config::default(),
            extensions: Vec::new(),
            json: false,
            default_ttl_ms: 60_000,
            request_timeout: Duration::from_secs(30),
            max_task_wall: Duration::from_secs(3600),
            trust: None,
            signing: None,
            verify: None,
        }
    }
}

/// Per-call overrides.
#[derive(Debug, Clone, Default)]
pub struct CallOptions {
    /// Bus request timeout for this call.
    pub timeout: Option<Duration>,
    /// Request lifetime for this call.
    pub ttl_ms: Option<u64>,
    /// W3C `traceparent` to propagate.
    pub traceparent: Option<String>,
    /// W3C `tracestate` to propagate.
    pub tracestate: Option<String>,
    /// External origin; honored only when the requester is the gateway (RFC-0002 0002-112).
    pub origin: Option<String>,
    /// Subscription resume point (`A2AB-After-Seq`).
    pub after_seq: Option<u64>,
    /// Extra service parameters as `(key, value)`; sent as `A2AB-SP-<key>`.
    pub service_params: Vec<(String, String)>,
}

/// Signs an outgoing request before it is sent (RFC-0007, disabled in M1).
pub trait SigningHook: Send + Sync {
    /// Adds signing headers to `outgoing`.
    fn before_send(&self, outgoing: &mut Outgoing) -> Result<(), ClientError>;
}

/// Verifies a reply before its content is used (RFC-0007, disabled in M1).
pub trait VerifyHook: Send + Sync {
    /// Accepts or refuses a reply with the given headers and payload.
    fn verify(&self, headers: &Headers, payload: &Bytes) -> Result<(), ClientError>;
}
