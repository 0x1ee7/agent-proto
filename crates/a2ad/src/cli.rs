//! Command-line entry points (RFC-0012 4.9; exit codes per 0012-055).
//!
//! Exit codes: 0 success, 1 runtime failure, 2 usage, 3 config file not found,
//! 4 config rejected or access denied, 5 `stop` timed out, 6 bus failure or lock held.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use a2a_types::ids::AgentId;
use clap::{Parser, Subcommand};

use crate::config::Config;
use crate::host::{HostControl, HostError};
use crate::{gateway_host, platform, supervisor, worker_host};

const EXIT_RUNTIME: u8 = 1;
const EXIT_USAGE: u8 = 2;
const EXIT_NOT_FOUND: u8 = 3;
const EXIT_REJECTED: u8 = 4;
const EXIT_TIMEOUT: u8 = 5;
const EXIT_BUS: u8 = 6;

/// Default wait for `a2ad stop` before it gives up with exit 5.
const STOP_TIMEOUT_SECS: u64 = 120;
const STOP_POLL: Duration = Duration::from_millis(200);

#[derive(Parser, Debug)]
#[command(name = "a2ad", version, about = "A2A-Bus supervisor (RFC-0012)")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Start the bus, the gateway and all workers, and supervise them until stopped.
    Run {
        /// Path to the supervisor configuration file.
        #[arg(long)]
        config: PathBuf,
    },
    /// Validate the configuration file without starting anything.
    CheckConfig {
        /// Path to the supervisor configuration file.
        #[arg(long)]
        config: PathBuf,
    },
    /// Print the running supervisor's state and the per-agent run records.
    Status {
        /// Path to the supervisor configuration file.
        #[arg(long)]
        config: PathBuf,
    },
    /// Ask a running supervisor to drain and stop, then wait for it to exit.
    Stop {
        /// Path to the supervisor configuration file.
        #[arg(long)]
        config: PathBuf,
        /// Seconds to wait for the supervisor to exit before failing with exit 5.
        #[arg(long, default_value_t = STOP_TIMEOUT_SECS)]
        timeout_secs: u64,
    },
    /// Internal: run one agent host process spawned by the supervisor.
    #[command(hide = true)]
    Host {
        /// Agent id to host.
        id: String,
        /// Path to the supervisor configuration file.
        #[arg(long)]
        config: PathBuf,
    },
}

/// Parses the command line and runs the selected subcommand.
#[must_use]
pub fn run() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Run { config } => run_supervisor(&config),
        Command::CheckConfig { config } => check_config(&config),
        Command::Status { config } => status(&config),
        Command::Stop {
            config,
            timeout_secs,
        } => stop(&config, Duration::from_secs(timeout_secs)),
        Command::Host { id, config } => run_host(&id, &config),
    }
}

fn run_supervisor(config_arg: &Path) -> ExitCode {
    let config_path = absolute(config_arg);
    let config = match load_config(&config_path) {
        Ok(config) => config,
        Err(code) => return code,
    };
    let stop = Arc::new(AtomicBool::new(false));
    if let Err(err) = install_signal_stop(&stop) {
        eprintln!("a2ad: cannot install signal handler: {err}");
        return ExitCode::from(EXIT_RUNTIME);
    }
    match supervisor::run_locked(config, config_path, stop) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("a2ad: {err}");
            ExitCode::from(err.exit_code())
        }
    }
}

fn check_config(config_arg: &Path) -> ExitCode {
    match load_config(&absolute(config_arg)) {
        Ok(config) => {
            println!(
                "config ok: {} agent(s), state dir {}",
                config.agents().len(),
                config.state_dir().display()
            );
            ExitCode::SUCCESS
        }
        Err(code) => code,
    }
}

