//! `a2a-tui`: the local operator console (RFC-0012 sec 4.10).
//!
//! This binary is the only place that names the concrete NATS binding (RFC-0001 0001-001). It
//! connects twice as the console identity `tui` (control and audit), loads the operator key from
//! the state directory, and runs the terminal UI on the main thread with the bus driver on a
//! tokio runtime. It does not read the a2ad config: an operator with `[security] trust_file` set
//! there must pass `--trust` with the same path.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use a2a_client::signing::envelope_hook;
use a2a_core::envelope::SystemClock;
use a2a_tui::app::App;
use a2a_tui::audit_feed::BusAuditFeed;
use a2a_tui::driver::{self, Msg};
use a2a_tui::operator::{self, Identity, console_agent, resolve_state_dir};
use a2a_tui::ui;
use a2a_types::ids::TenantId;
use bus_api::name::{ConsumerName, Principal, StreamName, SubjectPattern};
use bus_nats::config::{NatsConfig, Seed};
use bus_nats::layout;

const USAGE: &str = "usage: a2a-tui --seed-file <path> [--url <nats-url>] [--tenant <name>] \
[--state-dir <dir>] [--trust <trust-file>] [--root <dir>]...";
const DEFAULT_URL: &str = "nats://127.0.0.1:4222";
const DEFAULT_TENANT: &str = "local";
const AUDIT_CONSUMER: &str = "tui-audit";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("a2a-tui: {message}");
            ExitCode::from(2)
        }
    }
}

/// Command-line options. Parsed by hand to keep the dependency set small.
struct Args {
    url: String,
    seed_file: PathBuf,
    tenant: String,
    state_dir: Option<PathBuf>,
    trust: Option<PathBuf>,
    roots: Vec<PathBuf>,
}

impl Args {
    fn parse<I: Iterator<Item = String>>(mut iter: I) -> Result<Self, String> {
        let mut url = DEFAULT_URL.to_owned();
        let mut seed_file = None;
        let mut tenant = DEFAULT_TENANT.to_owned();
        let mut state_dir = None;
        let mut trust = None;
        let mut roots = Vec::new();
        while let Some(flag) = iter.next() {
            match flag.as_str() {
                "--url" => url = value(&mut iter, &flag)?,
                "--seed-file" => seed_file = Some(PathBuf::from(value(&mut iter, &flag)?)),
                "--tenant" => tenant = value(&mut iter, &flag)?,
                "--state-dir" => state_dir = Some(PathBuf::from(value(&mut iter, &flag)?)),
                "--trust" => trust = Some(PathBuf::from(value(&mut iter, &flag)?)),
                "--root" => roots.push(PathBuf::from(value(&mut iter, &flag)?)),
                "-h" | "--help" => return Err(USAGE.to_owned()),
                other => return Err(format!("unknown argument {other}\n{USAGE}")),
            }
        }
        let Some(seed_file) = seed_file else {
            return Err(format!("--seed-file is required\n{USAGE}"));
        };
        if roots.is_empty() {
            let cwd = std::env::current_dir().map_err(|e| format!("working directory: {e}"))?;
            roots.push(cwd);
        }
        Ok(Self {
            url,
            seed_file,
            tenant,
            state_dir,
            trust,
            roots,
        })
    }
}

fn value<I: Iterator<Item = String>>(iter: &mut I, flag: &str) -> Result<String, String> {
    iter.next()
        .ok_or_else(|| format!("{flag} needs a value\n{USAGE}"))
}

fn run() -> Result<(), String> {
    let args = Args::parse(std::env::args().skip(1))?;
    let tenant = TenantId::parse(&args.tenant).map_err(|e| format!("tenant: {e}"))?;
    let seed_text =
        std::fs::read_to_string(&args.seed_file).map_err(|e| format!("seed file: {e}"))?;
    let state = resolve_state_dir(args.state_dir, |key| std::env::var(key).ok(), cfg!(windows))?;
    let trust = args.trust.unwrap_or_else(|| state.join("trust.toml"));
    let creds = state.join("creds");
    let identity = console_agent();
    let nats = NatsConfig {
        url: args.url,
        identity: identity.as_str().to_owned(),
        seed: Seed::new(seed_text.trim()),
        tenant: tenant.clone(),
        request_timeout: REQUEST_TIMEOUT,
        ack_wait: layout::ACK_WAIT,
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("runtime: {e}"))?;
    let control = runtime
        .block_on(bus_nats::connect(nats.clone()))
        .map_err(|e| format!("control connection: {e}"))?;
    let audit_bus = runtime
        .block_on(bus_nats::connect(nats))
        .map_err(|e| format!("audit connection: {e}"))?;

    let who = Principal::named(identity.as_str()).map_err(|e| format!("identity: {e}"))?;
    let stream = StreamName::parse(&layout::audit_stream(&tenant))
        .map_err(|e| format!("audit stream: {e}"))?;
    let consumer = ConsumerName::parse(AUDIT_CONSUMER).map_err(|e| format!("consumer: {e}"))?;
    let filter = SubjectPattern::parse(&format!("{}.>", layout::tenant_subject(&tenant, "audit")))
        .map_err(|e| format!("audit filter: {e}"))?;
    let feed = runtime
        .block_on(BusAuditFeed::open(
            &audit_bus, &who, &stream, &consumer, &filter,
        ))
        .map_err(|e| format!("audit feed: {e}"))?;

    let (signing, read_only) = match operator::load(&creds, &trust, &args.roots) {
        Identity::Signed(signer) => (Some(envelope_hook(signer, Arc::new(SystemClock))), None),
        Identity::ReadOnly(reason) => (None, Some(reason)),
    };

    let mut terminal = ratatui::try_init().map_err(|e| format!("terminal: {e}"))?;
    let (ui_tx, ui_rx) = std::sync::mpsc::channel::<Msg>();
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(read_only.is_some());
    app.read_only_reason = read_only;

    let driver_tx = ui_tx.clone();
    runtime.spawn(async move {
        if let Err(message) = driver::run(control, feed, tenant, signing, ui_tx, cmd_rx).await {
            drop(driver_tx.send(Msg::Notice(format!("bus driver stopped: {message}"))));
        }
    });

    let outcome = ui::run(&mut terminal, &mut app, &ui_rx, &cmd_tx);
    let restored = ratatui::try_restore();
    outcome.map_err(|e| format!("terminal: {e}"))?;
    restored.map_err(|e| format!("terminal restore: {e}"))?;
    Ok(())
}
