//! Peer configuration and secret handling (RFC-0009 4.6 and 4.10; RFC-0008 0008-066..069).
//!
//! [`PeerConfig`] is the serde shape the supervisor reads. [`PeerConfig::validate`]
//! turns it into [`PeerSettings`] and refuses the configurations RFC-0009 forbids
//! (0009-052): a bearer token over a non-loopback `http://` URL.

use std::fmt;
use std::time::Duration;

use a2a_types::ids::AgentId;
use serde::Deserialize;
use url::{Host, Url};

/// Default per-peer request timeout: Hermes reply timeout (300 s) plus a margin (0009-053).
pub const DEFAULT_TIMEOUT_SECS: u64 = 330;

/// Cap on the card fetch timeout (0009-041).
pub const CARD_FETCH_CAP: Duration = Duration::from_secs(30);

/// Card refresh interval (0008-071).
pub const CARD_REFRESH: Duration = Duration::from_secs(300);

/// Stream idle limit: a stream with no frame for this long is dropped (0008-074).
pub const STREAM_IDLE: Duration = Duration::from_secs(30);

/// Redacts secret text in `Debug` output.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wraps a secret value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the secret text. Only the transport may call this (0009-071).
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// Operator peer configuration as read from the supervisor config file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerConfig {
    /// Bus agent id this bridge serves. Never derived from the peer card (0008-072).
    pub agent_id: String,
    /// Peer base URL, for example `https://hermes.example/`.
    pub base_url: String,
    /// Name of the environment variable that holds the bearer token (0009-071).
    /// Absent means anonymous access.
    #[serde(default)]
    pub bearer_token_env: Option<String>,
    /// Tenant sent in `params.tenant` when set (0009-042). Not an isolation boundary (0009-080).
    #[serde(default)]
    pub tenant: Option<String>,
    /// Per-request timeout in seconds (0009-053).
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Operator opt-in for `SendStreamingMessage` (0009-043).
    #[serde(default)]
    pub streaming: bool,
}

fn default_timeout_secs() -> u64 {
    DEFAULT_TIMEOUT_SECS
}

/// Validated peer settings used by the backend.
#[derive(Clone)]
pub struct PeerSettings {
    /// Bus agent id.
    pub agent_id: AgentId,
    /// Configured peer base URL.
    pub base_url: Url,
    /// Bearer token, if configured.
    pub bearer: Option<Secret>,
    /// Tenant sent in `params.tenant`, if configured.
    pub tenant: Option<String>,
    /// Per-request timeout.
    pub timeout: Duration,
    /// Operator opt-in for streaming.
    pub streaming: bool,
}

impl fmt::Debug for PeerSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PeerSettings")
            .field("agent_id", &self.agent_id)
            .field("base_url", &self.base_url.as_str())
            .field("bearer", &self.bearer.as_ref().map(|_| "<redacted>"))
            .field("tenant", &self.tenant)
            .field("timeout", &self.timeout)
            .field("streaming", &self.streaming)
            .finish()
    }
}

/// Configuration refused at load time.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// The bus agent id is not a valid identifier.
    #[error("invalid agent id: {0}")]
    AgentId(String),
    /// The base URL is not an absolute `http` or `https` URL.
    #[error("invalid peer base URL: {0}")]
    BaseUrl(String),
    /// A bearer token would travel over a non-loopback `http://` URL (0009-052).
    #[error("bearer token refused over non-loopback http URL")]
    InsecureBearer,
    /// The environment variable named for the token is not set.
    #[error("token environment variable {0} is not set")]
    TokenUnavailable(String),
    /// The timeout must be at least one second.
    #[error("timeout_secs must be at least 1")]
    Timeout,
}

