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
/// Creds directory under the state directory (RFC-0012 4.2).
pub const CREDS_DIR: &str = "creds";
/// CLI NKey seed file name under [`CREDS_DIR`].
pub const CLI_SEED_FILE: &str = "cli.nk";
/// Trust file name under the state directory.
pub const TRUST_FILE: &str = "trust.toml";

/// Config file layout used by the CLI (subset of RFC-0012 section 4.1; unknown keys are ignored).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct FileConfig {
    /// `[supervisor]` table.
    pub supervisor: SupervisorSection,
    /// `[bus]` table.
    pub bus: BusSection,
    /// `[paths]` table.
    pub paths: PathsSection,
    /// `[security]` table.
    pub security: SecuritySection,
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
}

/// `[security]` table.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct SecuritySection {
    /// Trust file for the operator and checkpoint verification; same key as a2ad's
    /// `[security] trust_file`.
    pub trust_file: Option<PathBuf>,
}

/// `[paths]` table.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct PathsSection {
    /// State directory (RFC-0012 4.2), overriding the platform default.
    pub state_dir: Option<PathBuf>,
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
    /// `--state-dir`.
    pub state_dir: Option<PathBuf>,
    /// `--trust`.
    pub trust: Option<PathBuf>,
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
            state_dir: cli.state_dir.clone(),
            trust: cli.trust.clone(),
        }
    }
}

/// Fully resolved settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// NATS URL.
    pub bus_url: String,
    /// Operator NKey seed file (`A2A_NKEY_SEED_FILE`), defaulting to
    /// `<state_dir>/creds/cli.nk`.
    pub seed_file: PathBuf,
    /// State directory: holds `creds/` with the NKey seeds and `operator-<K>.key`, and the
    /// default trust file.
    pub state_dir: PathBuf,
    /// Tenant.
    pub tenant: TenantId,
    /// Operator agent id.
    pub operator: AgentId,
    /// Trust file: `--trust`, else `[security] trust_file`, else `<state_dir>/trust.toml`.
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

