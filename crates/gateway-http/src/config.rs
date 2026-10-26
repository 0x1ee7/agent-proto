//! Gateway configuration and start-up validation (RFC-0008 4.1, 4.9, 4.11).

use std::collections::BTreeSet;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use a2a_core::budget::Allowance;
use a2a_types::ids::AgentId;

use crate::auth::token_digest;

/// Default HTTP port of the gateway (RFC-0008 0008-001).
pub const DEFAULT_PORT: u16 = 9910;
/// Default blocking wait of non-streaming SendMessage (RFC-0008 0008-052).
pub const DEFAULT_BLOCKING_WAIT: Duration = Duration::from_secs(100);
/// Name of the origin reserved for loopback callers.
pub const LOOPBACK_ORIGIN: &str = "loopback";

/// Errors found while validating a configuration before the gateway starts.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// Listener is not loopback and no TLS is configured (0008-002).
    #[error("non-loopback listener requires TLS 1.3 configuration")]
    NonLoopbackWithoutTls,
    /// Listener is not loopback and no peer is configured (0008-002).
    #[error("non-loopback listener requires at least one peer")]
    NonLoopbackWithoutPeers,
    /// Listener is not loopback and no trusted peer is configured (0008-002).
    #[error("non-loopback listener requires at least one trusted peer")]
    NonLoopbackWithoutTrustedPeers,
    /// The gateway agent itself was listed for exposure (0008-003).
    #[error("the gateway agent must not be exposed")]
    ExposesGateway,
    /// Default agent is not in the exposed set.
    #[error("default agent is not exposed")]
    DefaultAgentNotExposed,
    /// A peer routes to an agent that is not exposed (0008-004).
    #[error("peer routes to an agent that is not exposed")]
    PeerAgentNotExposed,
    /// Two peers share a name.
    #[error("duplicate peer name")]
    DuplicatePeer,
    /// Peer name is not a valid identifier or is reserved.
    #[error("invalid peer name")]
    InvalidPeerName,
    /// Trusted peer names a peer that does not exist.
    #[error("trusted peer is not configured")]
    UnknownTrustedPeer,
    /// Proxy mode combined with loopback anonymous access (0008-009).
    #[error("loopback anonymous access must be disabled behind a reverse proxy")]
    ProxyWithLoopbackAnonymous,
    /// Blocking wait must be non-zero.
    #[error("blocking wait must be non-zero")]
    ZeroBlockingWait,
    /// Public base URL is not an absolute http or https URL.
    #[error("public base URL must be an absolute http or https URL")]
    InvalidPublicBaseUrl,
    /// A push policy host is not a plain host name.
    #[error("push allow-list entry is not a host name")]
    InvalidPushHost,
}

/// Request-rate policy: at most `limit` requests per `window`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RatePolicy {
    /// Requests allowed per window.
    pub limit: u32,
    /// Window length.
    pub window: Duration,
}

/// Behavioural profile of a peer (RFC-0008 4.11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// Standard A2A HTTP peer.
    Standard,
    /// Hermes peer: no `A2A-Extensions` required, required extensions refused at start.
    Hermes,
}

/// One HTTP peer. Only the digest of the bearer token is stored (0008-066).
#[derive(Debug, Clone)]
pub struct PeerConfig {
    pub(crate) name: String,
    pub(crate) digest: [u8; 32],
    /// Agents this peer may reach (0008-004).
    pub agents: Vec<AgentId>,
    /// Behavioural profile.
    pub profile: Profile,
    /// Budget cap applied to every message from this peer (0008-033).
    pub ingress_budget: Allowance,
    /// Host names allowed as push targets (0008-060). Empty denies all push URLs.
    pub push_allow: Vec<String>,
    /// Whether plain `http` push targets on a loopback host are allowed.
    pub push_loopback_http: bool,
}

impl PeerConfig {
    /// Creates a peer from its name and bearer token. The token itself is not retained.
    ///
    /// # Errors
    /// Returns `InvalidPeerName` when the name is not an agent identifier or is reserved.
    pub fn new(name: &str, token: &str, agents: Vec<AgentId>) -> Result<Self, ConfigError> {
        let parsed = AgentId::parse(name).map_err(|_| ConfigError::InvalidPeerName)?;
        if parsed.as_str() == LOOPBACK_ORIGIN || parsed.is_reserved() {
            return Err(ConfigError::InvalidPeerName);
        }
        Ok(Self {
            name: name.to_owned(),
            digest: token_digest(token),
            agents,
            profile: Profile::Standard,
            ingress_budget: Allowance::default(),
            push_allow: Vec::new(),
            push_loopback_http: false,
        })
    }

    /// Peer name, also the effective origin of its requests.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// TLS material for non-loopback listeners (0008-068, 4.9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsConfig {
    /// PEM certificate chain.
    pub cert_path: PathBuf,
    /// PEM private key; must be owner-only on Unix (0008-066).
    pub key_path: PathBuf,
}

