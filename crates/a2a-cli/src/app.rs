//! Command dispatch shared by the binary and the tests. Commands that need no bus are handled by
//! [`offline`]; everything else goes through [`dispatch`] with a connected [`Session`].

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use a2a_client::{Client, Config, SigningHook};
use a2a_core::envelope::SystemClock;
use a2a_security::trust::TrustStore;
use bus_api::bus::Bus;

use crate::approvals;
use crate::audit;
use crate::cli::{AuditAction, Command};
use crate::commands::{self, Session};
use crate::config::{Settings, parse_agent};
use crate::error::{CliError, Exit};
use crate::output::{Io, line};

/// Wall-clock time in milliseconds since the Unix epoch.
///
/// # Errors
/// Returns a failure when the system clock is before the Unix epoch.
pub fn now_ms() -> Result<u64, CliError> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| CliError::Failed(format!("system clock before the epoch: {error}")))?;
    u64::try_from(elapsed.as_millis())
        .map_err(|_| CliError::Failed("system clock out of range".to_owned()))
}

/// Builds a session over an already connected bus.
///
/// # Errors
/// Returns the mapped client error when the client rejects its configuration.
pub fn build_session<B: Bus>(
    bus: B,
    settings: &Settings,
    operator: Option<Arc<dyn SigningHook>>,
    trust: Option<Arc<TrustStore>>,
) -> Result<Session<B>, CliError> {
    let config = Config {
        trust,
        ..Config::default()
    };
    let client = Client::new(
        bus,
        Arc::new(SystemClock),
        config,
        settings.tenant.clone(),
        settings.operator.clone(),
    )?;
    Ok(Session {
        client,
        settings: settings.clone(),
        operator,
    })
}

/// Verifies audit files against the trust store.
///
/// # Errors
/// Returns not-found for a missing explicit trust file or audit file, or a failure for an
/// unreadable trust file.
pub fn run_audit(
    action: &AuditAction,
    settings: &Settings,
    io: &mut Io<'_>,
) -> Result<Exit, CliError> {
    match action {
        AuditAction::Verify { files, trust } => {
            let trust_path = match trust {
                Some(path) => {
                    if !path.is_file() {
                        return Err(CliError::NotFound(format!(
                            "trust file {} not found",
                            path.display()
                        )));
                    }
                    path.clone()
                }
                None => settings.trust_file.clone(),
            };
            let store = if trust_path.is_file() {
                TrustStore::load(&trust_path).map_err(|error| {
                    CliError::Failed(format!(
                        "loading trust file {}: {error}",
                        trust_path.display()
                    ))
                })?
            } else {
                TrustStore::default()
            };
            let report = audit::verify_files(files, &store)?;
            line(io.out, &audit::render(&report))?;
            Ok(audit::exit_of(&report))
        }
    }
}

/// Handles the commands that never touch the bus. `start` and `stop` validate their arguments
/// first; the supervisor control channel they need is not defined by any RFC yet, so they fail
/// without starting or stopping anything.
///
/// # Errors
/// Returns usage for invalid agent ids, not-found for a missing audit file, or failure when the
/// command cannot be carried out.
pub fn offline(command: &Command, settings: &Settings, io: &mut Io<'_>) -> Result<Exit, CliError> {
    match command {
        Command::Audit { action } => run_audit(action, settings, io),
        Command::Start { agent } => {
            parse_agent(agent)?;
            Err(CliError::Failed(format!(
                "start {agent}: no supervisor control channel is defined (RFC-0012 4.11); agent not started"
            )))
        }
        Command::Stop { agent } => {
            if let Some(agent) = agent {
                parse_agent(agent)?;
            }
            Err(CliError::Failed(
                "stop: no supervisor control channel is defined (RFC-0012 4.11); nothing stopped"
                    .to_owned(),
            ))
        }
        _ => Err(CliError::Failed(
            "command requires a bus connection".to_owned(),
        )),
    }
}

/// Runs a bus command against a connected session. Offline commands are routed to [`offline`].
///
/// # Errors
/// Returns the error of the selected command.
pub async fn dispatch<B: Bus>(
    command: &Command,
    session: &Session<B>,
    io: &mut Io<'_>,
) -> Result<Exit, CliError> {
    match command {
        Command::Discover(args) => commands::discover(session, args, io).await,
        Command::Send(args) => commands::send(session, args, io).await,
        Command::Watch(args) => commands::watch(session, args, io).await,
        Command::Cancel(args) => commands::cancel(session, args, io).await,
        Command::Approvals { action } => {
            let now = now_ms()?;
            approvals::run(session, action, io, now).await
        }
        Command::Audit { .. } | Command::Start { .. } | Command::Stop { .. } => {
            offline(command, &session.settings, io)
        }
    }
}
