//! `a2a-tui`: the local operator console (RFC-0012 sec 4.10).
//!
//! This binary is the only place that names the concrete NATS binding (RFC-0001 0001-001). It
//! connects twice as the console identity `tui` (control and audit), loads the operator key from
//! the state directory, and runs the terminal UI on the main thread with the bus driver on a
//! tokio runtime. It does not read the a2ad config: an operator with `[security] trust_file` set
//! there must pass `--trust` with the same path. With `--follow` the binary instead prints what
//! the console would show (agent presence, task states, artifacts, approvals, audit records) to
//! stdout, one line per change, and needs only the bus seed.

use std::io::IsTerminal;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use a2a_client::signing::envelope_hook;
use a2a_core::envelope::SystemClock;
use a2a_tui::app::App;
use a2a_tui::audit_feed::{AuditFeed, BusAuditFeed};
use a2a_tui::driver::{self, Msg};
use a2a_tui::follow::Follower;
use a2a_tui::operator::{self, Identity, console_agent, resolve_state_dir};
use a2a_tui::ui;
use a2a_types::ids::TenantId;
use bus_api::bus::Bus;
use bus_api::name::{ConsumerName, Principal, StreamName, SubjectPattern};
use bus_nats::config::{NatsConfig, Seed};
use bus_nats::layout;

const USAGE: &str = "usage: a2a-tui --seed-file <path> [--url <nats-url>] [--tenant <name>] \
[--state-dir <dir>] [--trust <trust-file>] [--root <dir>]... [--follow]";
const DEFAULT_URL: &str = "nats://127.0.0.1:4222";
const DEFAULT_TENANT: &str = "local";
const AUDIT_CONSUMER: &str = "tui-audit";
/// Durable consumer of `--follow`; separate from [`AUDIT_CONSUMER`] so a follower and an open
/// console never split one consumer's deliveries.
const FOLLOW_CONSUMER: &str = "tui-audit-follow";
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
    follow: bool,
}

impl Args {
    fn parse<I: Iterator<Item = String>>(mut iter: I) -> Result<Self, String> {
        let mut url = DEFAULT_URL.to_owned();
        let mut seed_file = None;
        let mut tenant = DEFAULT_TENANT.to_owned();
        let mut state_dir = None;
        let mut trust = None;
        let mut roots = Vec::new();
        let mut follow = false;
        while let Some(flag) = iter.next() {
            match flag.as_str() {
                "--url" => url = value(&mut iter, &flag)?,
                "--seed-file" => seed_file = Some(PathBuf::from(value(&mut iter, &flag)?)),
                "--tenant" => tenant = value(&mut iter, &flag)?,
                "--state-dir" => state_dir = Some(PathBuf::from(value(&mut iter, &flag)?)),
                "--trust" => trust = Some(PathBuf::from(value(&mut iter, &flag)?)),
                "--root" => roots.push(PathBuf::from(value(&mut iter, &flag)?)),
                "--follow" => follow = true,
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
            follow,
        })
    }
}

fn value<I: Iterator<Item = String>>(iter: &mut I, flag: &str) -> Result<String, String> {
    iter.next()
        .ok_or_else(|| format!("{flag} needs a value\n{USAGE}"))
}

/// Names of the audit tail: the console principal, the tenant's audit stream and subject filter.
fn audit_names(
    tenant: &TenantId,
    identity: &str,
) -> Result<(Principal, StreamName, SubjectPattern), String> {
    let who = Principal::named(identity).map_err(|e| format!("identity: {e}"))?;
    let stream = StreamName::parse(&layout::audit_stream(tenant))
        .map_err(|e| format!("audit stream: {e}"))?;
    let filter = SubjectPattern::parse(&format!("{}.>", layout::tenant_subject(tenant, "audit")))
        .map_err(|e| format!("audit filter: {e}"))?;
    Ok((who, stream, filter))
}

/// Headless mode: prints one line per change (agent presence, task state, artifact, approval,
/// audit record) to stdout until the bus connection fails. Needs only the bus seed; no terminal,
/// operator key or trust file. The driver runs unsigned and its command channel stays unused, so
/// this mode can observe but never decide.
fn follow<B, F>(
    runtime: &tokio::runtime::Runtime,
    control: B,
    feed: F,
    tenant: TenantId,
) -> Result<(), String>
where
    B: Bus + 'static,
    F: AuditFeed + 'static,
{
    let (ui_tx, ui_rx) = std::sync::mpsc::channel::<Msg>();
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let driver = runtime.spawn(driver::run(control, feed, tenant, None, ui_tx, cmd_rx));
    let mut follower = Follower::default();
    let mut out = std::io::stdout();
    while let Ok(msg) = ui_rx.recv() {
        for line in follower.lines(ui::now_ms(), msg) {
            if writeln!(out, "{line}").and_then(|()| out.flush()).is_err() {
                return Ok(());
            }
        }
    }
    drop(cmd_tx);
    match runtime.block_on(driver) {
        Ok(result) => result,
        Err(e) => Err(format!("bus driver: {e}")),
    }
}

fn run() -> Result<(), String> {
    let args = Args::parse(std::env::args().skip(1))?;
    if !args.follow && !std::io::stdout().is_terminal() {
        return Err(
            "terminal: stdout is not a terminal (use --follow for plain output)".to_owned(),
        );
    }
    let tenant = TenantId::parse(&args.tenant).map_err(|e| format!("tenant: {e}"))?;
    let seed_text =
        std::fs::read_to_string(&args.seed_file).map_err(|e| format!("seed file: {e}"))?;
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
    let (who, stream, filter) = audit_names(&tenant, identity.as_str())?;
    let control = runtime
        .block_on(bus_nats::connect(nats.clone()))
        .map_err(|e| format!("control connection: {e}"))?;
    let audit_bus = runtime
        .block_on(bus_nats::connect(nats))
        .map_err(|e| format!("audit connection: {e}"))?;
    let consumer_name = if args.follow {
        FOLLOW_CONSUMER
    } else {
        AUDIT_CONSUMER
    };
    let consumer = ConsumerName::parse(consumer_name).map_err(|e| format!("consumer: {e}"))?;
    let feed = runtime
        .block_on(BusAuditFeed::open(
            &audit_bus, &who, &stream, &consumer, &filter,
        ))
        .map_err(|e| format!("audit feed: {e}"))?;
    if args.follow {
        return follow(&runtime, control, feed, tenant);
    }

    let state = resolve_state_dir(args.state_dir, |key| std::env::var(key).ok(), cfg!(windows))?;
    let trust = args.trust.unwrap_or_else(|| state.join("trust.toml"));
    let creds = state.join("creds");
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
