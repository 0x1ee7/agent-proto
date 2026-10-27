//! Configuration file of the supervisor (RFC-0012 sec 4.1, 4.2, 4.8, 4.10; RFC-0015 4.8).
//!
//! Every table is decoded with a typed `deny_unknown_fields` schema, and every error
//! names the offending key and its table path (0012-001). All checks run before any
//! process is started (0012-004).

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use a2a_types::ids::{AgentId, TenantId};
use a2a_workspace::{NonGitPolicy, WorkspaceMode};
use serde::Deserialize;

/// The only accepted `config_version` (0012-001).
pub const CONFIG_VERSION: i64 = 1;

/// Agent ids reserved for other roles (0012-002, RFC-0010 0010-011). `bus` is the broker
/// and is configured in `[bus]`, not as an `[[agent]]`.
const RESERVED_IDS: [&str; 4] = ["bus", "tui", "cli", "supervisor"];

/// The gateway identity (RFC-0010 0010-011).
pub const GATEWAY_ID: &str = "gateway";

/// Default broker listen port (RFC-0010 0010-002).
pub const DEFAULT_BUS_PORT: u16 = 4222;

/// Default health endpoint address (RFC-0013 sec 4.8, loopback only).
pub const DEFAULT_HEALTH_LISTEN: &str = "127.0.0.1:9465";

const DEFAULT_BUS_BINARY: &str = "nats-server";
const DEFAULT_DRAIN_MS: u64 = 15_000;
const DEFAULT_GRACE_MS: u64 = 10_000;
const MAX_SHUTDOWN_MS: u64 = 3_600_000;
const DEFAULT_OPERATOR: &str = "tui";
const OPERATORS: [&str; 2] = ["tui", "cli"];
const DEFAULT_APPROVAL: &str = "ask-all";
const DEFAULT_PEER_TIMEOUT_SECS: u64 = 330;
const DEFAULT_MAX_INFLIGHT: u64 = 4;
const DEFAULT_MAX_SENDS: u64 = 30;
const DEFAULT_MAX_CALLS: u64 = 120;
const MAX_DESCRIPTION_BYTES: usize = 512;

/// Provider key names that are stripped from child environments (0012-062).
pub const PROVIDER_KEYS: [&str; 3] = ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "OMP_API_KEY"];

/// True for a provider key name: a listed key or any `*_API_KEY` name (0012-062).
#[must_use]
pub fn is_provider_key(name: &str) -> bool {
    PROVIDER_KEYS.contains(&name) || name.ends_with("_API_KEY")
}

/// A configuration error. `table` is the table path (for example `[supervisor]` or
/// `[[agent]] omp`); `detail` names the key and the rule it breaks.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("config error in {table}: {detail}")]
pub struct ConfigError {
    /// Table path of the error.
    pub table: String,
    /// Key and rule that failed.
    pub detail: String,
}

impl ConfigError {
    fn new(table: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            table: table.into(),
            detail: detail.into(),
        }
    }
}

/// Kind of an `[[agent]]` table (0012-003).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A bus agent backed by a driver profile.
    Worker,
    /// The gateway (RFC-0008) fronting the workers over A2A HTTP.
    Gateway,
    /// A process the supervisor starts but that is not a bus agent (for example Hermes).
    External,
}

/// Driver backend of a worker (RFC-0009 profiles).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// omp over ACP.
    AcpOmp,
    /// Claude Code over ACP.
    AcpClaude,
    /// A2A HTTP bridge to an external peer.
    A2aHttp,
}

impl Backend {
    /// The configuration spelling of the backend.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AcpOmp => "acp-omp",
            Self::AcpClaude => "acp-claude",
            Self::A2aHttp => "a2a-http",
        }
    }

    /// Whether the backend runs a child program in a workspace (`repo`, `workspace`,
    /// `command` allowed).
    #[must_use]
    pub const fn is_acp(self) -> bool {
        matches!(self, Self::AcpOmp | Self::AcpClaude)
    }

    /// Safe and dangerous approval modes of the backend (RFC-0006 0006-025/026,
    /// RFC-0009 0009-063/064).
    const fn modes(self) -> (&'static [&'static str], &'static [&'static str]) {
        match self {
            Self::AcpOmp => (&["ask-all", "ask-exec"], &["write", "yolo"]),
            Self::AcpClaude => (
                &["ask-all", "default", "plan"],
                &[
                    "acceptEdits",
                    "bypassPermissions",
                    "dontAsk",
                    "auto",
                    "bypass",
                ],
            ),
            Self::A2aHttp => (&["ask-all"], &[]),
        }
    }

    /// Default approval mode of the backend.
    #[must_use]
    pub const fn default_approval(self) -> &'static str {
        DEFAULT_APPROVAL
    }
}

/// Readiness probe of an agent (0012-008..010). Probes are loopback and send no credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// TCP connect to `address` (loopback).
    Tcp {
        /// `ip:port` on loopback.
        address: String,
    },
    /// HTTP GET of `url` (loopback), ready on 2xx.
    Http {
        /// Loopback `http://` URL.
        url: String,
    },
    /// HTTP GET of an agent card (loopback), ready on 2xx with a parseable card.
    Card {
        /// Loopback `http://` URL of the agent card.
        url: String,
    },
    /// The agent's bus presence is `PRESENCE_STATE_READY` (0012-009).
    BusPresence,
}

/// Readiness of an agent with its timeout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Readiness {
    /// The probe kind and target.
    pub probe: Probe,
    /// Time allowed for the probe to succeed (0012-008).
    pub timeout: Duration,
}

/// Restart policy (0012-011).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restart {
    /// Restart after a non-zero exit or crash, with backoff.
    OnFailure,
    /// Never restart.
    Never,
}

/// Workspace policy of a worker (RFC-0012 4.8; values from `a2a-workspace`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspacePolicy {
    /// Per-session or per-task lifetime.
    pub mode: WorkspaceMode,
    /// Base revision of the checkout.
    pub base_ref: String,
    /// Behaviour for a non-git source.
    pub non_git: NonGitPolicy,
}

/// Delegation settings of a worker (RFC-0015 4.8, 0015-086).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delegation {
    /// Agent ids this worker may call. Empty means no peers.
    pub peers: Vec<String>,
    /// Concurrent outbound calls, 1..=16.
    pub max_inflight: u8,
    /// Sends per minute, 1..=600.
    pub max_sends_per_min: u16,
    /// Total calls per minute, 1..=1200.
    pub max_calls_per_min: u16,
    /// Absolute path of the MCP shim, or `None` for the default beside `a2ad`.
    pub shim_path: Option<PathBuf>,
}

impl Default for Delegation {
    fn default() -> Self {
        Self {
            peers: Vec::new(),
            max_inflight: 4,
            max_sends_per_min: 30,
            max_calls_per_min: 120,
            shim_path: None,
        }
    }
}

/// Peer settings of an `a2a-http` worker (RFC-0008 0008-070). Required for that backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerSpec {
    /// Base URL of the external peer.
    pub base_url: String,
    /// Name of the environment variable that holds the bearer token (never the value).
    pub bearer_token_env: Option<String>,
    /// Name of the environment variable that holds the context secret (never the value).
    pub context_secret_env: Option<String>,
    /// Tenant the peer serves.
    pub tenant: Option<String>,
    /// Request timeout in seconds.
    pub timeout_secs: u64,
    /// Whether the peer is called over streaming.
    pub streaming: bool,
}

