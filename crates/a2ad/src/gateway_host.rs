//! In-process HTTP gateway host (RFC-0008, RFC-0012 section 4.6).
//!
//! The gateway is started when its `[[agent]] kind = "gateway"` entry has no `command`.
//! Its own file (`gateway_config`) is parsed here. Peer tokens are never stored: a
//! `token = "env:<NAME>"` or `token = "file:<path>"` reference is resolved at start and only
//! its SHA-256 digest is kept by `gateway-http` (0008-066).
//!
//! Gateway file format (this host's reading; RFC-0008 gives no file schema):
//!
//! ```toml
//! config_version = 1
//! listen = "127.0.0.1:9910"            # optional, loopback default (0008-001)
//! public_base_url = "http://127.0.0.1:9910"
//! expose = ["omp"]                      # optional, default none (0008-003)
//! default_agent = "omp"                 # optional, must be exposed (0008-012)
//! trusted_peers = ["hermes"]            # optional (0008-024)
//! loopback_anonymous = true             # optional, default true (0008-022)
//! behind_proxy = false                  # optional (0008-009)
//! blocking_wait_ms = 100000             # optional (0008-052)
//! push_notifications = true             # optional (0008-013)
//!
//! [tls]                                 # required for non-loopback listeners (0008-002)
//! cert_path = "..."
//! key_path = "..."
//!
//! [[peer]]
//! name = "hermes"
//! token = "env:A2AB_PEER_HERMES"        # or "file:<owner-only path>"
//! agents = ["omp"]
//! profile = "standard"                  # or "hermes" (0008-100)
//! push_allow = []                       # optional (0008-060)
//! push_loopback_http = false            # optional
//! ```

use std::fs;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use a2a_types::ids::AgentId;
use bus_api::bus::Bus;
use gateway_http::{GatewayConfig, HttpGateway, PeerConfig, Profile, TlsConfig};
use serde::Deserialize;

use crate::config::{CONFIG_VERSION, Config, Kind};
use crate::host::{HostControl, HostError};
use crate::hosts;

/// How often the host checks the stop flag and readiness.
const POLL: Duration = Duration::from_millis(100);

/// Runs the in-process gateway until the stop flag is set (RFC-0012 section 4.6).
///
/// Connects to the broker as `gateway`, serves HTTP, and reports readiness once the listener
/// accepts TCP connections.
///
/// # Errors
/// Returns [`HostError::Config`] for an invalid gateway file or a missing gateway entry,
/// [`HostError::Bus`] when the broker connection fails, and [`HostError::Backend`] when the
/// gateway stops on its own.
pub fn run_gateway(config: &Config, control: &HostControl) -> Result<(), HostError> {
    let spec = config
        .agents()
        .iter()
        .find(|agent| agent.kind == Kind::Gateway)
        .ok_or_else(|| HostError::Config("no gateway agent is configured".to_owned()))?;
    let path = spec.gateway_config.as_ref().ok_or_else(|| {
        HostError::Config("gateway_config is required for the in-process gateway".to_owned())
    })?;
    let text = fs::read_to_string(path)
        .map_err(|err| HostError::Config(format!("cannot read gateway config: {}", err.kind())))?;
    let gateway = parse_gateway_config(&text, |name| std::env::var(name).ok())?;
    ensure_exposed_are_workers(config, &gateway)?;

    let runtime = hosts::runtime()?;
    runtime.block_on(async {
        let seed = hosts::read_seed(&crate::creds::seed_path(
            config.state_dir(),
            crate::config::GATEWAY_ID,
        ))?;
        let bus = hosts::connect_bus(
            crate::broker::nats_url(config),
            crate::config::GATEWAY_ID,
            seed,
            config.tenant().clone(),
        )
        .await?;
        serve_on(bus, gateway, control).await
    })
}

