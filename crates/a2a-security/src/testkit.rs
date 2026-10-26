//! Shared fixtures for unit tests: seeded agent keys and trust-file text. Compiled for tests only.

use a2a_types::ids::AgentId;

use crate::identity::{Ed25519Signer, Signer, encode_public_key};
use crate::trust::TrustStore;

pub(crate) fn agent(name: &str) -> AgentId {
    AgentId::parse(name).expect("valid agent id")
}

/// Deterministic signer for `name` derived from `seed`.
pub(crate) fn signer(name: &str, seed: u8) -> Ed25519Signer {
    Ed25519Signer::from_seed(&agent(name), &[seed; 32])
}

/// One `[[key]]` block with role `agent` and the given extra lines.
pub(crate) fn key_block(name: &str, seed: u8, extra: &str) -> String {
    key_block_with_role(name, seed, "agent", extra)
}

/// One `[[key]]` block with an explicit role and the given extra lines.
pub(crate) fn key_block_with_role(name: &str, seed: u8, role: &str, extra: &str) -> String {
    let signer = signer(name, seed);
    let kid = signer.kid().to_string();
    let pk = encode_public_key(&signer.public_key());
    format!(
        "[[key]]\nkid = \"{kid}\"\nagent = \"{name}\"\nrole = \"{role}\"\npublic_key = \"{pk}\"\n{extra}\n"
    )
}

/// Wraps key blocks in a file with the required schema line.
pub(crate) fn file(body: &str) -> String {
    format!("schema = \"a2abus.trust/1\"\n{body}")
}

/// A validated store containing the given key blocks.
pub(crate) fn store(body: &str) -> TrustStore {
    TrustStore::parse(&file(body)).expect("fixture trust file loads")
}