/// One validated `[[agent]]` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSpec {
    /// Agent id (bus identity for workers and the gateway).
    pub id: String,
    /// Kind of the process.
    pub kind: Kind,
    /// Driver backend (workers only).
    pub backend: Option<Backend>,
    /// Presence description (1..=512 bytes); the agent id when absent.
    pub description: Option<String>,
    /// Command line. For acp-* backends `command[0]` is the program; for externals and the
    /// gateway the whole line runs verbatim (no shell). Empty for a2a-http.
    pub command: Vec<String>,
    /// Gateway TOML read by the in-process gateway host when `command` is absent.
    pub gateway_config: Option<PathBuf>,
    /// Ids that must be started and ready first; `bus` is the broker.
    pub depends_on: Vec<String>,
    /// Source checkout of a workspace-backed worker (absolute).
    pub repo: Option<PathBuf>,
    /// Workspace policy of a workspace-backed worker.
    pub workspace: Option<WorkspacePolicy>,
    /// Approval mode (RFC-0006 names, workers only).
    pub approval_mode: Option<String>,
    /// True when `approval_mode` is a dangerous mode the operator acknowledged (0012-051).
    pub dangerous: bool,
    /// Peer settings (required for a2a-http, refused otherwise).
    pub peer: Option<PeerSpec>,
    /// Non-secret environment variables added to the child. `A2AB_*` and provider keys are refused.
    pub env: BTreeMap<String, String>,
    /// Environment names that must be present at start; checked by name only (4.7).
    pub env_require: Vec<String>,
    /// Provider key names this agent may receive (0012-062).
    pub env_allow: Vec<String>,
    /// Whether all environment passes through (`env_passthrough = "all"`, audited).
    pub passthrough_all: bool,
    /// Readiness probe (0012-007..010).
    pub readiness: Readiness,
    /// Restart policy (0012-011).
    pub restart: Restart,
    /// Delegation settings (workers only).
    pub delegation: Delegation,
}

impl AgentSpec {
    /// True for worker agents.
    #[must_use]
    pub fn is_worker(&self) -> bool {
        self.kind == Kind::Worker
    }
}

/// Settings of the broker (RFC-0010 4.1, 4.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusSettings {
    /// Loopback listen port.
    pub port: u16,
    /// The `nats-server` executable (absolute path, or a name resolved on `PATH`).
    pub binary: PathBuf,
    /// Lowercase hex SHA-256 of the executable (RFC-0010 0010-069).
    pub sha256: String,
}

/// The validated supervisor configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    tenant: TenantId,
    state_dir: PathBuf,
    repos_root: Option<PathBuf>,
    operator_id: String,
    drain: Duration,
    grace: Duration,
    health_listen: SocketAddr,
    bus: BusSettings,
    agents: Vec<AgentSpec>,
}

impl Config {
    /// Reads and validates the file at `path`.
    ///
    /// # Errors
    /// Returns [`ConfigError`] for an unreadable file or any rule violation.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|err| {
            ConfigError::new(
                "<document>",
                format!("cannot read {}: {err}", path.display()),
            )
        })?;
        Self::parse(&text, default_state_dir()?)
    }

    /// Validates `text`. `default_state_dir` is used when `[paths] state_dir` is absent.
    ///
    /// # Errors
    /// Returns [`ConfigError`] naming the key and table path of the first violation.
    pub fn parse(text: &str, default_state_dir: PathBuf) -> Result<Self, ConfigError> {
        let root: toml::Table =
            toml::from_str(text).map_err(|err| ConfigError::new("<document>", err.to_string()))?;
        check_root_keys(&root)?;
        match root.get("config_version").and_then(toml::Value::as_integer) {
            Some(CONFIG_VERSION) => {}
            _ => {
                return Err(ConfigError::new(
                    "<root>",
                    "config_version must be 1 (RFC-0012 0012-001)",
                ));
            }
        }
        let tenant_text = match root.get("tenant") {
            None => "local".to_owned(),
            Some(value) => value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| ConfigError::new("<root>", "tenant must be a string"))?,
        };
        let tenant = TenantId::parse(&tenant_text)
            .map_err(|_| ConfigError::new("<root>", "tenant does not match the tenant grammar"))?;

        let paths: RawPaths = decode_table(&root, "paths", "[paths]")?.unwrap_or_default();
        let state_dir = match paths.state_dir {
            Some(dir) => absolute(&dir, "[paths]", "state_dir")?,
            None => default_state_dir,
        };
        let repos_root = paths
            .repos_root
            .map(|dir| absolute(&dir, "[paths]", "repos_root"))
            .transpose()?;

        let sup: RawSupervisor =
            decode_table(&root, "supervisor", "[supervisor]")?.unwrap_or_default();
        let drain_ms = sup.drain_ms.unwrap_or(DEFAULT_DRAIN_MS);
        let grace_ms = sup.grace_ms.unwrap_or(DEFAULT_GRACE_MS);
        for (key, value) in [("drain_ms", drain_ms), ("grace_ms", grace_ms)] {
            if value > MAX_SHUTDOWN_MS {
                return Err(ConfigError::new(
                    "[supervisor]",
                    format!("{key} must be at most {MAX_SHUTDOWN_MS}"),
                ));
            }
        }
        let operator_id = sup
            .operator_id
            .unwrap_or_else(|| DEFAULT_OPERATOR.to_owned());
        if !OPERATORS.contains(&operator_id.as_str()) {
            return Err(ConfigError::new(
                "[supervisor]",
                "operator_id must be \"tui\" or \"cli\"",
            ));
        }
        let health_text = sup
            .health_listen
            .unwrap_or_else(|| DEFAULT_HEALTH_LISTEN.to_owned());
        let health_listen = loopback_socket(&health_text, "[supervisor]", "health_listen")?;

        let bus = build_bus(decode_table(&root, "bus", "[bus]")?.unwrap_or_default())?;

        let raw_agents = raw_agent_tables(&root)?;
        let mut agents = Vec::with_capacity(raw_agents.len());
        for (index, raw) in raw_agents.into_iter().enumerate() {
            agents.push(build_agent(raw, index)?);
        }
        check_ids_and_refs(&agents)?;

        Ok(Self {
            tenant,
            state_dir,
            repos_root,
            operator_id,
            drain: Duration::from_millis(drain_ms),
            grace: Duration::from_millis(grace_ms),
            health_listen,
            bus,
            agents,
        })
    }

    /// The tenant (M1: `local`).
    #[must_use]
    pub fn tenant(&self) -> &TenantId {
        &self.tenant
    }

    /// The state directory (RFC-0012 4.2).
    #[must_use]
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// Root under which repositories may be configured, if any.
    #[must_use]
    pub fn repos_root(&self) -> Option<&Path> {
        self.repos_root.as_deref()
    }

    /// The operator identity the TUI or CLI connects as (`tui` or `cli`).
    #[must_use]
    pub fn operator_id(&self) -> &str {
        &self.operator_id
    }

    /// Drain phase length (0012-015).
    #[must_use]
    pub fn drain(&self) -> Duration {
        self.drain
    }

    /// Grace period before a forced kill (0012-015).
    #[must_use]
    pub fn grace(&self) -> Duration {
        self.grace
    }

    /// Health endpoint address (RFC-0013 sec 4.8). Loopback only.
    #[must_use]
    pub fn health_listen(&self) -> SocketAddr {
        self.health_listen
    }

    /// Broker settings.
    #[must_use]
    pub fn bus(&self) -> &BusSettings {
        &self.bus
    }

    /// Every `[[agent]]` in configuration order.
    #[must_use]
    pub fn agents(&self) -> &[AgentSpec] {
        &self.agents
    }

    /// The agent with `id`, if configured.
    #[must_use]
    pub fn agent(&self, id: &str) -> Option<&AgentSpec> {
        self.agents.iter().find(|agent| agent.id == id)
    }

    /// The gateway `[[agent]]`, if configured (at most one, 0012-003).
    #[must_use]
    pub fn gateway(&self) -> Option<&AgentSpec> {
        self.agents.iter().find(|agent| agent.kind == Kind::Gateway)
    }

    /// Agents in start order: each agent comes after every agent it depends on. The broker
    /// is started before this list (0012-007).
    #[must_use]
    pub fn start_order(&self) -> Vec<&AgentSpec> {
        let mut seen = BTreeSet::new();
        let mut out = Vec::with_capacity(self.agents.len());
        for agent in &self.agents {
            self.visit(agent, &mut seen, &mut out);
        }
        out
    }

    fn visit<'a>(
        &'a self,
        agent: &'a AgentSpec,
        seen: &mut BTreeSet<String>,
        out: &mut Vec<&'a AgentSpec>,
    ) {
        if !seen.insert(agent.id.clone()) {
            return;
        }
        for dep in &agent.depends_on {
            if let Some(dep_agent) = self.agent(dep) {
                self.visit(dep_agent, seen, out);
            }
        }
        out.push(agent);
    }
}

