//! Operator configuration and launch specification (RFC-0009 sections 4.5, 4.8, 4.10).

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use a2a_core::approval::DEFAULT_EXPIRY_MS;

/// Default turn idle timeout, measured from the last agent activity (0009-024).
pub const DEFAULT_TURN_IDLE_TIMEOUT: Duration = Duration::from_secs(600);
/// Worker approval expiry. The worker applies it to every permission request and passes no
/// per-request value (0006-017), so the driver backstop must outlast it.
pub const WORKER_APPROVAL_EXPIRY: Duration = Duration::from_millis(DEFAULT_EXPIRY_MS);
/// Default backstop for a permission or elicitation wait: the worker expiry plus a 30 s margin
/// (0009-033). The worker's expiry is authoritative; this fires only if the worker did not.
pub const DEFAULT_PERMISSION_TIMEOUT: Duration = Duration::from_secs(330);
/// Default time the agent has to finish a cancelled prompt before the process tree is killed (0009-036).
pub const DEFAULT_CANCEL_GRACE: Duration = Duration::from_secs(10);
/// Default time the agent has to exit after its stdin is closed (0009-060).
pub const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Agent profile: which ACP agent this driver instance launches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// `omp --mode acp` (profile `acp-omp`).
    Omp,
    /// `claude-agent-acp` (profile `acp-claude`).
    Claude,
}

impl Profile {
    /// Parses a profile name from configuration.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "acp-omp" => Some(Self::Omp),
            "acp-claude" => Some(Self::Claude),
            _ => None,
        }
    }

    /// The configuration name of this profile.
    pub fn name(self) -> &'static str {
        match self {
            Self::Omp => "acp-omp",
            Self::Claude => "acp-claude",
        }
    }

    /// Approval mode used when the operator configures none.
    pub fn default_approval_mode(self) -> &'static str {
        match self {
            Self::Omp => "ask-exec",
            Self::Claude => "default",
        }
    }
}

/// Configuration for one ACP driver instance. All paths must be absolute.
#[derive(Debug, Clone)]
pub struct AcpDriverConfig {
    /// Agent profile.
    pub profile: Profile,
    /// Absolute path of the executable (omp, or node on Windows for claude-agent-acp).
    pub program: PathBuf,
    /// Arguments placed before the profile arguments (for example the claude package entry).
    pub program_args: Vec<String>,
    /// Workspace root: ACP `cwd` and omp `--cwd`.
    pub workspace: PathBuf,
    /// Supervisor-owned agent configuration directory (`PI_CODING_AGENT_DIR` or `CLAUDE_CONFIG_DIR`).
    pub config_dir: PathBuf,
    /// Configured approval mode: omp `ask-exec`/`write`/`yolo`; claude `default`/`plan`/...
    pub approval_mode: String,
    /// Operator acknowledgement that a dangerous approval mode may be used (0009-064).
    pub acknowledge_dangerous: bool,
    /// MCP server descriptions passed through verbatim to `session/new` and `session/load`.
    pub mcp_servers: Vec<serde_json::Value>,
    /// Non-secret operator environment pairs added to the child environment.
    pub env: Vec<(String, String)>,
    /// Idle timeout measured from last agent activity (0009-024).
    pub turn_idle_timeout: Duration,
    /// Time a permission or elicitation waits before it is answered as cancelled (0009-033).
    pub permission_timeout: Duration,
    /// Time between `session/cancel` and the process tree kill (0009-036).
    pub cancel_grace: Duration,
    /// Time the child gets to exit after stdin closes (0009-060).
    pub shutdown_grace: Duration,
}

/// Structural configuration errors, reported when the driver is constructed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// The executable path is relative (spawn must use an absolute path, 0009-037).
    #[error("program must be an absolute path")]
    ProgramNotAbsolute,
    /// The workspace path is relative.
    #[error("workspace must be an absolute path")]
    WorkspaceNotAbsolute,
    /// The config directory path is relative.
    #[error("config_dir must be an absolute path")]
    ConfigDirNotAbsolute,
    /// An operator environment pair names a provider credential variable (0009-069).
    #[error("env name {0} is a provider credential variable and cannot be set by the driver")]
    CredentialEnv(String),
    /// An MCP server entry is not a JSON object.
    #[error("mcp_servers entry {0} must be a JSON object")]
    McpEntry(usize),
    /// The driver's worker thread could not be started.
    #[error("driver thread could not start: {0}")]
    ThreadStart(String),
    /// The permission backstop does not outlast the worker's approval expiry (0006-017).
    #[error(
        "permission_timeout must be greater than the worker approval expiry of {}s",
        WORKER_APPROVAL_EXPIRY.as_secs()
    )]
    PermissionTimeoutNotAboveApproval,
}

