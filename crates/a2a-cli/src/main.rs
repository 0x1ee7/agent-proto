//! `a2a`: operator command-line surface of the A2A-Bus host. This is the only file that names the
//! concrete NATS binding; everything else is generic over `bus_api::Bus`.

#![forbid(unsafe_code)]

use std::io::{self, IsTerminal};
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use a2a_cli::app;
use a2a_cli::cli::{ApprovalsAction, Cli, Command};
use a2a_cli::config::{self, Overrides, Settings};
use a2a_cli::error::{CliError, Exit};
use a2a_cli::keys::{load_operator_signer, operator_creds_dir};
use a2a_cli::output::{self, Io};
use a2a_client::SigningHook;
use a2a_core::envelope::SystemClock;
use a2a_security::trust::TrustStore;
use bus_nats::config::{NatsConfig, Seed};
use clap::Parser;

/// Request timeout used when `--timeout` is not given.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// JetStream acknowledgement wait.
const ACK_WAIT: Duration = Duration::from_secs(30);

fn main() -> ExitCode {
    let cli = Cli::parse();
    let outcome = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime.block_on(run(&cli)),
        Err(error) => Err(CliError::Failed(format!("cannot start runtime: {error}"))),
    };
    match outcome {
        Ok(exit) => ExitCode::from(exit_byte(exit.code())),
        Err(error) => {
            eprintln!("a2a: {error}");
            ExitCode::from(exit_byte(error.exit().code()))
        }
    }
}

/// Narrows an exit code to the byte the process reports; every class fits in a byte.
fn exit_byte(code: i32) -> u8 {
    u8::try_from(code).unwrap_or(1)
}

/// Whether a command needs a bus connection.
fn needs_bus(command: &Command) -> bool {
    !matches!(
        command,
        Command::Audit { .. } | Command::Start { .. } | Command::Stop { .. }
    )
}

/// Whether a command signs an operator decision, which needs the operator's key.
fn needs_signing(command: &Command) -> bool {
    matches!(
        command,
        Command::Approvals {
            action: ApprovalsAction::Approve(_) | ApprovalsAction::Deny(_),
        }
    )
}

/// Loads the trust store when its file exists. Without a file, the store is empty.
fn load_trust(path: &Path) -> Result<TrustStore, CliError> {
    if !path.is_file() {
        return Ok(TrustStore::default());
    }
    TrustStore::load(path).map_err(|error| {
        CliError::Failed(format!("loading trust file {}: {error}", path.display()))
    })
}

/// Reads the NKey seed from its file. The seed is never accepted inline.
fn read_seed(settings: &Settings) -> Result<Seed, CliError> {
    let path = &settings.seed_file;
    let text = std::fs::read_to_string(path).map_err(|error| {
        CliError::Unavailable(format!(
            "cannot read NKey seed file {}: {error}",
            path.display()
        ))
    })?;
    Ok(Seed::new(text.trim()))
}

async fn run(cli: &Cli) -> Result<Exit, CliError> {
    let env = |key: &str| std::env::var(key).ok();
    let windows = cfg!(windows);
    let overrides = Overrides::from_cli(cli);
    let (_path, file) = config::load_file(&overrides, &env, windows)?;
    let state_default = config::default_state_dir(&env, windows)?;
    let settings = config::resolve(&overrides, &env, &file, &state_default)?;

    let stdout = io::stdout();
    let stderr = io::stderr();
    let stdin = io::stdin();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    let mut input = stdin.lock();
    let mut streams = Io {
        out: &mut out,
        err: &mut err,
        input: &mut input,
        interactive: io::stdin().is_terminal(),
        json: cli.json,
        color: output::color_enabled(cli.no_color, &env, io::stdout().is_terminal(), windows),
    };

    if !needs_bus(&cli.command) {
        return app::offline(&cli.command, &settings, &mut streams);
    }

    let seed = read_seed(&settings)?;
    let bus = bus_nats::connect(NatsConfig {
        url: settings.bus_url.clone(),
        identity: settings.operator.as_str().to_owned(),
        seed,
        tenant: settings.tenant.clone(),
        request_timeout: settings.timeout.unwrap_or(DEFAULT_REQUEST_TIMEOUT),
        ack_wait: ACK_WAIT,
    })
    .await
    .map_err(|error| CliError::Unavailable(format!("cannot connect to the bus: {error}")))?;

    let trust = Arc::new(load_trust(&settings.trust_file)?);
    let operator: Option<Arc<dyn SigningHook>> = if needs_signing(&cli.command) {
        let signer = load_operator_signer(&operator_creds_dir(&settings), &trust)?;
        Some(a2a_client::signing::envelope_hook(
            Arc::new(signer),
            Arc::new(SystemClock),
        ))
    } else {
        None
    };
    let session = app::build_session(bus, &settings, operator, Some(trust))?;
    app::dispatch(&cli.command, &session, &mut streams).await
}