/// Default state directory of the platform (RFC-0012 4.2).
///
/// # Errors
/// Returns [`ConfigError`] when the required environment variable is missing.
pub fn default_state_dir() -> Result<PathBuf, ConfigError> {
    if cfg!(windows) {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .ok_or_else(|| {
                ConfigError::new("[paths]", "LOCALAPPDATA is not set; set [paths] state_dir")
            })?;
        return Ok(base.join("a2abus"));
    }
    if let Some(base) = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        return Ok(base.join("a2abus"));
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| ConfigError::new("[paths]", "HOME is not set; set [paths] state_dir"))?;
    Ok(home.join(".local").join("state").join("a2abus"))
}

// ---------- raw schema -------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPaths {
    state_dir: Option<String>,
    repos_root: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSupervisor {
    drain_ms: Option<u64>,
    grace_ms: Option<u64>,
    operator_id: Option<String>,
    health_listen: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBus {
    port: Option<u16>,
    binary: Option<String>,
    sha256: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWorkspace {
    mode: Option<String>,
    base_ref: Option<String>,
    non_git: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawReadiness {
    kind: String,
    address: Option<String>,
    url: Option<String>,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDelegation {
    peers: Option<Vec<String>>,
    max_inflight: Option<u64>,
    max_sends_per_min: Option<u64>,
    max_calls_per_min: Option<u64>,
    shim_path: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPeer {
    base_url: String,
    bearer_token_env: Option<String>,
    context_secret_env: Option<String>,
    tenant: Option<String>,
    timeout_secs: Option<u64>,
    streaming: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAgent {
    id: String,
    kind: String,
    backend: Option<String>,
    description: Option<String>,
    command: Option<Vec<String>>,
    gateway_config: Option<String>,
    depends_on: Option<Vec<String>>,
    repo: Option<String>,
    workspace: Option<RawWorkspace>,
    approval_mode: Option<String>,
    acknowledge_dangerous: Option<String>,
    peer: Option<RawPeer>,
    env: Option<BTreeMap<String, String>>,
    env_require: Option<Vec<String>>,
    env_allow: Option<Vec<String>>,
    env_passthrough: Option<String>,
    readiness: Option<RawReadiness>,
    restart: Option<String>,
    delegation: Option<RawDelegation>,
}

// ---------- decoding helpers --------------------------------------------------------

fn check_root_keys(root: &toml::Table) -> Result<(), ConfigError> {
    const ALLOWED: [&str; 6] = [
        "config_version",
        "tenant",
        "paths",
        "supervisor",
        "bus",
        "agent",
    ];
    for key in root.keys() {
        if !ALLOWED.contains(&key.as_str()) {
            return Err(ConfigError::new("<root>", format!("unknown key `{key}`")));
        }
    }
    Ok(())
}

fn decode_table<T: serde::de::DeserializeOwned>(
    root: &toml::Table,
    key: &str,
    table: &str,
) -> Result<Option<T>, ConfigError> {
    match root.get(key) {
        None => Ok(None),
        Some(value) => value
            .clone()
            .try_into::<T>()
            .map(Some)
            .map_err(|err| ConfigError::new(table, err.to_string())),
    }
}

fn raw_agent_tables(root: &toml::Table) -> Result<Vec<RawAgent>, ConfigError> {
    let Some(value) = root.get("agent") else {
        return Ok(Vec::new());
    };
    let items = value
        .as_array()
        .ok_or_else(|| ConfigError::new("[[agent]]", "agent must be an array of tables"))?;
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            item.clone()
                .try_into::<RawAgent>()
                .map_err(|err| ConfigError::new(format!("[[agent]] #{index}"), err.to_string()))
        })
        .collect()
}

fn absolute(text: &str, table: &str, key: &str) -> Result<PathBuf, ConfigError> {
    let path = PathBuf::from(text);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(ConfigError::new(
            table,
            format!("{key} must be an absolute path"),
        ))
    }
}

fn loopback_socket(text: &str, table: &str, key: &str) -> Result<SocketAddr, ConfigError> {
    let addr: SocketAddr = text
        .parse()
        .map_err(|_| ConfigError::new(table, format!("{key} must be an ip:port address")))?;
    if addr.ip().is_loopback() {
        Ok(addr)
    } else {
        Err(ConfigError::new(
            table,
            format!("{key} must be a loopback address (RFC-0013 4.8)"),
        ))
    }
}

/// True for a loopback URL with the `http` scheme and a literal loopback host.
fn is_loopback_http_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let host = rest.split(['/', ':']).next().unwrap_or_default();
    host == "127.0.0.1" || host == "localhost"
}

fn build_bus(raw: RawBus) -> Result<BusSettings, ConfigError> {
    let port = raw.port.unwrap_or(DEFAULT_BUS_PORT);
    if port == 0 {
        return Err(ConfigError::new("[bus]", "port must be non-zero"));
    }
    let binary = PathBuf::from(raw.binary.unwrap_or_else(|| DEFAULT_BUS_BINARY.to_owned()));
    let sha256 = raw
        .sha256
        .ok_or_else(|| ConfigError::new("[bus]", "sha256 is required (RFC-0010 0010-069)"))?
        .to_ascii_lowercase();
    if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ConfigError::new(
            "[bus]",
            "sha256 must be 64 hexadecimal characters",
        ));
    }
    Ok(BusSettings {
        port,
        binary,
        sha256,
    })
}

fn check_agent_id(raw: &str, table: &str) -> Result<String, ConfigError> {
    AgentId::parse(raw)
        .map(|id| id.as_str().to_owned())
        .map_err(|_| {
            ConfigError::new(
                table,
                format!("id `{raw}` does not match the agent id grammar"),
            )
        })
}