/// Checks a permission backstop against the worker's approval expiry. The worker's expiry is
/// authoritative and the backstop must strictly outlast it (0006-017). a2ad calls this at config
/// load, so an unsafe value is rejected before any agent is spawned.
pub fn check_permission_timeout(timeout: Duration) -> Result<(), ConfigError> {
    if timeout > WORKER_APPROVAL_EXPIRY {
        Ok(())
    } else {
        Err(ConfigError::PermissionTimeoutNotAboveApproval)
    }
}

/// Provider credential variables the driver refuses to set (0009-069). The driver never reads
/// or forwards credential values; the supervisor owns them.
const CREDENTIAL_ENV_NAMES: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "OPENAI_API_KEY",
    "OMP_API_KEY",
];

/// Parent variables copied into the child environment (0009-068). Nothing else from the worker
/// environment is inherited; operator-named variables come from `AcpDriverConfig::env`.
const INHERITED_ENV_NAMES: &[&str] = &[
    "PATH",
    "PATHEXT",
    "SYSTEMROOT",
    "SYSTEMDRIVE",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "HOME",
    "TEMP",
    "TMP",
    "LANG",
    "LC_ALL",
];

impl AcpDriverConfig {
    /// Builds a configuration with the RFC defaults for `profile`.
    pub fn new(
        profile: Profile,
        program: PathBuf,
        workspace: PathBuf,
        config_dir: PathBuf,
    ) -> Self {
        Self {
            profile,
            program,
            program_args: Vec::new(),
            workspace,
            config_dir,
            approval_mode: profile.default_approval_mode().to_owned(),
            acknowledge_dangerous: false,
            mcp_servers: Vec::new(),
            env: Vec::new(),
            turn_idle_timeout: DEFAULT_TURN_IDLE_TIMEOUT,
            permission_timeout: DEFAULT_PERMISSION_TIMEOUT,
            cancel_grace: DEFAULT_CANCEL_GRACE,
            shutdown_grace: DEFAULT_SHUTDOWN_GRACE,
        }
    }

    /// Structural validation. Approval mode is checked at turn start, not here (0009-064).
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !self.program.is_absolute() {
            return Err(ConfigError::ProgramNotAbsolute);
        }
        if !self.workspace.is_absolute() {
            return Err(ConfigError::WorkspaceNotAbsolute);
        }
        if !self.config_dir.is_absolute() {
            return Err(ConfigError::ConfigDirNotAbsolute);
        }
        for (name, _) in &self.env {
            if CREDENTIAL_ENV_NAMES.contains(&name.as_str()) {
                return Err(ConfigError::CredentialEnv(name.clone()));
            }
        }
        for (index, entry) in self.mcp_servers.iter().enumerate() {
            if !entry.is_object() {
                return Err(ConfigError::McpEntry(index));
            }
        }
        Ok(())
    }

    /// Argument vector and environment for the child process. The parent's credential variables
    /// are never read; only [`INHERITED_ENV_NAMES`] and operator-named pairs are forwarded.
    pub fn launch_spec(&self) -> LaunchSpec {
        let mut args: Vec<OsString> = self.program_args.iter().map(OsString::from).collect();
        let mut env: Vec<(OsString, OsString)> = Vec::new();

        for name in INHERITED_ENV_NAMES {
            if let Some(value) = std::env::var_os(name) {
                env.push((OsString::from(name), value));
            }
        }

        match self.profile {
            Profile::Omp => {
                for fixed in ["--mode", "acp", "--cwd"] {
                    args.push(OsString::from(fixed));
                }
                args.push(self.workspace.as_os_str().to_owned());
                args.push(OsString::from("--approval-mode"));
                // omp 18.12.6 accepts canonical names only (`always-ask-cached`, not `ask-exec`).
                let cli_mode = crate::mode::canonical(Profile::Omp, &self.approval_mode)
                    .map_or_else(|| self.approval_mode.clone(), |(name, _)| name.to_owned());
                args.push(OsString::from(cli_mode));
                env.push((OsString::from("PI_NO_TITLE"), OsString::from("1")));
                env.push((
                    OsString::from("PI_CODING_AGENT_DIR"),
                    self.config_dir.as_os_str().to_owned(),
                ));
            }
            Profile::Claude => {
                env.push((
                    OsString::from("CLAUDE_CONFIG_DIR"),
                    self.config_dir.as_os_str().to_owned(),
                ));
            }
        }
        for (name, value) in &self.env {
            env.push((OsString::from(name), OsString::from(value)));
        }

        LaunchSpec {
            program: self.program.clone(),
            args,
            env,
            cwd: self.workspace.clone(),
        }
    }
}