/// Parses a gateway file and builds the validated [`GatewayConfig`].
///
/// `lookup` resolves `env:` secret references (injected for tests).
///
/// # Errors
/// Returns [`HostError::Config`] for syntax errors, unknown keys, a wrong `config_version`,
/// invalid ids, unresolved secrets, or any `gateway-http` validation failure.
pub fn parse_gateway_config(
    text: &str,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<GatewayConfig, HostError> {
    let file: GatewayFile =
        toml::from_str(text).map_err(|err| HostError::Config(format!("gateway config: {err}")))?;
    if file.config_version != CONFIG_VERSION {
        return Err(HostError::Config(format!(
            "gateway config_version must be {CONFIG_VERSION}"
        )));
    }

    let defaults = GatewayConfig::default();
    let mut config = GatewayConfig {
        listen: file.listen.unwrap_or(defaults.listen),
        public_base_url: file.public_base_url,
        expose: parse_ids(&file.expose)?,
        default_agent: file.default_agent.as_deref().map(parse_id).transpose()?,
        trusted_peers: file.trusted_peers.into_iter().collect(),
        loopback_anonymous: file
            .loopback_anonymous
            .unwrap_or(defaults.loopback_anonymous),
        behind_proxy: file.behind_proxy,
        blocking_wait: file
            .blocking_wait_ms
            .map_or(defaults.blocking_wait, Duration::from_millis),
        push_notifications: file
            .push_notifications
            .unwrap_or(defaults.push_notifications),
        ..defaults
    };
    if let Some(tls) = file.tls {
        hosts::check_owner_only(&tls.cert_path)?;
        hosts::check_owner_only(&tls.key_path)?;
        config.tls = Some(TlsConfig {
            cert_path: tls.cert_path,
            key_path: tls.key_path,
        });
    }
    for peer in file.peers {
        let token = hosts::resolve_secret(&peer.token, &lookup)?;
        let agents = parse_ids(&peer.agents)?;
        let mut built = PeerConfig::new(&peer.name, &token, agents)
            .map_err(|err| HostError::Config(format!("peer {}: {err}", peer.name)))?;
        built.profile = match peer.profile {
            ProfileName::Standard => Profile::Standard,
            ProfileName::Hermes => Profile::Hermes,
        };
        built.push_allow = peer.push_allow;
        built.push_loopback_http = peer.push_loopback_http;
        config.peers.push(built);
    }
    config
        .validate()
        .map_err(|err| HostError::Config(format!("gateway config: {err}")))?;
    Ok(config)
}

/// Serves `gateway` on `bus` until the stop flag is set. Generic over the bus so tests can
/// use the in-memory bus.
pub(crate) async fn serve_on<B: Bus + Send + Sync + 'static>(
    bus: B,
    config: GatewayConfig,
    control: &HostControl,
) -> Result<(), HostError> {
    let probe = probe_address(config.listen);
    let clock = std::sync::Arc::new(a2a_core::envelope::SystemClock);
    let gateway = HttpGateway::new(bus, config, a2a_client::Config::default(), clock)
        .map_err(|err| HostError::Config(format!("gateway: {err}")))?;
    let server = tokio::spawn(gateway_http::serve(gateway));
    let mut ready = false;
    loop {
        if server.is_finished() {
            return match server.await {
                Ok(Ok(())) => Err(HostError::Backend(
                    "gateway stopped without a stop request".to_owned(),
                )),
                Ok(Err(err)) => Err(HostError::Backend(format!("gateway: {err}"))),
                Err(err) => Err(HostError::Backend(format!("gateway task: {err}"))),
            };
        }
        if control.stop_requested() {
            server.abort();
            let _ = server.await;
            return Ok(());
        }
        if !ready && tokio::net::TcpStream::connect(probe).await.is_ok() {
            control.report_ready()?;
            ready = true;
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Address used to probe readiness; unspecified listen addresses probe loopback.
fn probe_address(listen: SocketAddr) -> SocketAddr {
    let ip = match listen.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        other => other,
    };
    SocketAddr::new(ip, listen.port())
}

fn ensure_exposed_are_workers(config: &Config, gateway: &GatewayConfig) -> Result<(), HostError> {
    for id in &gateway.expose {
        let configured = config
            .agents()
            .iter()
            .any(|agent| agent.is_worker() && agent.id == id.as_str());
        if !configured {
            return Err(HostError::Config(
                "an exposed agent is not a configured worker".to_owned(),
            ));
        }
    }
    Ok(())
}

fn parse_id(text: &str) -> Result<AgentId, HostError> {
    AgentId::parse(text)
        .map_err(|_| HostError::Config("invalid agent id in gateway config".to_owned()))
}

fn parse_ids(texts: &[String]) -> Result<Vec<AgentId>, HostError> {
    texts.iter().map(|text| parse_id(text)).collect()
}

fn default_profile() -> ProfileName {
    ProfileName::Standard
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GatewayFile {
    config_version: i64,
    listen: Option<SocketAddr>,
    public_base_url: String,
    #[serde(default)]
    expose: Vec<String>,
    default_agent: Option<String>,
    #[serde(default)]
    trusted_peers: Vec<String>,
    loopback_anonymous: Option<bool>,
    #[serde(default)]
    behind_proxy: bool,
    blocking_wait_ms: Option<u64>,
    push_notifications: Option<bool>,
    tls: Option<TlsFile>,
    #[serde(default, rename = "peer")]
    peers: Vec<PeerFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TlsFile {
    cert_path: PathBuf,
    key_path: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PeerFile {
    name: String,
    token: String,
    agents: Vec<String>,
    #[serde(default = "default_profile")]
    profile: ProfileName,
    #[serde(default)]
    push_allow: Vec<String>,
    #[serde(default)]
    push_loopback_http: bool,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ProfileName {
    Standard,
    Hermes,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
config_version = 1
public_base_url = "http://127.0.0.1:9910"
expose = ["omp"]
default_agent = "omp"
"#;

    fn none(_: &str) -> Option<String> {
        None
    }

    fn with_peer(agents: &str, extra: &str) -> String {
        format!(
            "{MINIMAL}\n[[peer]]\nname = \"hermes\"\ntoken = \"env:A2AB_TEST_HERMES\"\nagents = {agents}\n{extra}"
        )
    }

    fn peer_lookup(name: &str) -> Option<String> {
        (name == "A2AB_TEST_HERMES").then(|| "peer-token-value".to_owned())
    }

    // 0008-001: the listener defaults to loopback on port 9910.
    #[test]
    fn req_0008_001_default_listener_is_loopback_9910() {
        let config = parse_gateway_config(MINIMAL, none).unwrap();
        assert_eq!(
            config.listen,
            "127.0.0.1:9910".parse::<SocketAddr>().unwrap()
        );
        assert!(config.loopback_anonymous);
        assert!(config.push_notifications);
        assert_eq!(config.blocking_wait, Duration::from_secs(100));
    }

    // 0008-002: a non-loopback listener without TLS, peers and trusted peers is refused.
    #[test]
    fn req_0008_002_non_loopback_without_preconditions_is_refused() {
        let text = MINIMAL.replace("expose", "listen = \"0.0.0.0:9910\"\nexpose");
        assert!(parse_gateway_config(&text, none).is_err());
    }

    // 0008-003: the gateway agent must never be exposed.
    #[test]
    fn req_0008_003_gateway_agent_cannot_be_exposed() {
        let text = MINIMAL.replace("\"omp\"]", "\"gateway\"]");
        assert!(parse_gateway_config(&text, none).is_err());
    }

    // 0008-004: a peer reaches only the agents it lists; the peer is built with them.
    #[test]
    fn req_0008_004_peer_agents_are_parsed() {
        let config = parse_gateway_config(&with_peer("[\"omp\"]", ""), peer_lookup).unwrap();
        assert_eq!(config.peers.len(), 1);
        assert_eq!(config.peers[0].name(), "hermes");
        assert_eq!(config.peers[0].agents.len(), 1);
        assert_eq!(config.peers[0].profile, Profile::Standard);
    }

    // 0008-100: the hermes profile is selectable per peer.
    #[test]
    fn req_0008_100_hermes_profile_is_selectable() {
        let config = parse_gateway_config(
            &with_peer("[\"omp\"]", "profile = \"hermes\"\n"),
            peer_lookup,
        )
        .unwrap();
        assert_eq!(config.peers[0].profile, Profile::Hermes);
    }

    // 0008-066: a peer token is never stored; a literal token value is refused.
    #[test]
    fn req_0008_066_literal_peer_token_is_refused() {
        let text = with_peer("[\"omp\"]", "").replace("env:A2AB_TEST_HERMES", "peer-token-value");
        let err = parse_gateway_config(&text, none).unwrap_err();
        assert!(!err.to_string().contains("peer-token-value"));
    }

    // 0008-066: an unresolved env reference refuses startup.
    #[test]
    fn req_0008_066_unset_peer_token_env_is_refused() {
        assert!(parse_gateway_config(&with_peer("[\"omp\"]", ""), none).is_err());
    }

    // 0008-024 and 0008-052: trusted peers and the blocking wait are read from the file.
    #[test]
    fn req_0008_052_blocking_wait_and_trusted_peers_are_read() {
        let text = format!(
            "{MINIMAL}\ntrusted_peers = [\"hermes\"]\nblocking_wait_ms = 45000\n\
             [[peer]]\nname = \"hermes\"\ntoken = \"env:A2AB_TEST_HERMES\"\nagents = [\"omp\"]\n"
        );
        let config = parse_gateway_config(&text, peer_lookup).unwrap();
        assert!(config.trusted_peers.contains("hermes"));
        assert_eq!(config.blocking_wait, Duration::from_millis(45_000));
    }

    #[test]
    fn req_0008_unknown_key_is_refused() {
        let text = format!("{MINIMAL}\nsurprise = true\n");
        assert!(parse_gateway_config(&text, none).is_err());
    }

    #[test]
    fn req_0008_config_version_mismatch_is_refused() {
        let text = MINIMAL.replace("config_version = 1", "config_version = 2");
        assert!(parse_gateway_config(&text, none).is_err());
    }

    #[test]
    fn probe_address_maps_unspecified_to_loopback() {
        let probe = probe_address("0.0.0.0:9910".parse().unwrap());
        assert_eq!(probe, "127.0.0.1:9910".parse::<SocketAddr>().unwrap());
    }
}