fn parse_kind(text: &str, table: &str) -> Result<Kind, ConfigError> {
    match text {
        "worker" => Ok(Kind::Worker),
        "gateway" => Ok(Kind::Gateway),
        "external" => Ok(Kind::External),
        _ => Err(ConfigError::new(
            table,
            "kind must be \"worker\", \"gateway\" or \"external\"",
        )),
    }
}

fn parse_backend(text: &str, table: &str) -> Result<Backend, ConfigError> {
    match text {
        "acp-omp" => Ok(Backend::AcpOmp),
        "acp-claude" => Ok(Backend::AcpClaude),
        "a2a-http" => Ok(Backend::A2aHttp),
        _ => Err(ConfigError::new(
            table,
            format!("backend `{text}` is not one of acp-omp, acp-claude, a2a-http"),
        )),
    }
}

fn parse_restart(text: Option<&str>, table: &str) -> Result<Restart, ConfigError> {
    match text {
        None | Some("on-failure") => Ok(Restart::OnFailure),
        Some("never") => Ok(Restart::Never),
        Some(_) => Err(ConfigError::new(
            table,
            "restart must be \"on-failure\" or \"never\"",
        )),
    }
}

fn build_readiness(
    raw: Option<&RawReadiness>,
    kind: Kind,
    table: &str,
) -> Result<Readiness, ConfigError> {
    let Some(raw) = raw else {
        return if kind == Kind::Worker {
            Ok(Readiness {
                probe: Probe::BusPresence,
                timeout: Duration::from_millis(30_000),
            })
        } else {
            Err(ConfigError::new(
                table,
                "readiness is required for externals and the gateway (0012-008)",
            ))
        };
    };
    let timeout_ms = |default: u64| raw.timeout_ms.unwrap_or(default);
    let probe = match raw.kind.as_str() {
        "tcp" => {
            let address = raw
                .address
                .clone()
                .ok_or_else(|| ConfigError::new(table, "readiness tcp requires `address`"))?;
            loopback_socket(&address, table, "readiness.address")?;
            (Probe::Tcp { address }, timeout_ms(10_000))
        }
        "http" | "card" => {
            let url = raw
                .url
                .clone()
                .ok_or_else(|| ConfigError::new(table, "readiness http and card require `url`"))?;
            if !is_loopback_http_url(&url) {
                return Err(ConfigError::new(
                    table,
                    "readiness url must be a loopback http:// URL (probes send no credentials)",
                ));
            }
            if raw.kind == "http" {
                (Probe::Http { url }, timeout_ms(60_000))
            } else {
                (Probe::Card { url }, timeout_ms(15_000))
            }
        }
        "bus-presence" => {
            if kind != Kind::Worker {
                return Err(ConfigError::new(
                    table,
                    "readiness bus-presence is only valid for workers (0012-009)",
                ));
            }
            (Probe::BusPresence, timeout_ms(30_000))
        }
        _ => {
            return Err(ConfigError::new(
                table,
                "readiness kind must be tcp, http, card or bus-presence",
            ));
        }
    };
    if probe.1 == 0 {
        return Err(ConfigError::new(
            table,
            "readiness timeout_ms must be non-zero",
        ));
    }
    if matches!(probe.0, Probe::BusPresence) && raw.address.is_some() {
        return Err(ConfigError::new(
            table,
            "readiness bus-presence takes no address",
        ));
    }
    Ok(Readiness {
        probe: probe.0,
        timeout: Duration::from_millis(probe.1),
    })
}

fn build_workspace(raw: RawWorkspace, table: &str) -> Result<WorkspacePolicy, ConfigError> {
    let mode = match raw.mode.as_deref().unwrap_or("per-session") {
        "per-session" => WorkspaceMode::PerSession,
        "per-task" => WorkspaceMode::PerTask,
        _ => {
            return Err(ConfigError::new(
                table,
                "workspace.mode must be \"per-session\" or \"per-task\"",
            ));
        }
    };
    let base_ref = raw.base_ref.unwrap_or_else(|| "HEAD".to_owned());
    let safe_ref = !base_ref.is_empty()
        && base_ref.len() <= 256
        && !base_ref.starts_with('-')
        && !base_ref.contains("..")
        && base_ref.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-' | b'~' | b'^')
        });
    if !safe_ref {
        return Err(ConfigError::new(
            table,
            "workspace.base_ref is not a safe revision name",
        ));
    }
    let non_git = match raw.non_git.as_deref().unwrap_or("refuse") {
        "refuse" => NonGitPolicy::Refuse,
        "copy" => NonGitPolicy::Copy,
        _ => {
            return Err(ConfigError::new(
                table,
                "workspace.non_git must be \"refuse\" or \"copy\"",
            ));
        }
    };
    Ok(WorkspacePolicy {
        mode,
        base_ref,
        non_git,
    })
}

fn build_delegation(raw: RawDelegation, table: &str) -> Result<Delegation, ConfigError> {
    let range = |value: Option<u64>, default: u64, max: u64, key: &str| {
        let value = value.unwrap_or(default);
        if (1..=max).contains(&value) {
            Ok(value)
        } else {
            Err(ConfigError::new(
                table,
                format!("delegation.{key} must be between 1 and {max}"),
            ))
        }
    };
    let max_inflight = range(raw.max_inflight, DEFAULT_MAX_INFLIGHT, 16, "max_inflight")?;
    let max_sends = range(
        raw.max_sends_per_min,
        DEFAULT_MAX_SENDS,
        600,
        "max_sends_per_min",
    )?;
    let max_calls = range(
        raw.max_calls_per_min,
        DEFAULT_MAX_CALLS,
        1200,
        "max_calls_per_min",
    )?;
    let shim_path = raw
        .shim_path
        .map(|path| absolute(&path, table, "delegation.shim_path"))
        .transpose()?;
    Ok(Delegation {
        peers: raw.peers.unwrap_or_default(),
        max_inflight: u8::try_from(max_inflight).unwrap_or(16),
        max_sends_per_min: u16::try_from(max_sends).unwrap_or(600),
        max_calls_per_min: u16::try_from(max_calls).unwrap_or(1200),
        shim_path,
    })
}

fn build_peer(raw: RawPeer, table: &str) -> Result<PeerSpec, ConfigError> {
    if !(raw.base_url.starts_with("http://") || raw.base_url.starts_with("https://")) {
        return Err(ConfigError::new(
            table,
            "peer.base_url must be an http:// or https:// URL",
        ));
    }
    let timeout_secs = raw.timeout_secs.unwrap_or(DEFAULT_PEER_TIMEOUT_SECS);
    if timeout_secs == 0 {
        return Err(ConfigError::new(
            table,
            "peer.timeout_secs must be non-zero",
        ));
    }
    Ok(PeerSpec {
        base_url: raw.base_url,
        bearer_token_env: raw.bearer_token_env,
        context_secret_env: raw.context_secret_env,
        tenant: raw.tenant,
        timeout_secs,
        streaming: raw.streaming.unwrap_or(false),
    })
}

fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn check_env(
    env: &BTreeMap<String, String>,
    require: &[String],
    allow: &[String],
    table: &str,
) -> Result<(), ConfigError> {
    for name in env.keys() {
        if !valid_env_name(name) {
            return Err(ConfigError::new(
                table,
                format!("env name `{name}` is invalid"),
            ));
        }
        if name.starts_with("A2AB_") {
            return Err(ConfigError::new(
                table,
                format!("env `{name}` uses the reserved A2AB_ prefix"),
            ));
        }
        if is_provider_key(name) {
            return Err(ConfigError::new(
                table,
                format!("env `{name}` is a provider key; provider keys are never set in config"),
            ));
        }
    }
    for name in require {
        if !valid_env_name(name) {
            return Err(ConfigError::new(
                table,
                format!("env_require name `{name}` is invalid"),
            ));
        }
    }
    for name in allow {
        if !is_provider_key(name) {
            return Err(ConfigError::new(
                table,
                format!("env_allow `{name}` is not a provider key name (0012-062)"),
            ));
        }
    }
    Ok(())
}

fn build_agent(raw: RawAgent, index: usize) -> Result<AgentSpec, ConfigError> {
    let id = check_agent_id(&raw.id, &format!("[[agent]] #{index}"))?;
    let table = format!("[[agent]] {id}");
    if RESERVED_IDS.contains(&id.as_str()) {
        return Err(ConfigError::new(
            table,
            format!("id `{id}` is reserved (0012-002); `bus` is configured in [bus]"),
        ));
    }
    let kind = parse_kind(&raw.kind, &table)?;
    if id == GATEWAY_ID && kind != Kind::Gateway {
        return Err(ConfigError::new(
            table,
            "id `gateway` is reserved for the gateway kind",
        ));
    }
    if kind == Kind::Gateway && id != GATEWAY_ID {
        return Err(ConfigError::new(
            table,
            "the gateway id must be `gateway` (RFC-0010 0010-011)",
        ));
    }
    let description = match raw.description.clone() {
        None => None,
        Some(text) if text.is_empty() || text.len() > MAX_DESCRIPTION_BYTES => {
            return Err(ConfigError::new(
                table,
                format!("description must be 1..={MAX_DESCRIPTION_BYTES} bytes"),
            ));
        }
        Some(text) => Some(text),
    };
    let depends_on = raw.depends_on.clone().unwrap_or_default();
    let env = raw.env.clone().unwrap_or_default();
    let env_require = raw.env_require.clone().unwrap_or_default();
    let env_allow = raw.env_allow.clone().unwrap_or_default();
    check_env(&env, &env_require, &env_allow, &table)?;
    let restart = parse_restart(raw.restart.as_deref(), &table)?;
    let readiness = build_readiness(raw.readiness.as_ref(), kind, &table)?;

    let mut spec = AgentSpec {
        id,
        kind,
        backend: None,
        description,
        command: raw.command.clone().unwrap_or_default(),
        gateway_config: None,
        depends_on,
        repo: None,
        workspace: None,
        approval_mode: None,
        dangerous: false,
        peer: None,
        env,
        env_require,
        env_allow,
        passthrough_all: false,
        readiness,
        restart,
        delegation: Delegation::default(),
    };

    match kind {
        Kind::Worker => build_worker(&raw, &mut spec, &table)?,
        Kind::Gateway => build_gateway(&raw, &mut spec, &table)?,
        Kind::External => build_external(&raw, &mut spec, &table)?,
    }
    Ok(spec)
}

fn refuse(present: bool, key: &str, table: &str) -> Result<(), ConfigError> {
    if present {
        Err(ConfigError::new(
            table,
            format!("`{key}` is not valid for this agent kind"),
        ))
    } else {
        Ok(())
    }
}

fn build_worker(raw: &RawAgent, spec: &mut AgentSpec, table: &str) -> Result<(), ConfigError> {
    let backend_text = raw
        .backend
        .as_deref()
        .ok_or_else(|| ConfigError::new(table, "workers require `backend`"))?;
    let backend = parse_backend(backend_text, table)?;
    spec.backend = Some(backend);
    refuse(raw.gateway_config.is_some(), "gateway_config", table)?;

    if backend.is_acp() {
        refuse(raw.peer.is_some(), "peer", table)?;
        let program = raw
            .command
            .as_ref()
            .and_then(|command| command.first())
            .ok_or_else(|| {
                ConfigError::new(
                    table,
                    "acp backends require `command` with the program first",
                )
            })?;
        if program.is_empty() {
            return Err(ConfigError::new(table, "command[0] must not be empty"));
        }
        let repo = raw
            .repo
            .as_deref()
            .ok_or_else(|| ConfigError::new(table, "acp backends require `repo`"))?;
        spec.repo = Some(absolute(repo, table, "repo")?);
        spec.workspace = Some(match &raw.workspace {
            Some(ws) => build_workspace(
                RawWorkspace {
                    mode: ws.mode.clone(),
                    base_ref: ws.base_ref.clone(),
                    non_git: ws.non_git.clone(),
                },
                table,
            )?,
            None => WorkspacePolicy {
                mode: WorkspaceMode::PerSession,
                base_ref: "HEAD".to_owned(),
                non_git: NonGitPolicy::Refuse,
            },
        });
    } else {
        refuse(
            !raw.command.as_ref().is_none_or(Vec::is_empty),
            "command",
            table,
        )?;
        refuse(raw.repo.is_some(), "repo", table)?;
        refuse(raw.workspace.is_some(), "workspace", table)?;
        let peer = raw
            .peer
            .as_ref()
            .ok_or_else(|| ConfigError::new(table, "a2a-http requires a `peer` table"))?;
        spec.peer = Some(build_peer(
            RawPeer {
                base_url: peer.base_url.clone(),
                bearer_token_env: peer.bearer_token_env.clone(),
                context_secret_env: peer.context_secret_env.clone(),
                tenant: peer.tenant.clone(),
                timeout_secs: peer.timeout_secs,
                streaming: peer.streaming,
            },
            table,
        )?);
    }

    let approval = raw
        .approval_mode
        .clone()
        .unwrap_or_else(|| backend.default_approval().to_owned());
    let (safe, dangerous) = backend.modes();
    let is_dangerous = dangerous.contains(&approval.as_str());
    if !is_dangerous && !safe.contains(&approval.as_str()) {
        return Err(ConfigError::new(
            table,
            format!(
                "approval_mode `{approval}` is not valid for backend {}",
                backend.as_str()
            ),
        ));
    }
    if is_dangerous {
        let acknowledged = raw.acknowledge_dangerous.as_deref() == Some(approval.as_str());
        if !acknowledged {
            return Err(ConfigError::new(
                table,
                format!(
                    "dangerous approval_mode `{approval}` requires acknowledge_dangerous = \"{approval}\" (RFC-0012 0012-051)"
                ),
            ));
        }
        spec.dangerous = true;
    } else if raw.acknowledge_dangerous.is_some() {
        return Err(ConfigError::new(
            table,
            "acknowledge_dangerous is set but approval_mode is not dangerous",
        ));
    }
    spec.approval_mode = Some(approval);

    spec.passthrough_all = match raw.env_passthrough.as_deref() {
        None | Some("none") => false,
        Some("all") => true,
        Some(_) => {
            return Err(ConfigError::new(
                table,
                "env_passthrough must be \"all\" or \"none\"",
            ));
        }
    };

    if let Some(raw_delegation) = raw.delegation.as_ref() {
        spec.delegation = build_delegation(
            RawDelegation {
                peers: raw_delegation.peers.clone(),
                max_inflight: raw_delegation.max_inflight,
                max_sends_per_min: raw_delegation.max_sends_per_min,
                max_calls_per_min: raw_delegation.max_calls_per_min,
                shim_path: raw_delegation.shim_path.clone(),
            },
            table,
        )?;
    }
    Ok(())
}