/// What the spawner needs to start the agent process.
#[derive(Debug, Clone)]
pub struct LaunchSpec {
    /// Absolute path of the agent executable.
    pub program: PathBuf,
    /// Full argument vector, without the program name.
    pub args: Vec<OsString>,
    /// Complete child environment; the parent environment is never inherited wholesale.
    pub env: Vec<(OsString, OsString)>,
    /// Working directory for the child (the workspace).
    pub cwd: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(profile: Profile) -> AcpDriverConfig {
        let root = std::env::temp_dir();
        AcpDriverConfig::new(profile, root.join("omp"), root.clone(), root.join("cfg"))
    }

    #[test]
    fn profile_names_round_trip() {
        assert_eq!(Profile::parse("acp-omp"), Some(Profile::Omp));
        assert_eq!(Profile::parse("acp-claude"), Some(Profile::Claude));
        assert_eq!(Profile::parse("acp-other"), None);
        assert_eq!(Profile::Omp.name(), "acp-omp");
    }

    #[test]
    fn relative_program_is_rejected() {
        let mut c = cfg(Profile::Omp);
        c.program = PathBuf::from("omp");
        assert_eq!(c.validate(), Err(ConfigError::ProgramNotAbsolute));
    }

    #[test]
    fn credential_env_names_are_refused_0009_069() {
        let mut c = cfg(Profile::Claude);
        c.env.push(("ANTHROPIC_API_KEY".to_owned(), "x".to_owned()));
        assert_eq!(
            c.validate(),
            Err(ConfigError::CredentialEnv("ANTHROPIC_API_KEY".to_owned()))
        );
    }

    #[test]
    fn omp_argv_has_mode_cwd_and_approval_mode_and_no_session_dir_0009_039() {
        let c = cfg(Profile::Omp);
        let spec = c.launch_spec();
        let args: Vec<String> = spec
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(&args[..3], ["--mode", "acp", "--cwd"]);
        assert!(args.contains(&"--approval-mode".to_owned()));
        assert!(!args.iter().any(|a| a.contains("session-dir")));
        let env_names: Vec<String> = spec
            .env
            .iter()
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        assert!(env_names.contains(&"PI_CODING_AGENT_DIR".to_owned()));
        assert!(env_names.contains(&"PI_NO_TITLE".to_owned()));
    }

    #[test]
    fn omp_argv_passes_canonical_approval_value_0009_039() {
        let c = cfg(Profile::Omp);
        assert_eq!(c.approval_mode, "ask-exec");
        let args: Vec<String> = c
            .launch_spec()
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let value = args
            .iter()
            .position(|a| a == "--approval-mode")
            .and_then(|at| args.get(at + 1))
            .map(String::as_str);
        assert_eq!(value, Some("always-ask-cached"));
        assert!(!args.iter().any(|a| a == "ask-exec"));
    }

    #[test]
    fn claude_launch_uses_config_dir_env_and_program_args_prefix_0009_084() {
        let mut c = cfg(Profile::Claude);
        c.program_args = vec!["/abs/entry.js".to_owned()];
        let spec = c.launch_spec();
        assert_eq!(spec.args, vec![OsString::from("/abs/entry.js")]);
        assert!(spec.env.iter().any(|(k, _)| k == "CLAUDE_CONFIG_DIR"));
    }

    #[test]
    fn worker_env_is_not_forwarded_except_allowlist_0009_068() {
        let c = cfg(Profile::Claude);
        let spec = c.launch_spec();
        assert!(
            !spec
                .env
                .iter()
                .any(|(k, _)| k.to_string_lossy().starts_with("A2AB_"))
        );
    }

    #[test]
    fn permission_backstop_must_outlast_worker_expiry_0006_017() {
        assert!(check_permission_timeout(WORKER_APPROVAL_EXPIRY).is_err());
        assert!(check_permission_timeout(WORKER_APPROVAL_EXPIRY + Duration::from_secs(1)).is_ok());
        assert!(check_permission_timeout(DEFAULT_PERMISSION_TIMEOUT).is_ok());
        assert_eq!(
            DEFAULT_PERMISSION_TIMEOUT - WORKER_APPROVAL_EXPIRY,
            Duration::from_secs(30)
        );
    }
}
