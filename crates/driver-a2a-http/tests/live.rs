#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Live tests against the Hermes A2A spike gateway (spikes/hermes-a2a/README.md).
//!
//! Start the stub model and the gateway first, then run:
//! `cargo test -p driver-a2a-http --test live -- --ignored`
//! The base URL defaults to `http://127.0.0.1:9911/` and can be overridden with
//! `A2A_LIVE_BASE_URL`. The bearer is read from the spike token file and never printed.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use a2a_worker::backend::{
    Backend, BackendEvent, ChannelSink, InputPart, Outcome, Protocol, StartTurn, TurnRef,
};
use a2a_worker::backend_contract::{check_all_turns_closed, check_turn_stream};
use driver_a2a_http::{HttpBackend, PeerConfig};
use serde_json::{Value, json};

const TOKEN_ENV: &str = "A2A_LIVE_PEER_TOKEN";

fn token_file() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.spikes/hermes/secrets/peer.token")
}

/// The spike token file holds `<name>:<hex>`; only the hex part is the bearer value.
fn bearer_from_file() -> String {
    let text = std::fs::read_to_string(token_file())
        .expect("spike peer token file present (spikes/hermes-a2a/README.md step 3)");
    let (_, token) = text
        .trim()
        .split_once(':')
        .expect("spike token file has the <name>:<hex> format");
    token.to_owned()
}

fn base_url() -> String {
    std::env::var("A2A_LIVE_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:9911/".to_owned())
}

fn collect(rx: &Receiver<BackendEvent>, turn: &str) -> Vec<BackendEvent> {
    let mut events = Vec::new();
    loop {
        let event = rx
            .recv_timeout(Duration::from_secs(60))
            .expect("event within 60 s");
        let last = matches!(&event, BackendEvent::Terminal { turn: t, .. } if t.as_str() == turn);
        events.push(event);
        if last {
            return events;
        }
    }
}

fn terminal(events: &[BackendEvent]) -> Outcome {
    events
        .iter()
        .find_map(|event| match event {
            BackendEvent::Terminal { outcome, .. } => Some(outcome.clone()),
            _ => None,
        })
        .expect("terminal event")
}

/// Runs each turn against the live gateway through one backend, checking the turn contract.
fn run_turns(extra: Value, turns: &[&str]) {
    let token = bearer_from_file();
    let mut config = json!({
        "agent_id": "hermes-live",
        "base_url": base_url(),
        "bearer_token_env": TOKEN_ENV,
        "task_map_path": std::env::temp_dir()
            .join(format!("a2a-http-live-map-{}.json", std::process::id()))
            .to_string_lossy()
            .into_owned(),
        "timeout_secs": 120,
    });
    if let (Some(map), Value::Object(more)) = (config.as_object_mut(), extra) {
        map.extend(more);
    }
    let peer: PeerConfig = serde_json::from_value(config).expect("peer config");
    let settings = peer
        .validate(|name| (name == TOKEN_ENV).then(|| token.clone()))
        .expect("settings");
    let mut backend = HttpBackend::connect(settings).expect("connect");
    let (sink, rx) = ChannelSink::channel();
    backend.attach(Box::new(sink));

    for &turn in turns {
        backend
            .start(StartTurn {
                turn: TurnRef::new(turn),
                origin: "origin-live".to_owned(),
                context_id: "ctx-live-bus".to_owned(),
                input: vec![InputPart::Text(
                    "hello from the A2A-Bus live test".to_owned(),
                )],
                deadline_ms: 0,
                session: None,
            })
            .expect("start accepted");
        let events = collect(&rx, turn);
        check_turn_stream(&events, Protocol::A2aHttp).expect("turn contract");
        check_all_turns_closed(&events).expect("turn closed");
        assert_eq!(terminal(&events), Outcome::Completed, "turn {turn}");
    }
    backend.shutdown(Duration::from_secs(10)).expect("shutdown");
}

#[test]
#[ignore = "live: hermes spike gateway on 127.0.0.1:9911"]
fn live_hermes_unary_turns_complete() {
    run_turns(json!({}), &["live-1", "live-2"]);
}

#[test]
#[ignore = "live: hermes spike gateway on 127.0.0.1:9911"]
fn live_hermes_streaming_turn_completes() {
    run_turns(json!({"streaming": true}), &["live-stream-1"]);
}