fn build_gateway(raw: &RawAgent, spec: &mut AgentSpec, table: &str) -> Result<(), ConfigError> {
    refuse(raw.backend.is_some(), "backend", table)?;
    refuse(raw.repo.is_some(), "repo", table)?;
    refuse(raw.workspace.is_some(), "workspace", table)?;
    refuse(raw.approval_mode.is_some(), "approval_mode", table)?;
    refuse(raw.peer.is_some(), "peer", table)?;
    refuse(raw.delegation.is_some(), "delegation", table)?;
    refuse(raw.env_passthrough.is_some(), "env_passthrough", table)?;
    refuse(!spec.env_allow.is_empty(), "env_allow", table)?;
    refuse(raw.description.is_some(), "description", table)?;
    if spec.readiness.probe == Probe::BusPresence {
        return Err(ConfigError::new(
            table,
            "the gateway readiness must be tcp, http or card",
        ));
    }
    match (&raw.command, &raw.gateway_config) {
        (Some(command), None) if !command.is_empty() => {
            if command[0].is_empty() {
                return Err(ConfigError::new(table, "command[0] must not be empty"));
            }
        }
        (None, Some(path)) => {
            spec.gateway_config = Some(absolute(path, table, "gateway_config")?);
        }
        _ => {
            return Err(ConfigError::new(
                table,
                "the gateway needs either `command` or `gateway_config`, not both",
            ));
        }
    }
    if let Some(command) = &raw.command {
        spec.command = command.clone();
    }
    Ok(())
}

fn build_external(raw: &RawAgent, spec: &mut AgentSpec, table: &str) -> Result<(), ConfigError> {
    refuse(raw.backend.is_some(), "backend", table)?;
    refuse(raw.repo.is_some(), "repo", table)?;
    refuse(raw.workspace.is_some(), "workspace", table)?;
    refuse(raw.approval_mode.is_some(), "approval_mode", table)?;
    refuse(
        raw.acknowledge_dangerous.is_some(),
        "acknowledge_dangerous",
        table,
    )?;
    refuse(raw.peer.is_some(), "peer", table)?;
    refuse(raw.delegation.is_some(), "delegation", table)?;
    refuse(raw.env_passthrough.is_some(), "env_passthrough", table)?;
    refuse(!spec.env_allow.is_empty(), "env_allow", table)?;
    refuse(raw.gateway_config.is_some(), "gateway_config", table)?;
    refuse(raw.description.is_some(), "description", table)?;
    if spec.readiness.probe == Probe::BusPresence {
        return Err(ConfigError::new(
            table,
            "externals are not bus agents; readiness must be tcp, http or card",
        ));
    }
    let command = raw
        .command
        .as_ref()
        .filter(|command| !command.is_empty())
        .ok_or_else(|| ConfigError::new(table, "externals require a non-empty `command`"))?;
    if command[0].is_empty() {
        return Err(ConfigError::new(table, "command[0] must not be empty"));
    }
    spec.command = command.clone();
    Ok(())
}

fn check_ids_and_refs(agents: &[AgentSpec]) -> Result<(), ConfigError> {
    let mut ids = BTreeSet::new();
    for agent in agents {
        if !ids.insert(agent.id.as_str()) {
            return Err(ConfigError::new(
                format!("[[agent]] {}", agent.id),
                "duplicate id",
            ));
        }
    }
    let gateways = agents
        .iter()
        .filter(|agent| agent.kind == Kind::Gateway)
        .count();
    if gateways > 1 {
        return Err(ConfigError::new(
            "[[agent]]",
            "at most one gateway is allowed",
        ));
    }

    for agent in agents {
        let table = format!("[[agent]] {}", agent.id);
        for dep in &agent.depends_on {
            if dep == "bus" {
                continue;
            }
            if dep == &agent.id {
                return Err(ConfigError::new(table, "depends_on names the agent itself"));
            }
            if !ids.contains(dep.as_str()) {
                return Err(ConfigError::new(
                    table,
                    format!("depends_on `{dep}` is not a configured agent"),
                ));
            }
        }
        if agent.is_worker() {
            for peer in &agent.delegation.peers {
                AgentId::parse(peer).map_err(|_| {
                    ConfigError::new(
                        table.clone(),
                        format!("delegation peer `{peer}` does not match the agent id grammar"),
                    )
                })?;
                if peer == &agent.id {
                    return Err(ConfigError::new(
                        table.clone(),
                        "delegation peer equals the worker itself (0015-086)",
                    ));
                }
                if !ids.contains(peer.as_str()) {
                    return Err(ConfigError::new(
                        table.clone(),
                        format!("delegation peer `{peer}` is not a configured agent (0015-086)"),
                    ));
                }
            }
        }
    }
    check_acyclic(agents)
}

