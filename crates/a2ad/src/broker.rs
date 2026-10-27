//! Broker lifecycle: `nats-server` binary check, configuration, start, readiness, provisioning
//! and stop (RFC-0010 sections 4.1 and 4.2, RFC-0012 sec 4.4).
//!
//! The broker is configured in `[bus]`, not as an `[[agent]]` (0010-001). It listens on
//! loopback only (0010-002). Its configuration names only public NKeys (0010-069 and the
//! seed rule of RFC-0010). Provisioning runs only after a connection as `supervisor` is
//! accepted (0010-008).

use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use bus_nats::config::{NatsConfig, Seed};
use bus_nats::connect;
use bus_nats::layout::ACK_WAIT;
use bus_nats::permissions::{Role, ServerSpec, ServerUser, permissions, render_server_conf};
use bus_nats::provision::{SUPERVISOR, provision};
use sha2::{Digest, Sha256};

use crate::config::{Config, Kind};
use crate::creds::{self, CredsError, LOGS_DIR, NATS_STORE_DIR, RUN_DIR};
use crate::platform::{self, Containment, PlatformError};

/// Broker configuration file inside `run/`.
pub const BUS_CONF: &str = "nats-server.conf";
/// Broker log file inside `logs/`.
pub const BUS_LOG: &str = "bus.log";
/// Readiness deadline of the broker after spawn.
pub const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// Client-side deadline for the readiness connection.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Broker lifecycle errors. Messages name the rule and never contain secret values.
#[derive(Debug, thiserror::Error)]
pub enum BrokerError {
    /// The configured `nats-server` cannot be found (dependency_missing).
    #[error("nats-server `{0}` not found (dependency_missing)")]
    NotFound(String),
    /// The executable does not match the configured SHA-256 (0010-069).
    #[error("nats-server SHA-256 is {found}, configuration expects {expected} (binary_hash)")]
    HashMismatch {
        /// Digest configured in `[bus] sha256`.
        expected: String,
        /// Digest of the executable on disk.
        found: String,
    },
    /// A permission set could not be derived.
    #[error("permissions for {identity}: {detail}")]
    Permissions {
        /// Identity whose permission set failed.
        identity: String,
        /// Underlying rule message.
        detail: String,
    },
    /// Credential file error.
    #[error(transparent)]
    Creds(#[from] CredsError),
    /// Platform error.
    #[error(transparent)]
    Platform(#[from] PlatformError),
    /// File system error.
    #[error("broker io: {0}")]
    Io(#[from] io::Error),
    /// The broker did not accept a connection before the deadline.
    #[error("broker did not become ready within {0:?}")]
    NotReady(Duration),
    /// The broker exited before it became ready.
    #[error("broker exited before becoming ready: {0}")]
    Exited(String),
    /// Provisioning was rejected by the broker.
    #[error("provisioning failed: {0}")]
    Provision(String),
    /// The async runtime for the readiness connection failed.
    #[error("runtime: {0}")]
    Runtime(String),
}

/// One bus identity the broker must know, with its role (RFC-0010 4.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// Identity name, which is also the NKey file stem.
    pub name: String,
    /// Role used for the permission set.
    pub role: Role,
}

/// Every bus identity of the configuration: supervisor, operator, workers and the gateway.
/// External processes are not bus agents and get no identity.
#[must_use]
pub fn identities(config: &Config) -> Vec<Identity> {
    let mut out = vec![
        Identity {
            name: SUPERVISOR.to_owned(),
            role: Role::Supervisor,
        },
        Identity {
            name: config.operator_id().to_owned(),
            role: Role::Operator,
        },
    ];
    for agent in config.agents() {
        match agent.kind {
            Kind::Worker => out.push(Identity {
                name: agent.id.clone(),
                role: Role::Agent,
            }),
            Kind::Gateway => out.push(Identity {
                name: crate::config::GATEWAY_ID.to_owned(),
                role: Role::Gateway,
            }),
            Kind::External => {}
        }
    }
    out
}

/// The URL workers and operators use to reach the broker (0010-002, loopback).
#[must_use]
pub fn nats_url(config: &Config) -> String {
    format!("nats://127.0.0.1:{}", config.bus().port)
}

/// Renders the broker configuration from the identities and their public NKeys. The output
/// contains no seeds (RFC-0010 section 4.2).
///
/// # Errors
/// Returns [`BrokerError::Permissions`] when a permission set cannot be derived.
pub fn render_config(
    config: &Config,
    state_dir: &Path,
    public_keys: &[(Identity, String)],
) -> Result<String, BrokerError> {
    let mut users = Vec::with_capacity(public_keys.len());
    for (identity, public_key) in public_keys {
        let perms = permissions(identity.role, &identity.name, config.tenant()).map_err(|err| {
            BrokerError::Permissions {
                identity: identity.name.clone(),
                detail: err.to_string(),
            }
        })?;
        users.push(ServerUser {
            public_key: public_key.clone(),
            permissions: perms,
        });
    }
    let store = state_dir.join(NATS_STORE_DIR);
    Ok(render_server_conf(&ServerSpec {
        host: "127.0.0.1",
        port: config.bus().port,
        store_dir: &store,
        users: &users,
    }))
}

/// Resolves the configured `nats-server` to an executable file. An absolute or relative path
/// with a separator is used as given; a bare name is searched on `PATH` (`.exe` added on
/// Windows).
///
/// # Errors
/// Returns [`BrokerError::NotFound`] when no file matches.
pub fn resolve_binary(binary: &Path) -> Result<PathBuf, BrokerError> {
    let display = binary.display().to_string();
    if binary.components().count() > 1 || binary.is_absolute() {
        return if binary.is_file() {
            Ok(binary.to_path_buf())
        } else {
            Err(BrokerError::NotFound(display))
        };
    }
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    let names = executable_names(&display);
    for dir in std::env::split_paths(&path_var) {
        for name in &names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(BrokerError::NotFound(display))
}

#[cfg(windows)]
fn executable_names(name: &str) -> Vec<String> {
    if name.to_ascii_lowercase().ends_with(".exe") {
        vec![name.to_owned()]
    } else {
        vec![format!("{name}.exe"), name.to_owned()]
    }
}

#[cfg(not(windows))]
fn executable_names(name: &str) -> Vec<String> {
    vec![name.to_owned()]
}

/// SHA-256 of a file as lowercase hex (0010-069).
///
/// # Errors
/// Returns [`io::Error`] when the file cannot be read.
pub fn sha256_hex(path: &Path) -> io::Result<String> {
    let bytes = fs::read(path)?;
    let digest = Sha256::digest(&bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    Ok(out)
}

/// Checks the executable against the configured digest (0010-069).
///
/// # Errors
/// Returns [`BrokerError::HashMismatch`] when the digests differ.
pub fn verify_binary(path: &Path, expected: &str) -> Result<(), BrokerError> {
    let found = sha256_hex(path)?;
    if found != expected {
        return Err(BrokerError::HashMismatch {
            expected: expected.to_owned(),
            found,
        });
    }
    Ok(())
}

/// A running, contained `nats-server`.
#[derive(Debug)]
pub struct Broker {
    child: Child,
    containment: Containment,
}

impl Broker {
    /// Verifies the binary, writes the configuration, starts the broker contained, waits for
    /// a connection as `supervisor`, and provisions the tenant (0010-008).
    ///
    /// # Errors
    /// Returns [`BrokerError`] on any failed step; a started broker is stopped first.
    pub fn start(config: &Config, public_keys: &[(Identity, String)]) -> Result<Self, BrokerError> {
        let state = config.state_dir();
        creds::ensure_layout(state)?;
        let binary = resolve_binary(&config.bus().binary)?;
        verify_binary(&binary, &config.bus().sha256)?;
        let conf_text = render_config(config, state, public_keys)?;
        let conf_path = state.join(RUN_DIR).join(BUS_CONF);
        write_owner_only(&conf_path, conf_text.as_bytes())?;
        let log_path = state.join(LOGS_DIR).join(BUS_LOG);
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        platform::restrict_to_owner(&log_path, false)?;
        let log_err = log.try_clone()?;
        let mut command = std::process::Command::new(&binary);
        command
            .arg("-c")
            .arg(&conf_path)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err));
        let (child, containment) = platform::spawn_contained(&mut command)?;
        let mut broker = Self { child, containment };
        match broker.await_ready(config) {
            Ok(()) => Ok(broker),
            Err(err) => {
                broker.stop(Duration::from_secs(1));
                Err(err)
            }
        }
    }

    /// Process id of the broker.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.containment.pid()
    }

    /// Whether the broker process is still running.
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Stops the broker: graceful signal, wait up to `grace`, then forced kill (0012-016).
    pub fn stop(&mut self, grace: Duration) {
        if !self.is_alive() {
            return;
        }
        let _ = self.containment.graceful();
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if !self.is_alive() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.containment.forced();
        let _ = self.child.wait();
    }

    fn await_ready(&mut self, config: &Config) -> Result<(), BrokerError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|err| BrokerError::Runtime(err.to_string()))?;
        let seed = creds::load_seed(config.state_dir(), SUPERVISOR)?;
        let nats = NatsConfig {
            url: nats_url(config),
            identity: SUPERVISOR.to_owned(),
            seed: Seed::new(seed),
            tenant: config.tenant().clone(),
            request_timeout: REQUEST_TIMEOUT,
            ack_wait: ACK_WAIT,
        };
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            if !self.is_alive() {
                return Err(BrokerError::Exited(format!(
                    "pid {} stopped (see {})",
                    self.pid(),
                    BUS_LOG
                )));
            }
            let attempt = runtime.block_on(async {
                let bus = connect(nats.clone()).await.map_err(|err| err.to_string())?;
                provision(&bus)
                    .await
                    .map_err(|err| BrokerError::Provision(err.to_string()).to_string())
            });
            match attempt {
                Ok(()) => return Ok(()),
                Err(msg) if msg.starts_with("provisioning failed") => {
                    return Err(BrokerError::Provision(msg));
                }
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(200));
                }
                Err(_) => return Err(BrokerError::NotReady(READY_TIMEOUT)),
            }
        }
    }
}

