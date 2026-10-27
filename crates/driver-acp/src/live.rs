//! Live tests against real agent binaries driven by the spike stubs. They are
//! `#[ignore]`d and run only with `cargo test -p driver-acp -- --ignored --test-threads=1`
//! after the stub named in each test's reason is running. No real model is called.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::time::Duration;

use a2a_worker::backend::{
    Backend, BackendEvent, ChannelSink, InputPart, Outcome, Protocol, StartTurn, TurnRef,
};
use a2a_worker::backend_contract::check_turn_stream;

use crate::config::{AcpDriverConfig, Profile};
use crate::driver::AcpDriver;

/// Upper bound for one live turn, including model latency of the stub.
const LIVE_WAIT: Duration = Duration::from_secs(120);

/// `.spikes/` at the repository root (gitignored scratch for the spikes).
fn spike_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo = manifest
        .parent()
        .and_then(std::path::Path::parent)
        .expect("crate lives two levels below the repo root");
    repo.join(".spikes")
}

fn env_or(name: &str, default: PathBuf) -> PathBuf {
    std::env::var_os(name).map_or(default, PathBuf::from)
}

fn start_turn(text: &str) -> StartTurn {
    StartTurn {
        turn: TurnRef::new("live-1"),
        context_id: "live-ctx".to_owned(),
        input: vec![InputPart::Text(text.to_owned())],
        origin: "live-operator".to_owned(),
        peer_tools: None,
        deadline_ms: u64::MAX,
        session: None,
    }
}

/// Starts one turn on a fresh driver and collects its events through the Terminal.
fn run_turn(config: AcpDriverConfig, text: &str) -> Vec<BackendEvent> {
    let mut driver = AcpDriver::new(config).expect("valid live config");
    let (sink, rx) = ChannelSink::channel();
    driver.attach(Box::new(sink));
    driver.start(start_turn(text)).expect("turn starts");
    let mut events = Vec::new();
    loop {
        let event = rx.recv_timeout(LIVE_WAIT).expect("terminal before timeout");
        let done = matches!(event, BackendEvent::Terminal { .. });
        events.push(event);
        if done {
            break;
        }
    }
    drop(driver);
    events
}

fn terminal_outcome(events: &[BackendEvent]) -> Outcome {
    events
        .iter()
        .find_map(|e| match e {
            BackendEvent::Terminal { outcome, .. } => Some(outcome.clone()),
            _ => None,
        })
        .expect("turn has a Terminal")
}

fn reply_text(events: &[BackendEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            BackendEvent::ArtifactDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// omp 18.12.6 over `--mode acp`, isolated with `PI_CODING_AGENT_DIR=.spikes/omp-acp/agent`
/// and the stub on `127.0.0.1:18765` (see `spikes/omp-acp/README.md`).
/// Requires `DRIVER_ACP_OMP_BIN` = absolute path of the omp executable. Optional:
/// `DRIVER_ACP_OMP_WORKSPACE` and `DRIVER_ACP_OMP_CONFIG_DIR` override the spike paths.
#[test]
#[ignore = "live: omp --mode acp; stub on 127.0.0.1:18765; needs DRIVER_ACP_OMP_BIN"]
fn live_omp_plain_turn_completes_0009_025() {
    let program = PathBuf::from(
        std::env::var("DRIVER_ACP_OMP_BIN")
            .expect("set DRIVER_ACP_OMP_BIN to the absolute path of omp"),
    );
    let root = spike_root().join("omp-acp");
    let workspace = env_or("DRIVER_ACP_OMP_WORKSPACE", root.join("live-ws"));
    let config_dir = env_or("DRIVER_ACP_OMP_CONFIG_DIR", root.join("agent"));
    std::fs::create_dir_all(&workspace).expect("live workspace");
    let config = AcpDriverConfig::new(Profile::Omp, program, workspace, config_dir);
    let events = run_turn(config, "hello");
    check_turn_stream(&events, Protocol::Acp).expect("turn stream obeys the seam contract");
    assert_eq!(terminal_outcome(&events), Outcome::Completed);
    assert!(
        !reply_text(&events).is_empty(),
        "the stub answers with some text"
    );
}

/// claude-agent-acp 0.97.0 under the spike's Node runtime, against the Messages API stub
/// started by `.spikes/claude-acp/run_stub.py` (prints its base URL).
/// Requires `DRIVER_ACP_CLAUDE_STUB_URL`. Optional: `DRIVER_ACP_NODE` and
/// `DRIVER_ACP_CLAUDE_ENTRY` (default to the spike install under `.spikes/`).
#[test]
#[ignore = "live: claude-agent-acp 0.97.0 against the Messages stub; needs DRIVER_ACP_CLAUDE_STUB_URL"]
fn live_claude_plain_turn_completes_0009_025() {
    let root = spike_root();
    let node = env_or(
        "DRIVER_ACP_NODE",
        root.join("node/node-v24.22.1-win-x64/node.exe"),
    );
    let entry = env_or(
        "DRIVER_ACP_CLAUDE_ENTRY",
        root.join("claude-acp/node_modules/@agentclientprotocol/claude-agent-acp/dist/index.js"),
    );
    let stub_url = std::env::var("DRIVER_ACP_CLAUDE_STUB_URL")
        .expect("set DRIVER_ACP_CLAUDE_STUB_URL to the stub base URL");
    let config_dir = root.join("claude-acp/cfg/live");
    std::fs::create_dir_all(&config_dir).expect("live config dir");
    // Supervisor-owned settings file (0009-084): the default mode is set here, not through `_meta`.
    // The adapter refuses to start a turn without a key ("Authentication required"), and the
    // driver refuses credential names in its env (0009-069). The fake key below is the spike's
    // stub key; it is written only into this throwaway config dir and is never a real credential.
    std::fs::write(
        config_dir.join("settings.json"),
        r#"{"permissions":{"defaultMode":"default"},"env":{"ANTHROPIC_API_KEY":"sk-stub-not-real"}}"#,
    )
    .expect("settings file");
    let workspace = root.join("claude-acp/live-ws");
    std::fs::create_dir_all(&workspace).expect("live workspace");
    let mut config = AcpDriverConfig::new(Profile::Claude, node, workspace, config_dir);
    config.program_args = vec![entry.to_string_lossy().into_owned()];
    config.env.push(("ANTHROPIC_BASE_URL".to_owned(), stub_url));
    let events = run_turn(config, "hello");
    check_turn_stream(&events, Protocol::Acp).expect("turn stream obeys the seam contract");
    assert_eq!(terminal_outcome(&events), Outcome::Completed);
    assert!(
        !reply_text(&events).is_empty(),
        "the stub answers with some text"
    );
}