fn check_acyclic(agents: &[AgentSpec]) -> Result<(), ConfigError> {
    fn walk<'a>(
        id: &'a str,
        agents: &'a [AgentSpec],
        done: &mut BTreeSet<&'a str>,
        active: &mut Vec<&'a str>,
    ) -> Result<(), ConfigError> {
        if done.contains(id) {
            return Ok(());
        }
        if active.contains(&id) {
            return Err(ConfigError::new(
                format!("[[agent]] {id}"),
                "depends_on forms a cycle",
            ));
        }
        active.push(id);
        if let Some(agent) = agents.iter().find(|agent| agent.id == id) {
            for dep in &agent.depends_on {
                if dep != "bus" {
                    walk(dep, agents, done, active)?;
                }
            }
        }
        active.pop();
        done.insert(id);
        Ok(())
    }
    let mut done = BTreeSet::new();
    let mut active = Vec::new();
    for agent in agents {
        walk(&agent.id, agents, &mut done, &mut active)?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn state() -> PathBuf {
        PathBuf::from(if cfg!(windows) {
            "C:\\state\\a2abus"
        } else {
            "/state/a2abus"
        })
    }

    fn abs(path: &str) -> String {
        if cfg!(windows) {
            format!("C:\\dev\\{path}")
        } else {
            format!("/dev/{path}")
        }
    }

    const SHA: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    fn doc(body: &str) -> String {
        format!(
            "config_version = 1\n[bus]\nsha256 = \"{SHA}\"\n{body}",
            body = body
        )
    }

    fn worker_omp(extra: &str) -> String {
        format!(
            "[[agent]]\nid = \"omp\"\nkind = \"worker\"\nbackend = \"acp-omp\"\ncommand = [\"omp\", \"--mode\", \"acp\"]\nrepo = \"{repo}\"\n{extra}",
            repo = abs("repo").replace('\\', "\\\\"),
        )
    }

    fn parse(text: &str) -> Result<Config, ConfigError> {
        Config::parse(text, state())
    }

    #[test]
    fn stable_0012_001_unknown_key_names_key_and_table() {
        let err = parse(&doc("[supervisor]\nbogus = 1\n")).unwrap_err();
        assert_eq!(err.table, "[supervisor]");
        let err = parse(&doc("[paths]\nbogus = \"x\"\n")).unwrap_err();
        assert_eq!(err.table, "[paths]");
        assert!(err.detail.contains("bogus"), "{err}");
        let err = parse(&doc(&worker_omp(
            "bogus_key = 1\napproval_mode = \"ask-all\"\n",
        )))
        .unwrap_err();
        assert!(err.table.starts_with("[[agent]] #"), "{err}");
        assert!(err.detail.contains("bogus_key"), "{err}");
    }

    #[test]
    fn stable_0012_001_config_version_is_one() {
        let err = parse("config_version = 2\n").unwrap_err();
        assert!(err.detail.contains("config_version"));
    }

    #[test]
    fn stable_0012_002_reserved_ids_refused() {
        for id in ["tui", "cli", "supervisor", "bus"] {
            let text = doc(&format!(
                "[[agent]]\nid = \"{id}\"\nkind = \"external\"\ncommand = [\"x\"]\nreadiness = {{ kind = \"tcp\", address = \"127.0.0.1:9\" }}\n"
            ));
            let err = parse(&text).unwrap_err();
            assert!(err.detail.contains("reserved"), "{id}: {err}");
        }
    }

    #[test]
    fn stable_0012_002_duplicate_ids_refused() {
        let ext = "[[agent]]\nid = \"peer\"\nkind = \"external\"\ncommand = [\"x\"]\nreadiness = { kind = \"tcp\", address = \"127.0.0.1:9\" }\n";
        let text = doc(&format!("{ext}{ext}"));
        let err = parse(&text).unwrap_err();
        assert!(err.detail.contains("duplicate"), "{err}");
    }

    #[test]
    fn stable_0012_003_worker_requires_backend_and_external_requires_command() {
        let no_backend = doc("[[agent]]\nid = \"omp\"\nkind = \"worker\"\n");
        assert!(parse(&no_backend).unwrap_err().detail.contains("backend"));
        let no_cmd = doc(
            "[[agent]]\nid = \"peer\"\nkind = \"external\"\nreadiness = { kind = \"tcp\", address = \"127.0.0.1:9\" }\n",
        );
        assert!(parse(&no_cmd).unwrap_err().detail.contains("command"));
    }

    #[test]
    fn stable_0012_003_only_worker_may_have_workspace() {
        let text = doc(
            "[[agent]]\nid = \"peer\"\nkind = \"external\"\ncommand = [\"x\"]\nworkspace = { mode = \"per-session\" }\nreadiness = { kind = \"tcp\", address = \"127.0.0.1:9\" }\n",
        );
        assert!(parse(&text).unwrap_err().detail.contains("workspace"));
    }

    #[test]
    fn stable_0012_004_all_violations_reported_before_start() {
        let text = doc(&format!(
            "{}[[agent]]\nid = \"bad\"\nkind = \"external\"\n",
            worker_omp("approval_mode = \"ask-all\"\n")
        ));
        assert!(parse(&text).is_err());
    }

    #[test]
    fn stable_0012_051_dangerous_mode_needs_exact_acknowledgement() {
        let refused = doc(&worker_omp("approval_mode = \"yolo\"\n"));
        assert!(
            parse(&refused)
                .unwrap_err()
                .detail
                .contains("acknowledge_dangerous")
        );
        let wrong = doc(&worker_omp(
            "approval_mode = \"yolo\"\nacknowledge_dangerous = \"write\"\n",
        ));
        assert!(parse(&wrong).is_err());
        let ok = parse(&doc(&worker_omp(
            "approval_mode = \"yolo\"\nacknowledge_dangerous = \"yolo\"\n",
        )))
        .unwrap();
        let omp = ok.agent("omp").unwrap();
        assert!(omp.dangerous);
        assert_eq!(omp.approval_mode.as_deref(), Some("yolo"));
    }

    #[test]
    fn stable_0012_051_claude_bypass_alias_is_dangerous() {
        let text = doc(&format!(
            "[[agent]]\nid = \"claude\"\nkind = \"worker\"\nbackend = \"acp-claude\"\ncommand = [\"node\", \"entry.js\"]\nrepo = \"{}\"\napproval_mode = \"bypass\"\nacknowledge_dangerous = \"bypass\"\n",
            abs("repo").replace('\\', "\\\\")
        ));
        assert!(parse(&text).unwrap().agent("claude").unwrap().dangerous);
    }

    #[test]
    fn stable_0015_086_delegation_peers_checked() {
        let omp = worker_omp("delegation = { peers = [\"claude\"] }\n");
        let claude = format!(
            "[[agent]]\nid = \"claude\"\nkind = \"worker\"\nbackend = \"acp-claude\"\ncommand = [\"node\"]\nrepo = \"{}\"\n",
            abs("repo").replace('\\', "\\\\")
        );
        let ok = parse(&doc(&format!("{omp}{claude}"))).unwrap();
        assert_eq!(
            ok.agent("omp").unwrap().delegation.peers,
            vec!["claude".to_owned()]
        );
        let unknown = parse(&doc(&worker_omp("delegation = { peers = [\"ghost\"] }\n")));
        assert!(
            unknown
                .unwrap_err()
                .detail
                .contains("not a configured agent")
        );
        let selfish = parse(&doc(&worker_omp("delegation = { peers = [\"omp\"] }\n")));
        assert!(selfish.unwrap_err().detail.contains("itself"));
        let grammar = parse(&doc(&worker_omp("delegation = { peers = [\"Bad Id\"] }\n")));
        assert!(grammar.unwrap_err().detail.contains("grammar"));
    }

    #[test]
    fn stable_0015_086_delegation_limits_bounded() {
        let text = doc(&worker_omp("delegation = { max_inflight = 17 }\n"));
        assert!(parse(&text).unwrap_err().detail.contains("max_inflight"));
    }

    #[test]
    fn stable_0012_062_env_allow_only_provider_keys() {
        let text = doc(&worker_omp("env_allow = [\"PATH\"]\n"));
        assert!(parse(&text).unwrap_err().detail.contains("provider key"));
        let ok = parse(&doc(&worker_omp("env_allow = [\"ANTHROPIC_API_KEY\"]\n"))).unwrap();
        assert_eq!(
            ok.agent("omp").unwrap().env_allow,
            vec!["ANTHROPIC_API_KEY"]
        );
    }

    #[test]
    fn stable_0012_062_env_passthrough_all_is_flagged() {
        let ok = parse(&doc(&worker_omp("env_passthrough = \"all\"\n"))).unwrap();
        assert!(ok.agent("omp").unwrap().passthrough_all);
        let bad = parse(&doc(&worker_omp("env_passthrough = \"some\"\n")));
        assert!(bad.is_err());
    }

    #[test]
    fn stable_0012_022_a2ab_prefix_and_provider_keys_refused_in_env() {
        let a2ab = doc(&worker_omp("env = { A2AB_AGENT_ID = \"x\" }\n"));
        assert!(parse(&a2ab).unwrap_err().detail.contains("A2AB_"));
        let key = doc(&worker_omp("env = { OPENAI_API_KEY = \"x\" }\n"));
        assert!(parse(&key).unwrap_err().detail.contains("provider key"));
    }

    #[test]
    fn stable_0012_010_probes_loopback_only() {
        let remote = doc(
            "[[agent]]\nid = \"peer\"\nkind = \"external\"\ncommand = [\"x\"]\nreadiness = { kind = \"http\", url = \"http://10.0.0.5:9900/health\" }\n",
        );
        assert!(parse(&remote).unwrap_err().detail.contains("loopback"));
        let local = doc(
            "[[agent]]\nid = \"peer\"\nkind = \"external\"\ncommand = [\"x\"]\nreadiness = { kind = \"http\", url = \"http://127.0.0.1:9900/health\", timeout_ms = 60000 }\n",
        );
        let cfg = parse(&local).unwrap();
        assert_eq!(
            cfg.agent("peer").unwrap().readiness.probe,
            Probe::Http {
                url: "http://127.0.0.1:9900/health".to_owned()
            }
        );
    }

    #[test]
    fn stable_0012_009_bus_presence_only_for_workers() {
        let text = doc(
            "[[agent]]\nid = \"peer\"\nkind = \"external\"\ncommand = [\"x\"]\nreadiness = { kind = \"bus-presence\" }\n",
        );
        assert!(parse(&text).unwrap_err().detail.contains("bus-presence"));
    }

    #[test]
    fn stable_0012_003_gateway_id_and_command_rules() {
        let wrong_id = doc(
            "[[agent]]\nid = \"gw\"\nkind = \"gateway\"\ncommand = [\"a2a-gateway\"]\nreadiness = { kind = \"card\", url = \"http://127.0.0.1:9910/.well-known/agent-card.json\" }\n",
        );
        assert!(parse(&wrong_id).unwrap_err().detail.contains("gateway"));
        let both = doc(
            "[[agent]]\nid = \"gateway\"\nkind = \"gateway\"\ncommand = [\"a2a-gateway\"]\ngateway_config = \"/etc/gw.toml\"\nreadiness = { kind = \"card\", url = \"http://127.0.0.1:9910/.well-known/agent-card.json\" }\n",
        );
        assert!(parse(&both).is_err());
        let cfg_only = doc(&format!(
            "[[agent]]\nid = \"gateway\"\nkind = \"gateway\"\ngateway_config = \"{}\"\nreadiness = {{ kind = \"card\", url = \"http://127.0.0.1:9910/.well-known/agent-card.json\" }}\n",
            abs("gw.toml").replace('\\', "\\\\")
        ));
        assert!(parse(&cfg_only).unwrap().gateway().is_some());
    }

    #[test]
    fn stable_0012_007_start_order_follows_dependencies() {
        let text = doc(&format!(
            "[[agent]]\nid = \"omp\"\nkind = \"worker\"\nbackend = \"acp-omp\"\ncommand = [\"omp\"]\nrepo = \"{}\"\ndepends_on = [\"hermes\"]\n[[agent]]\nid = \"hermes\"\nkind = \"worker\"\nbackend = \"a2a-http\"\npeer = {{ base_url = \"http://127.0.0.1:9900\" }}\n",
            abs("repo").replace('\\', "\\\\")
        ));
        let cfg = parse(&text).unwrap();
        let order: Vec<&str> = cfg.start_order().iter().map(|a| a.id.as_str()).collect();
        assert_eq!(order, vec!["hermes", "omp"]);
    }

    #[test]
    fn stable_0012_007_dependency_cycle_refused() {
        let text = doc(
            "[[agent]]\nid = \"a1\"\nkind = \"external\"\ncommand = [\"x\"]\ndepends_on = [\"a2\"]\nreadiness = { kind = \"tcp\", address = \"127.0.0.1:9\" }\n[[agent]]\nid = \"a2\"\nkind = \"external\"\ncommand = [\"x\"]\ndepends_on = [\"a1\"]\nreadiness = { kind = \"tcp\", address = \"127.0.0.1:9\" }\n",
        );
        assert!(parse(&text).unwrap_err().detail.contains("cycle"));
    }

    #[test]
    fn stable_0012_002_gateway_id_reserved_for_gateway_kind() {
        let text = doc(
            "[[agent]]\nid = \"gateway\"\nkind = \"external\"\ncommand = [\"x\"]\nreadiness = { kind = \"tcp\", address = \"127.0.0.1:9\" }\n",
        );
        assert!(parse(&text).is_err());
    }

    #[test]
    fn stable_0012_023_bus_sha256_required() {
        let text = "config_version = 1\n[bus]\nport = 4222\n";
        assert!(parse(text).unwrap_err().detail.contains("sha256"));
    }

    #[test]
    fn stable_0012_003_worker_default_readiness_is_bus_presence() {
        let cfg = parse(&doc(&worker_omp(""))).unwrap();
        let omp = cfg.agent("omp").unwrap();
        assert_eq!(omp.readiness.probe, Probe::BusPresence);
        assert_eq!(omp.readiness.timeout, Duration::from_millis(30_000));
        assert_eq!(omp.restart, Restart::OnFailure);
    }

    #[test]
    fn stable_0013_036_health_listen_loopback_only() {
        let remote = doc("[supervisor]\nhealth_listen = \"0.0.0.0:9465\"\n");
        assert!(parse(&remote).unwrap_err().detail.contains("loopback"));
        let default = parse(&doc("")).unwrap();
        assert_eq!(default.health_listen(), "127.0.0.1:9465".parse().unwrap());
    }

    #[test]
    fn stable_0012_015_shutdown_defaults_and_bounds() {
        let cfg = parse(&doc("")).unwrap();
        assert_eq!(cfg.drain(), Duration::from_millis(15_000));
        assert_eq!(cfg.grace(), Duration::from_millis(10_000));
        let big = parse(&doc("[supervisor]\ngrace_ms = 4000000\n"));
        assert!(big.unwrap_err().detail.contains("grace_ms"));
    }

    #[test]
    fn stable_0012_030_workspace_policy_values() {
        let cfg = parse(&doc(&worker_omp(
            "workspace = { mode = \"per-task\", base_ref = \"main\", non_git = \"copy\" }\n",
        )))
        .unwrap();
        let ws = cfg.agent("omp").unwrap().workspace.clone().unwrap();
        assert_eq!(ws.mode, WorkspaceMode::PerTask);
        assert_eq!(ws.non_git, NonGitPolicy::Copy);
        let bad = parse(&doc(&worker_omp(
            "workspace = { base_ref = \"--upload-pack=x\" }\n",
        )));
        assert!(bad.is_err());
    }

    #[test]
    fn stable_0012_060_a2a_http_peer_required_and_command_refused() {
        let missing =
            doc("[[agent]]\nid = \"hermes\"\nkind = \"worker\"\nbackend = \"a2a-http\"\n");
        assert!(parse(&missing).unwrap_err().detail.contains("peer"));
        let with_cmd = doc(
            "[[agent]]\nid = \"hermes\"\nkind = \"worker\"\nbackend = \"a2a-http\"\ncommand = [\"x\"]\npeer = { base_url = \"http://127.0.0.1:9900\" }\n",
        );
        assert!(parse(&with_cmd).unwrap_err().detail.contains("command"));
    }

    #[test]
    fn stable_0012_026_provider_key_values_never_read() {
        let cfg = parse(&doc(&worker_omp("env_require = [\"OMP_API_KEY\"]\n"))).unwrap();
        assert_eq!(cfg.agent("omp").unwrap().env_require, vec!["OMP_API_KEY"]);
    }

    #[test]
    fn stable_0010_069_bus_sha256_must_be_hex64() {
        let text = "config_version = 1\n[bus]\nsha256 = \"zz\"\n";
        assert!(parse(text).unwrap_err().detail.contains("64"));
    }
}