fn write_owner_only(path: &Path, bytes: &[u8]) -> Result<(), BrokerError> {
    use std::io::Write;
    if path.exists() {
        fs::remove_file(path)?;
    }
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    platform::restrict_to_owner(path, false)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::config::Config;

    const TENANT_CONF: &str = r#"
config_version = 1
tenant = "local"

[paths]
state_dir = "STATE"

[bus]
port = 4333
binary = "nats-server"
sha256 = "0000000000000000000000000000000000000000000000000000000000000000"

[[agent]]
id = "alpha"
kind = "worker"
backend = "acp-omp"
command = ["omp", "acp"]
repo = "REPO"
"#;

    fn parse_with_paths(dir: &Path) -> Config {
        let state = dir.join("state");
        let repo = dir.join("repo");
        fs::create_dir_all(&repo).expect("repo");
        let state_text = state.to_string_lossy().replace('\\', "\\\\");
        let repo_text = repo.to_string_lossy().replace('\\', "\\\\");
        let text = TENANT_CONF
            .replace("STATE", &state_text)
            .replace("REPO", &repo_text);
        Config::parse(&text, state).expect("config parses")
    }

    #[test]
    fn stable_0010_001_bus_is_not_an_agent_and_identities_include_supervisor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = parse_with_paths(dir.path());
        let ids = identities(&config);
        assert!(
            ids.iter()
                .any(|i| i.name == SUPERVISOR && i.role == Role::Supervisor)
        );
        assert!(
            ids.iter()
                .any(|i| i.name == "alpha" && i.role == Role::Agent)
        );
        assert!(!ids.iter().any(|i| i.name == "bus"));
    }

    #[test]
    fn stable_0010_002_url_is_loopback_on_configured_port() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = parse_with_paths(dir.path());
        assert_eq!(nats_url(&config), "nats://127.0.0.1:4333");
    }

    #[test]
    fn stable_0010_069_digest_mismatch_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("exe");
        fs::write(&file, b"payload").expect("write");
        let real = sha256_hex(&file).expect("digest");
        assert_eq!(real.len(), 64);
        verify_binary(&file, &real).expect("matching digest");
        let err = verify_binary(&file, &"0".repeat(64)).unwrap_err();
        assert!(err.to_string().contains("binary_hash"), "{err}");
    }

    #[test]
    fn stable_0010_002_rendered_config_has_no_seed_and_binds_loopback() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = parse_with_paths(dir.path());
        let keys: Vec<(Identity, String)> = identities(&config)
            .into_iter()
            .map(|identity| (identity, "UPUBKEY".to_owned()))
            .collect();
        let conf = render_config(&config, config.state_dir(), &keys).expect("render");
        assert!(conf.contains("127.0.0.1"));
        assert!(
            !contains_seed_like_token(&conf),
            "seed material must not appear"
        );
    }

    /// True when `text` holds an NKey-seed-shaped token: `SU` followed by at least
    /// 50 base32 characters (seeds are 58 characters).
    fn contains_seed_like_token(text: &str) -> bool {
        text.match_indices("SU").any(|(pos, _)| {
            text[pos..]
                .chars()
                .take_while(|c| c.is_ascii_uppercase() || ('2'..='7').contains(c))
                .count()
                >= 50
        })
    }
}
