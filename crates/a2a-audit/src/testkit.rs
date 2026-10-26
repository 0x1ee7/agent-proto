//! Test doubles for checkpoint signing. The digest is `sha256(key_id || 0x00 || message)`, so a
//! signature is bound to its key id and to the exact message. Production callers supply a real
//! signer (Ed25519 with a supervisor-held key); these types exist so the chain and verifier can
//! be exercised without key material.

use sha2::{Digest, Sha256};

use crate::chain::{CheckpointSigner, CheckpointVerifier};

fn digest(key_id: &str, message: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(key_id.as_bytes());
    hasher.update([0u8]);
    hasher.update(message);
    hasher.finalize().to_vec()
}

/// Signs with the digest construction, under a fixed key id.
#[derive(Debug, Clone)]
pub struct DigestSigner {
    key_id: String,
}

impl DigestSigner {
    /// Creates a signer for `key_id`.
    pub fn new(key_id: &str) -> Self {
        Self {
            key_id: key_id.to_owned(),
        }
    }
}

impl CheckpointSigner for DigestSigner {
    fn key_id(&self) -> &str {
        &self.key_id
    }

    fn sign(&self, message: &[u8]) -> Vec<u8> {
        digest(&self.key_id, message)
    }
}

/// Verifies digest signatures for exactly one key id. Other key ids never verify.
#[derive(Debug, Clone)]
pub struct DigestVerifier {
    key_id: String,
}

impl DigestVerifier {
    /// Creates a verifier that accepts only `key_id`.
    pub fn new(key_id: &str) -> Self {
        Self {
            key_id: key_id.to_owned(),
        }
    }
}

impl CheckpointVerifier for DigestVerifier {
    fn verify(&self, key_id: &str, message: &[u8], signature: &[u8]) -> bool {
        key_id == self.key_id && digest(key_id, message) == signature
    }
}