/// Default state directory, as the supervisor resolves it (RFC-0012 4.2): `%LOCALAPPDATA%\a2abus`
/// on Windows; otherwise `$XDG_STATE_HOME/a2abus` (absolute values only) or `~/.local/state/a2abus`.
///
/// # Errors
/// Returns [`CliError::Failed`] when the variables needed to locate it are unset.
pub fn default_state_dir(env: EnvFn<'_>, windows: bool) -> Result<PathBuf, CliError> {
    if windows {
        return env("LOCALAPPDATA")
            .map(|base| PathBuf::from(base).join("a2abus"))
            .ok_or_else(|| {
                CliError::Failed(
                    "LOCALAPPDATA is not set; cannot locate the state directory".to_owned(),
                )
            });
    }
    if let Some(base) = env("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        return Ok(base.join("a2abus"));
    }
    env("HOME")
        .map(|home| {
            PathBuf::from(home)
                .join(".local")
                .join("state")
                .join("a2abus")
        })
        .ok_or_else(|| {
            CliError::Failed("HOME is not set; cannot locate the state directory".to_owned())
        })
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

/// Resolves settings. `default_state_dir` is the platform default, from
/// [`default_state_dir`], passed in so resolution stays pure.
///
/// # Errors
/// Returns [`CliError::Usage`] for an invalid tenant, operator or agent id, or a bad timeout.
pub fn resolve(
    overrides: &Overrides,
    env: EnvFn<'_>,
    file: &FileConfig,
    default_state_dir: &Path,
) -> Result<Settings, CliError> {
    let bus_url = overrides
        .bus_url
        .clone()
        .or_else(|| env("A2A_BUS_URL"))
        .or_else(|| file.bus.url.clone())
        .unwrap_or_else(|| DEFAULT_BUS_URL.to_owned());
    let state_dir = overrides
        .state_dir
        .clone()
        .or_else(|| env("A2AB_STATE_DIR").map(PathBuf::from))
        .or_else(|| file.paths.state_dir.clone())
        .unwrap_or_else(|| default_state_dir.to_path_buf());
    let seed_file = env("A2A_NKEY_SEED_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| state_dir.join(CREDS_DIR).join(CLI_SEED_FILE));
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
    let trust_file = overrides
        .trust
        .clone()
        .or_else(|| file.security.trust_file.clone())
        .unwrap_or_else(|| a2a_security::storage::trust_file(&state_dir));
    let agents = file
        .agents
        .iter()
        .map(|agent| parse_agent(&agent.id))
        .collect::<Result<Vec<_>, _>>()?;
    let timeout = overrides.timeout_secs.map(Duration::from_secs);
    Ok(Settings {
        bus_url,
        seed_file,
        state_dir,
        tenant,
        operator,
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

    fn state_default() -> PathBuf {
        PathBuf::from("/defaults/state")
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
        let settings = resolve(&flags, &env, &file, &state_default()).expect("resolves");
        assert_eq!(settings.bus_url, "nats://env:4222");
        assert_eq!(settings.tenant.as_str(), "flagt");
        assert_eq!(settings.operator.as_str(), "filer");
        assert_eq!(settings.timeout, None);
        assert_eq!(settings.trust_file, state_default().join("trust.toml"));
    }

    // req_0012_054_defaults
    #[test]
    fn req_0012_054_defaults() {
        let settings = resolve(
            &Overrides::default(),
            &env_of(&[]),
            &FileConfig::default(),
            &state_default(),
        )
        .expect("resolves");
        assert_eq!(settings.bus_url, DEFAULT_BUS_URL);
        assert_eq!(settings.tenant.as_str(), DEFAULT_TENANT);
        assert_eq!(settings.operator.as_str(), DEFAULT_OPERATOR);
        assert!(settings.agents.is_empty());
        assert_eq!(settings.state_dir, state_default());
        assert_eq!(
            settings.seed_file,
            state_default().join(CREDS_DIR).join(CLI_SEED_FILE)
        );
    }

    // req_0012_054_invalid_agent_rejected_before_bus
    #[test]
    fn req_0012_054_invalid_agent_rejected_before_bus() {
        let file = parse_file("[[agent]]\nid = \"Bad Id!\"\n").expect("valid toml");
        let error = resolve(&Overrides::default(), &env_of(&[]), &file, &state_default())
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
            &state_default(),
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

    // req_0012_043_state_dir_from_paths_table
    #[test]
    fn req_0012_043_state_dir_from_paths_table() {
        let file = parse_file("[paths]\nstate_dir = \"/custom/state\"\n").expect("valid config");
        let settings = resolve(&Overrides::default(), &env_of(&[]), &file, &state_default())
            .expect("resolves");
        assert_eq!(settings.state_dir, PathBuf::from("/custom/state"));
        assert_eq!(
            settings.seed_file,
            PathBuf::from("/custom/state")
                .join(CREDS_DIR)
                .join(CLI_SEED_FILE)
        );
    }

    // req_0012_043_state_dir_precedence
    #[test]
    fn req_0012_043_state_dir_precedence() {
        let file = parse_file("[paths]\nstate_dir = \"/file\"\n").expect("valid config");
        let env = env_of(&[("A2AB_STATE_DIR", "/env")]);
        let flags = Overrides {
            state_dir: Some(PathBuf::from("/flag")),
            ..Overrides::default()
        };
        let by_flag = resolve(&flags, &env, &file, &state_default()).expect("resolves");
        assert_eq!(by_flag.state_dir, PathBuf::from("/flag"));
        let by_env =
            resolve(&Overrides::default(), &env, &file, &state_default()).expect("resolves");
        assert_eq!(by_env.state_dir, PathBuf::from("/env"));
        let by_file = resolve(&Overrides::default(), &env_of(&[]), &file, &state_default())
            .expect("resolves");
        assert_eq!(by_file.state_dir, PathBuf::from("/file"));
        let by_default = resolve(
            &Overrides::default(),
            &env_of(&[]),
            &FileConfig::default(),
            &state_default(),
        )
        .expect("resolves");
        assert_eq!(by_default.state_dir, state_default());
    }

    // req_0012_043_trust_file_flag_or_state_dir
    #[test]
    fn req_0012_043_trust_file_flag_or_state_dir() {
        let flags = Overrides {
            trust: Some(PathBuf::from("/flag/trust.toml")),
            ..Overrides::default()
        };
        let by_flag = resolve(
            &flags,
            &env_of(&[]),
            &FileConfig::default(),
            &state_default(),
        )
        .expect("resolves");
        assert_eq!(by_flag.trust_file, PathBuf::from("/flag/trust.toml"));
        let by_state = resolve(
            &Overrides::default(),
            &env_of(&[]),
            &FileConfig::default(),
            &state_default(),
        )
        .expect("resolves");
        assert_eq!(by_state.trust_file, state_default().join(TRUST_FILE));
    }

    // req_0012_043_trust_file_config_before_state_dir
    #[test]
    fn req_0012_043_trust_file_config_before_state_dir() {
        let file =
            parse_file("[security]\ntrust_file = \"/cfg/trust.toml\"\n").expect("valid config");
        let by_file = resolve(&Overrides::default(), &env_of(&[]), &file, &state_default())
            .expect("resolves");
        assert_eq!(by_file.trust_file, PathBuf::from("/cfg/trust.toml"));
        let flags = Overrides {
            trust: Some(PathBuf::from("/flag/trust.toml")),
            ..Overrides::default()
        };
        let by_flag = resolve(&flags, &env_of(&[]), &file, &state_default()).expect("resolves");
        assert_eq!(by_flag.trust_file, PathBuf::from("/flag/trust.toml"));
    }

    // req_0012_043_bus_seed_file_key_not_read
    #[test]
    fn req_0012_043_bus_seed_file_key_not_read() {
        let file = parse_file("[bus]\nseed_file = \"/x.nk\"\n").expect("valid config");
        let settings = resolve(&Overrides::default(), &env_of(&[]), &file, &state_default())
            .expect("resolves");
        assert_eq!(
            settings.seed_file,
            state_default().join(CREDS_DIR).join(CLI_SEED_FILE)
        );
    }

    // req_0012_043_default_state_dir
    #[test]
    fn req_0012_043_default_state_dir() {
        let windows = env_of(&[("LOCALAPPDATA", "C:\\Users\\op\\AppData\\Local")]);
        assert_eq!(
            default_state_dir(&windows, true).expect("set"),
            PathBuf::from("C:\\Users\\op\\AppData\\Local").join("a2abus")
        );
        assert!(matches!(
            default_state_dir(&env_of(&[]), true),
            Err(CliError::Failed(_))
        ));
        let absolute = if cfg!(windows) {
            "C:\\xdg\\state"
        } else {
            "/xdg/state"
        };
        let pairs = [("XDG_STATE_HOME", absolute), ("HOME", "/home/op")];
        let xdg = env_of(&pairs);
        assert_eq!(
            default_state_dir(&xdg, false).expect("set"),
            PathBuf::from(absolute).join("a2abus")
        );
        let relative = env_of(&[("XDG_STATE_HOME", "state"), ("HOME", "/home/op")]);
        assert_eq!(
            default_state_dir(&relative, false).expect("set"),
            PathBuf::from("/home/op/.local/state/a2abus")
        );
        assert!(matches!(
            default_state_dir(&env_of(&[]), false),
            Err(CliError::Failed(_))
        ));
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
