//! Settings resolved from flags, environment, config file, and built-in defaults
//! (RFC-0012 section 4.1). Precedence: flag, then environment, then file, then default.

use std::path::{Path, PathBuf};
use std::time::Duration;

use a2a_types::ids::{AgentId, TenantId};
use serde::Deserialize;

use crate::cli::Cli;
use crate::error::CliError;
use crate::output::EnvFn;

/// Default NATS endpoint of a local host.
pub const DEFAULT_BUS_URL: &str = "nats://127.0.0.1:4222";
/// Default operator agent id (`[supervisor] operator_id`).
pub const DEFAULT_OPERATOR: &str = "tui";
/// Default tenant (`[supervisor] tenant`).
pub const DEFAULT_TENANT: &str = "local";

/// Config file layout used by the CLI (subset of RFC-0012 section 4.1; unknown keys are ignored).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct FileConfig {
    /// `[supervisor]` table.
    pub supervisor: SupervisorSection,
    /// `[bus]` table.
    pub bus: BusSection,
    /// `[security]` table.
    pub security: SecuritySection,
    /// `[audit]` table.
    pub audit: AuditSection,
    /// `[[agent]]` entries.
    #[serde(rename = "agent")]
    pub agents: Vec<AgentSection>,
}

/// `[supervisor]` table.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct SupervisorSection {
    /// Operator agent id.
    pub operator_id: Option<String>,
    /// Tenant.
    pub tenant: Option<String>,
}

/// `[bus]` table.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct BusSection {
    /// NATS URL.
    pub url: Option<String>,
    /// File holding the operator NKey seed. The seed never appears in the config.
    pub seed_file: Option<PathBuf>,
}

/// `[security]` table.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct SecuritySection {
    /// Directory with keys and `trust.toml`.
    pub config_dir: Option<PathBuf>,
}

/// `[audit]` table.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct AuditSection {
    /// Trust file for checkpoint verification.
    pub trust_file: Option<PathBuf>,
}

/// `[[agent]]` entry.
#[derive(Debug, Deserialize)]
pub struct AgentSection {
    /// Agent id, validated before use.
    pub id: String,
}

/// Flag values that override the environment and file.
#[derive(Debug, Default, Clone)]
pub struct Overrides {
    /// `--config`.
    pub config: Option<PathBuf>,
    /// `--bus-url`.
    pub bus_url: Option<String>,
    /// `--tenant`.
    pub tenant: Option<String>,
    /// `--operator`.
    pub operator: Option<String>,
    /// `--timeout`, seconds.
    pub timeout_secs: Option<u64>,
}

impl Overrides {
    /// Takes the global flags from the parsed command line.
    #[must_use]
    pub fn from_cli(cli: &Cli) -> Self {
        Self {
            config: cli.config.clone(),
            bus_url: cli.bus_url.clone(),
            tenant: cli.tenant.clone(),
            operator: cli.operator.clone(),
            timeout_secs: cli.timeout,
        }
    }
}

/// Fully resolved settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// NATS URL.
    pub bus_url: String,
    /// Operator NKey seed file. Required by the binary to connect.
    pub seed_file: Option<PathBuf>,
    /// Tenant.
    pub tenant: TenantId,
    /// Operator agent id.
    pub operator: AgentId,
    /// Security config directory (keys and `trust.toml`).
    pub security_dir: PathBuf,
    /// Trust file, from `[audit] trust_file` or `<security_dir>/trust.toml`.
    pub trust_file: PathBuf,
    /// Configured agents, searched by `watch`, `cancel` and `approvals`.
    pub agents: Vec<AgentId>,
    /// Bound on each request.
    pub timeout: Option<Duration>,
}