fn status(config_arg: &Path) -> ExitCode {
    let config = match load_config(&absolute(config_arg)) {
        Ok(config) => config,
        Err(code) => return code,
    };
    let state = config.state_dir();
    let pid = supervisor::holder_pid(state);
    let running = pid.is_some_and(|pid| platform::process_start_token(pid).is_some());
    let records = match supervisor::read_records(state) {
        Ok(records) => records,
        Err(err) => {
            eprintln!("a2ad: cannot read run records: {err}");
            return ExitCode::from(EXIT_RUNTIME);
        }
    };
    let report = serde_json::json!({
        "supervisor_pid": pid,
        "running": running,
        "agents": records,
    });
    match serde_json::to_string_pretty(&report) {
        Ok(text) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("a2ad: cannot render status: {err}");
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

fn stop(config_arg: &Path, timeout: Duration) -> ExitCode {
    let config = match load_config(&absolute(config_arg)) {
        Ok(config) => config,
        Err(code) => return code,
    };
    let state = config.state_dir();
    let Some(pid) = running_holder(state) else {
        println!("no supervisor is running");
        return ExitCode::SUCCESS;
    };
    if let Err(err) = supervisor::request_stop(state) {
        eprintln!("a2ad: cannot request stop: {err}");
        return ExitCode::from(EXIT_RUNTIME);
    }
    let deadline = Instant::now() + timeout;
    loop {
        if running_holder(state) != Some(pid) {
            println!("supervisor {pid} stopped");
            return ExitCode::SUCCESS;
        }
        if Instant::now() >= deadline {
            eprintln!(
                "a2ad: supervisor {pid} did not stop within {}s",
                timeout.as_secs()
            );
            return ExitCode::from(EXIT_TIMEOUT);
        }
        thread::sleep(STOP_POLL);
    }
}

fn run_host(id: &str, config_arg: &Path) -> ExitCode {
    let config = match load_config(&absolute(config_arg)) {
        Ok(config) => config,
        Err(code) => return code,
    };
    let agent = match AgentId::parse(id) {
        Ok(agent) => agent,
        Err(_) => {
            eprintln!("a2ad: `{id}` is not a valid agent id");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    let Some(spec) = config.agent(agent.as_str()) else {
        eprintln!("a2ad: `{id}` is not configured");
        return ExitCode::from(EXIT_NOT_FOUND);
    };
    let is_worker = spec.is_worker();
    let stop = Arc::new(AtomicBool::new(false));
    if let Err(err) = install_signal_stop(&stop) {
        eprintln!("a2ad: cannot install signal handler: {err}");
        return ExitCode::from(EXIT_RUNTIME);
    }
    watch_lifeline(Arc::clone(&stop));
    let control = HostControl::new(config.state_dir().to_path_buf(), agent.as_str(), stop);
    let result = if is_worker {
        worker_host::run_worker(&config, &agent, &control)
    } else {
        gateway_host::run_gateway(&config, &control)
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => host_failure(&err),
    }
}

/// Returns the supervisor pid only while its process is alive.
fn running_holder(state: &Path) -> Option<u32> {
    supervisor::holder_pid(state).filter(|pid| platform::process_start_token(*pid).is_some())
}

fn host_failure(err: &HostError) -> ExitCode {
    eprintln!("a2ad: {err}");
    ExitCode::from(match err {
        HostError::Config(_) => EXIT_REJECTED,
        HostError::Bus(_) => EXIT_BUS,
        HostError::Io(_) | HostError::Backend(_) => EXIT_RUNTIME,
    })
}

/// Loads the configuration, mapping failures to the RFC-0012 exit codes.
fn load_config(path: &Path) -> Result<Config, ExitCode> {
    if !path.is_file() {
        eprintln!("a2ad: config file not found: {}", path.display());
        return Err(ExitCode::from(EXIT_NOT_FOUND));
    }
    Config::load(path).map_err(|err| {
        eprintln!("a2ad: {err}");
        ExitCode::from(EXIT_REJECTED)
    })
}

fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Sets `stop` on Ctrl-C, Ctrl-Break, SIGTERM or console close (0012-017).
fn install_signal_stop(stop: &Arc<AtomicBool>) -> std::io::Result<()> {
    let flag = Arc::clone(stop);
    ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst)).map_err(std::io::Error::other)
}

/// Sets `stop` when stdin reaches end-of-file: the supervisor closes the child's stdin
/// before any signal, so EOF means the supervisor is draining this host (0012-061).
fn watch_lifeline(stop: Arc<AtomicBool>) {
    thread::spawn(move || {
        let mut sink = [0_u8; 256];
        let mut stdin = std::io::stdin();
        loop {
            match stdin.read(&mut sink) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
        stop.store(true, Ordering::SeqCst);
    });
}