impl PeerConfig {
    /// Validates the config. `env` looks up environment variables; tests inject it.
    ///
    /// # Errors
    /// Returns [`ConfigError`] for an invalid agent id, URL, timeout, a missing token
    /// variable, or a bearer token over non-loopback cleartext (0009-052, 0008-069).
    pub fn validate(
        self,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<PeerSettings, ConfigError> {
        let agent_id =
            AgentId::parse(&self.agent_id).map_err(|e| ConfigError::AgentId(e.to_string()))?;
        let base_url = parse_base_url(&self.base_url)?;
        if self.timeout_secs == 0 {
            return Err(ConfigError::Timeout);
        }
        let bearer = match self.bearer_token_env.as_deref() {
            None => None,
            Some(name) => {
                let value = env(name)
                    .filter(|v| !v.trim().is_empty())
                    .ok_or_else(|| ConfigError::TokenUnavailable(name.to_owned()))?;
                Some(Secret::new(value.trim().to_owned()))
            }
        };
        if bearer.is_some() && !bearer_allowed(&base_url) {
            return Err(ConfigError::InsecureBearer);
        }
        Ok(PeerSettings {
            agent_id,
            base_url,
            bearer,
            tenant: self.tenant.filter(|t| !t.is_empty()),
            timeout: Duration::from_secs(self.timeout_secs),
            streaming: self.streaming,
        })
    }
}

/// Parses an absolute `http` or `https` base URL.
///
/// # Errors
/// Returns [`ConfigError::BaseUrl`] for any other input.
pub fn parse_base_url(text: &str) -> Result<Url, ConfigError> {
    let url = Url::parse(text).map_err(|e| ConfigError::BaseUrl(e.to_string()))?;
    match url.scheme() {
        "http" | "https" if url.host().is_some() => Ok(url),
        _ => Err(ConfigError::BaseUrl(
            "scheme must be http or https with a host".to_owned(),
        )),
    }
}

/// Returns whether a bearer token may be sent to `url`: `https`, or `http` on loopback (0008-069).
pub fn bearer_allowed(url: &Url) -> bool {
    url.scheme() == "https" || (url.scheme() == "http" && is_loopback(url))
}

/// Returns whether the URL host is a loopback name or address.
pub fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(addr)) => addr.is_loopback(),
        Some(Host::Ipv6(addr)) => addr.is_loopback(),
        None => false,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn config(url: &str, token_env: Option<&str>) -> PeerConfig {
        PeerConfig {
            agent_id: "hermes-peer".to_owned(),
            base_url: url.to_owned(),
            bearer_token_env: token_env.map(str::to_owned),
            tenant: None,
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            streaming: false,
        }
    }

    fn env_with_token(name: &str) -> impl Fn(&str) -> Option<String> + '_ {
        move |key| (key == name).then(|| "tok-value".to_owned())
    }

    /// 0009-052: bearer over non-loopback http is refused at start.
    #[test]
    fn req_0009_052_refuses_bearer_over_cleartext_remote() {
        let err = config("http://peer.example/", Some("PEER_TOKEN"))
            .validate(env_with_token("PEER_TOKEN"))
            .unwrap_err();
        assert_eq!(err, ConfigError::InsecureBearer);
    }

    /// 0008-069: bearer allowed over loopback http and over https.
    #[test]
    fn req_0008_069_allows_bearer_over_loopback_and_https() {
        for url in [
            "http://127.0.0.1:9911/",
            "http://localhost:9911/",
            "https://peer.example/",
        ] {
            let settings = config(url, Some("PEER_TOKEN"))
                .validate(env_with_token("PEER_TOKEN"))
                .unwrap();
            assert!(settings.bearer.is_some(), "{url}");
        }
    }

    /// 0009-070: the token is read from the named variable and never printed.
    #[test]
    fn req_0009_070_token_is_redacted_in_debug() {
        let settings = config("http://127.0.0.1:9911/", Some("PEER_TOKEN"))
            .validate(env_with_token("PEER_TOKEN"))
            .unwrap();
        let text = format!("{settings:?}");
        assert!(!text.contains("tok-value"), "{text}");
    }

    #[test]
    fn missing_token_variable_is_refused() {
        let err = config("https://peer.example/", Some("ABSENT"))
            .validate(|_| None)
            .unwrap_err();
        assert_eq!(err, ConfigError::TokenUnavailable("ABSENT".to_owned()));
    }

    #[test]
    fn non_http_scheme_is_refused() {
        assert!(matches!(
            config("ftp://peer.example/", None).validate(|_| None),
            Err(ConfigError::BaseUrl(_))
        ));
    }

    /// 0009-053: default timeout is 330 s.
    #[test]
    fn req_0009_053_default_timeout_is_330_seconds() {
        let raw: PeerConfig = serde_json::from_str(
            r#"{"agent_id":"hermes-peer","base_url":"http://127.0.0.1:9911/"}"#,
        )
        .unwrap();
        assert_eq!(raw.timeout_secs, 330);
    }

    /// 0008-072: the bus agent id comes from config, not from the card.
    #[test]
    fn req_0008_072_agent_id_is_configured() {
        let settings = config("http://127.0.0.1:9911/", None)
            .validate(|_| None)
            .unwrap();
        assert_eq!(settings.agent_id.as_str(), "hermes-peer");
    }
}
