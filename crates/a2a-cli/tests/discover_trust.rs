//! `a2a discover` and the trust file: unsigned cards are listed as unverified when no signer key
//! is trusted, and dropped when one is ([0003-042], [0007-050], [0007-053]).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;

use a2a_cli::app::{self, card_trust};
use a2a_cli::cli::DiscoverArgs;
use a2a_cli::commands;
use a2a_security::card::sign_card;
use a2a_security::clock::ManualClock;
use a2a_security::identity::{Ed25519Signer, Signer, encode_public_key};
use a2a_security::trust::TrustStore;
use a2a_types::ids::AgentId;
use common::{Fixture, NOW_MS, Shared, Streams, TENANT};
use serde_json::{Value, json};

fn args() -> DiscoverArgs {
    DiscoverArgs {
        agent: None,
        skill: None,
        tag: None,
        extension: None,
        input_mode: None,
        output_mode: None,
        include_offline: false,
    }
}

fn card() -> Value {
    json!({
        "name": "Worker A",
        "version": "1.0.0",
        "skills": [{"id": "echo", "name": "Echo", "tags": ["demo"]}]
    })
}

fn signer_for(agent: &str, seed: u8) -> Ed25519Signer {
    Ed25519Signer::from_seed(&AgentId::parse(agent).expect("agent id"), &[seed; 32])
}

/// Trust file text with one key block per `(agent, role, signer)`.
fn trust_of(keys: &[(&str, &str, &Ed25519Signer)]) -> TrustStore {
    let mut text = String::from("schema = \"a2abus.trust/1\"\n");
    for (agent, role, signer) in keys {
        text.push_str(&format!(
            "[[key]]\nkid = \"{}\"\nagent = \"{agent}\"\nrole = \"{role}\"\ntenants = [\"{TENANT}\"]\npublic_key = \"{}\"\n",
            signer.kid(),
            encode_public_key(&signer.public_key()),
        ));
    }
    TrustStore::parse(&text).expect("trust loads")
}

/// The demo trust file: the operator key only, no agent signing key.
fn operator_only() -> TrustStore {
    trust_of(&[("operator", "operator", &signer_for("operator", 9))])
}

/// A trust store that pins the key `worker-a` signs with.
fn worker_trust() -> TrustStore {
    trust_of(&[("worker-a", "agent", &signer_for("worker-a", 3))])
}

async fn publish(fx: &Fixture, card: &Value) {
    fx.kv_put(
        "cards",
        "worker-a",
        &serde_json::to_vec(card).expect("json"),
    )
    .await;
    let presence = json!({
        "agentId": "worker-a", "tenant": TENANT, "instanceId": "i-1",
        "cardVersion": "1.0.0", "state": "PRESENCE_STATE_READY", "activeTasks": 0
    });
    fx.kv_put(
        "presence",
        "worker-a",
        &serde_json::to_vec(&presence).expect("json"),
    )
    .await;
}

fn signed(trust: &TrustStore, signer: &Ed25519Signer) -> Value {
    let mut card = card();
    sign_card(&mut card, signer, trust, &ManualClock::new(NOW_MS as i64)).expect("signed");
    card
}

/// Runs `a2a discover` over the fixture bus with `trust` as the CLI's loaded trust file.
async fn discover_output(fx: &Fixture, trust: Option<TrustStore>) -> String {
    let session = app::build_session(
        Shared(fx.bus.clone()),
        &fx.settings(Vec::new()),
        None,
        trust.map(Arc::new),
    )
    .expect("session");
    let mut streams = Streams::new("");
    {
        let mut io = streams.io(false, false);
        commands::discover(&session, &args(), &mut io)
            .await
            .expect("discover");
    }
    streams.out_text()
}

// [0007-053] [0003-042] An unsigned card is listed, marked unverified, when the trust file holds
// no agent signing key (the M1 default: the operator key only, or no trust file at all).
#[tokio::test]
async fn req_0007_053_no_signer_trust_lists_unsigned_card_unverified() {
    let fx = Fixture::new();
    publish(&fx, &card()).await;

    let expected = "worker-a  v1.0.0  ready  unverified\n";
    assert_eq!(discover_output(&fx, None).await, expected, "no trust file");
    assert_eq!(
        discover_output(&fx, Some(TrustStore::default())).await,
        expected,
        "empty trust store"
    );
    assert_eq!(
        discover_output(&fx, Some(operator_only())).await,
        expected,
        "operator key only"
    );
}

// [0003-042] [0007-050] With a signer key trusted, an unsigned card is not accepted.
#[tokio::test]
async fn req_0003_042_signer_trust_drops_unsigned_card() {
    let fx = Fixture::new();
    publish(&fx, &card()).await;
    assert_eq!(discover_output(&fx, Some(worker_trust())).await, "");
}

// [0007-053] With a signer key trusted, a card signed by that key is listed as verified.
#[tokio::test]
async fn req_0007_053_signer_trust_lists_validly_signed_card_verified() {
    let fx = Fixture::new();
    let trust = worker_trust();
    publish(&fx, &signed(&trust, &signer_for("worker-a", 3))).await;
    assert_eq!(
        discover_output(&fx, Some(trust)).await,
        "worker-a  v1.0.0  ready  verified\n"
    );
}

// [0007-053] [0003-042] A tampered card, and a card signed by another agent's key, are dropped
// when a signer key is trusted.
#[tokio::test]
async fn req_0003_042_signer_trust_drops_tampered_and_misattributed_cards() {
    let fx = Fixture::new();
    let trust = worker_trust();

    let mut tampered = signed(&trust, &signer_for("worker-a", 3));
    tampered["name"] = json!("Worker B");
    publish(&fx, &tampered).await;
    assert_eq!(discover_output(&fx, Some(trust)).await, "", "tampered");

    let both = trust_of(&[
        ("worker-a", "agent", &signer_for("worker-a", 3)),
        ("other", "agent", &signer_for("other", 5)),
    ]);
    publish(&fx, &signed(&both, &signer_for("other", 5))).await;
    assert_eq!(
        discover_output(&fx, Some(both)).await,
        "",
        "signed by another agent's key"
    );
}

// [0007-053] Only an `agent`-role key counts as a card signer.
#[test]
fn req_0007_053_card_trust_requires_an_agent_role_key() {
    assert!(card_trust(None).is_none());
    assert!(card_trust(Some(Arc::new(TrustStore::default()))).is_none());
    assert!(card_trust(Some(Arc::new(operator_only()))).is_none());
    assert!(card_trust(Some(Arc::new(worker_trust()))).is_some());
}

// [0012-054] `a2a discover` runs without a filter and lists every card; two filters conflict.
#[test]
fn req_0012_054_discover_filter_is_optional_and_exclusive() {
    use a2a_cli::cli::{Cli, Command};
    use clap::Parser;

    let bare = Cli::try_parse_from(["a2a", "discover"]).expect("bare discover parses");
    let Command::Discover(parsed) = bare.command else {
        panic!("expected discover");
    };
    assert!(commands::discover_query(&parsed).expect("query").is_none());

    assert!(Cli::try_parse_from(["a2a", "discover", "--skill", "echo"]).is_ok());
    assert!(Cli::try_parse_from(["a2a", "discover", "--skill", "a", "--tag", "b"]).is_err());
}