/// Default config file path: `%APPDATA%\a2abus\config.toml` on Windows, otherwise
/// `$XDG_CONFIG_HOME/a2abus/config.toml` or `~/.config/a2abus/config.toml`.
#[must_use]
pub fn default_config_path(env: EnvFn<'_>, windows: bool) -> Option<PathBuf> {
    if windows {
        return env("APPDATA").map(|dir| PathBuf::from(dir).join("a2abus").join("config.toml"));
    }
    let base = env("XDG_CONFIG_HOME")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| env("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("a2abus").join("config.toml"))
}

/// Locates and parses the config file. An explicit path (flag or `A2A_CONFIG`) must exist; the
/// platform default is optional.
///
/// # Errors
/// Returns [`CliError::Usage`] for a missing explicit file and [`CliError::Failed`] for an
/// unreadable or invalid file.
pub fn load_file(
    overrides: &Overrides,
    env: EnvFn<'_>,
    windows: bool,
) -> Result<(Option<PathBuf>, FileConfig), CliError> {
    let (path, explicit) = match overrides.config.clone() {
        Some(path) => (Some(path), true),
        None => match env("A2A_CONFIG") {
            Some(path) => (Some(PathBuf::from(path)), true),
            None => (default_config_path(env, windows), false),
        },
    };
    let Some(path) = path else {
        return Ok((None, FileConfig::default()));
    };
    if !path.exists() {
        if explicit {
            return Err(CliError::Usage(format!(
                "config file {} does not exist",
                path.display()
            )));
        }
        return Ok((None, FileConfig::default()));
    }
    let text = std::fs::read_to_string(&path).map_err(|error| {
        CliError::Failed(format!("cannot read config {}: {error}", path.display()))
    })?;
    let file = parse_file(&text)
        .map_err(|error| CliError::Failed(format!("config {}: {error}", path.display())))?;
    Ok((Some(path), file))
}

/// Parses config file text.
///
/// # Errors
/// Returns the TOML error text.
pub fn parse_file(text: &str) -> Result<FileConfig, String> {
    toml::from_str(text).map_err(|error| error.to_string())
}

/// Resolves settings. `default_security_dir` is the platform default (from
/// `a2a_security::storage::default_config_dir`), passed in so resolution stays pure.
///
/// # Errors
/// Returns [`CliError::Usage`] for an invalid tenant, operator or agent id, or a bad timeout.
pub fn resolve(
    overrides: &Overrides,
    env: EnvFn<'_>,
    file: &FileConfig,
    default_security_dir: &Path,
) -> Result<Settings, CliError> {
    let bus_url = overrides
        .bus_url
        .clone()
        .or_else(|| env("A2A_BUS_URL"))
        .or_else(|| file.bus.url.clone())
        .unwrap_or_else(|| DEFAULT_BUS_URL.to_owned());
    let seed_file = env("A2A_NKEY_SEED_FILE")
        .map(PathBuf::from)
        .or_else(|| file.bus.seed_file.clone());
    let tenant_text = overrides
        .tenant
        .clone()
        .or_else(|| env("A2A_TENANT"))
        .or_else(|| file.supervisor.tenant.clone())
        .unwrap_or_else(|| DEFAULT_TENANT.to_owned());
    let tenant = TenantId::parse(&tenant_text)
        .map_err(|error| CliError::Usage(format!("tenant {tenant_text:?}: {error}")))?;
    let operator_text = overrides
        .operator
        .clone()
        .or_else(|| env("A2A_OPERATOR"))
        .or_else(|| file.supervisor.operator_id.clone())
        .unwrap_or_else(|| DEFAULT_OPERATOR.to_owned());
    let operator = parse_agent(&operator_text)?;
    let security_dir = env("A2A_SECURITY_DIR")
        .map(PathBuf::from)
        .or_else(|| file.security.config_dir.clone())
        .unwrap_or_else(|| default_security_dir.to_path_buf());
    let trust_file = file
        .audit
        .trust_file
        .clone()
        .unwrap_or_else(|| a2a_security::storage::trust_file(&security_dir));
    let agents = file
        .agents
        .iter()
        .map(|agent| parse_agent(&agent.id))
        .collect::<Result<Vec<_>, _>>()?;
    let timeout = overrides.timeout_secs.map(Duration::from_secs);
    Ok(Settings {
        bus_url,
        seed_file,
        tenant,
        operator,
        security_dir,
        trust_file,
        agents,
        timeout,
    })
}

/// Parses an agent id and rejects it before any bus use ([0012-054]).
///
/// # Errors
/// Returns [`CliError::Usage`] when the id does not match RFC-0001.
pub fn parse_agent(text: &str) -> Result<AgentId, CliError> {
    AgentId::parse(text).map_err(|error| CliError::Usage(format!("agent id {text:?}: {error}")))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |key| {
            owned
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        }
    }

    fn security_default() -> PathBuf {
        PathBuf::from("/defaults/security")
    }

    // req_0012_054_config_precedence
    #[test]
    fn req_0012_054_config_precedence() {
        let file = parse_file(
            "[bus]\nurl = \"nats://file:4222\"\n[supervisor]\noperator_id = \"filer\"\ntenant = \"filet\"\n",
        )
        .expect("valid config");
        let env = env_of(&[("A2A_BUS_URL", "nats://env:4222")]);
        let flags = Overrides {
            tenant: Some("flagt".to_owned()),
            ..Overrides::default()
        };
        let settings = resolve(&flags, &env, &file, &security_default()).expect("resolves");
        assert_eq!(settings.bus_url, "nats://env:4222");
        assert_eq!(settings.tenant.as_str(), "flagt");
        assert_eq!(settings.operator.as_str(), "filer");
        assert_eq!(settings.timeout, None);
        assert_eq!(settings.trust_file, security_default().join("trust.toml"));
    }

    // req_0012_054_defaults
    #[test]
    fn req_0012_054_defaults() {
        let settings = resolve(
            &Overrides::default(),
            &env_of(&[]),
            &FileConfig::default(),
            &security_default(),
        )
        .expect("resolves");
        assert_eq!(settings.bus_url, DEFAULT_BUS_URL);
        assert_eq!(settings.tenant.as_str(), DEFAULT_TENANT);
        assert_eq!(settings.operator.as_str(), DEFAULT_OPERATOR);
        assert!(settings.agents.is_empty());
        assert!(settings.seed_file.is_none());
    }

    // req_0012_054_invalid_agent_rejected_before_bus
    #[test]
    fn req_0012_054_invalid_agent_rejected_before_bus() {
        let file = parse_file("[[agent]]\nid = \"Bad Id!\"\n").expect("valid toml");
        let error = resolve(
            &Overrides::default(),
            &env_of(&[]),
            &file,
            &security_default(),
        )
        .expect_err("invalid configured agent id");
        assert!(matches!(error, CliError::Usage(_)));
        assert!(matches!(parse_agent("UPPER"), Err(CliError::Usage(_))));
        assert!(parse_agent("omp").is_ok());
    }

    // req_0012_054_timeout_flag
    #[test]
    fn req_0012_054_timeout_flag() {
        let flags = Overrides {
            timeout_secs: Some(7),
            ..Overrides::default()
        };
        let settings = resolve(
            &flags,
            &env_of(&[]),
            &FileConfig::default(),
            &security_default(),
        )
        .expect("resolves");
        assert_eq!(settings.timeout, Some(Duration::from_secs(7)));
    }

    // req_0012_054_default_config_path
    #[test]
    fn req_0012_054_default_config_path() {
        let windows = env_of(&[("APPDATA", "C:\\Users\\op\\AppData\\Roaming")]);
        assert_eq!(
            default_config_path(&windows, true),
            Some(
                PathBuf::from("C:\\Users\\op\\AppData\\Roaming")
                    .join("a2abus")
                    .join("config.toml")
            )
        );
        let xdg = env_of(&[("XDG_CONFIG_HOME", "/xdg")]);
        assert_eq!(
            default_config_path(&xdg, false),
            Some(PathBuf::from("/xdg/a2abus/config.toml"))
        );
        let home = env_of(&[("HOME", "/home/op")]);
        assert_eq!(
            default_config_path(&home, false),
            Some(PathBuf::from("/home/op/.config/a2abus/config.toml"))
        );
        assert_eq!(default_config_path(&env_of(&[]), true), None);
    }

    // req_0012_054_missing_explicit_config_is_usage
    #[test]
    fn req_0012_054_missing_explicit_config_is_usage() {
        let flags = Overrides {
            config: Some(PathBuf::from("definitely/not/here.toml")),
            ..Overrides::default()
        };
        let error = load_file(&flags, &env_of(&[]), false).expect_err("explicit path missing");
        assert!(matches!(error, CliError::Usage(_)));
    }
}