/// Full gateway configuration.
#[derive(Debug, Clone)]
pub struct GatewayConfig {
    /// Listener address. Default `127.0.0.1:9910` (0008-001).
    pub listen: SocketAddr,
    /// Public base URL. Used for card URLs and the Host allow-list (0008-006, 0008-014).
    pub public_base_url: String,
    /// Agents reachable through the gateway (0008-003).
    pub expose: Vec<AgentId>,
    /// Agent answering on the root path (0008-012).
    pub default_agent: Option<AgentId>,
    /// HTTP peers.
    pub peers: Vec<PeerConfig>,
    /// Peer names allowed to call from a non-loopback socket (0008-024).
    pub trusted_peers: BTreeSet<String>,
    /// Accept unauthenticated requests from loopback sockets (0008-022).
    pub loopback_anonymous: bool,
    /// Gateway sits behind a reverse proxy (0008-009).
    pub behind_proxy: bool,
    /// TLS material; required for non-loopback listeners.
    pub tls: Option<TlsConfig>,
    /// Non-streaming SendMessage blocking wait (0008-052).
    pub blocking_wait: Duration,
    /// Advertise `capabilities.pushNotifications` (0008-013).
    pub push_notifications: bool,
    /// Rate policy per peer origin (0008-023).
    pub peer_rate: RatePolicy,
    /// Rate policy for the loopback origin (0008-022).
    pub loopback_rate: RatePolicy,
    /// Failed-authentication policy per source address (0008-023).
    pub failed_auth_rate: RatePolicy,
    /// Interval of the card and presence refresh.
    pub refresh_interval: Duration,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::new(IpAddr::from([127, 0, 0, 1]), DEFAULT_PORT),
            public_base_url: format!("http://127.0.0.1:{DEFAULT_PORT}"),
            expose: Vec::new(),
            default_agent: None,
            peers: Vec::new(),
            trusted_peers: BTreeSet::new(),
            loopback_anonymous: true,
            behind_proxy: false,
            tls: None,
            blocking_wait: DEFAULT_BLOCKING_WAIT,
            push_notifications: true,
            peer_rate: RatePolicy {
                limit: 60,
                window: Duration::from_secs(60),
            },
            loopback_rate: RatePolicy {
                limit: 600,
                window: Duration::from_secs(60),
            },
            failed_auth_rate: RatePolicy {
                limit: 30,
                window: Duration::from_secs(60),
            },
            refresh_interval: Duration::from_secs(2),
        }
    }
}

impl GatewayConfig {
    /// Validates the configuration. Called by `HttpGateway::new` before anything starts.
    ///
    /// # Errors
    /// Returns the first violated rule as a `ConfigError`.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let base = url::Url::parse(&self.public_base_url)
            .map_err(|_| ConfigError::InvalidPublicBaseUrl)?;
        if !matches!(base.scheme(), "http" | "https") || base.host_str().is_none() {
            return Err(ConfigError::InvalidPublicBaseUrl);
        }
        if self.blocking_wait.is_zero() {
            return Err(ConfigError::ZeroBlockingWait);
        }
        if self
            .expose
            .iter()
            .any(|a| a.as_str() == AgentId::gateway().as_str())
        {
            return Err(ConfigError::ExposesGateway);
        }
        if let Some(default) = &self.default_agent {
            if !self.is_exposed(default) {
                return Err(ConfigError::DefaultAgentNotExposed);
            }
        }
        let mut names = BTreeSet::new();
        for peer in &self.peers {
            if !names.insert(peer.name.as_str()) {
                return Err(ConfigError::DuplicatePeer);
            }
            if peer.agents.iter().any(|a| !self.is_exposed(a)) {
                return Err(ConfigError::PeerAgentNotExposed);
            }
            for host in &peer.push_allow {
                if host.is_empty() || host.contains(['/', ':', '@', ' ']) {
                    return Err(ConfigError::InvalidPushHost);
                }
            }
        }
        if self
            .trusted_peers
            .iter()
            .any(|name| !names.contains(name.as_str()))
        {
            return Err(ConfigError::UnknownTrustedPeer);
        }
        if self.behind_proxy && self.loopback_anonymous {
            return Err(ConfigError::ProxyWithLoopbackAnonymous);
        }
        if !self.listen.ip().is_loopback() {
            if self.tls.is_none() {
                return Err(ConfigError::NonLoopbackWithoutTls);
            }
            if self.peers.is_empty() {
                return Err(ConfigError::NonLoopbackWithoutPeers);
            }
            if self.trusted_peers.is_empty() {
                return Err(ConfigError::NonLoopbackWithoutTrustedPeers);
            }
        }
        Ok(())
    }

    /// Whether `agent` is in the exposed set.
    #[must_use]
    pub fn is_exposed(&self, agent: &AgentId) -> bool {
        self.expose.iter().any(|a| a.as_str() == agent.as_str())
    }

    /// Index of the peer named `name`.
    #[must_use]
    pub fn peer_index(&self, name: &str) -> Option<usize> {
        self.peers.iter().position(|p| p.name == name)
    }
}
