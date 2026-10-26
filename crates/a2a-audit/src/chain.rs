//! Hash chain and checkpoint signing seams (RFC-0013 0013-026, 0013-027).
//!
//! `chain_hash` is the lowercase hex SHA-256 of the JCS form of the event with `chainHash` and
//! `signature` removed. A checkpoint signs that same preimage, so the signature covers the
//! event's content, its `prevHash` and its `keyId`.

use std::fmt::Debug;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::Value;

use crate::canon::{CanonError, canonical_bytes, sha256_hex};

/// `prevHash` of the first event of an emitter (64 zero digits).
pub const GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// Canonical bytes hashed and signed for an event: JCS without `chainHash` and `signature`.
pub fn preimage(event: &Value) -> Result<Vec<u8>, CanonError> {
    let mut copy = event.clone();
    if let Value::Object(map) = &mut copy {
        map.remove("chainHash");
        map.remove("signature");
    }
    canonical_bytes(&copy)
}

/// `chainHash` for a preimage.
pub fn chain_hash(preimage: &[u8]) -> String {
    sha256_hex(preimage)
}

/// Encodes signature bytes as unpadded base64url (RFC-0013 0013-027).
pub fn encode_signature(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Decodes unpadded base64url signature text.
pub fn decode_signature(text: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(text).ok()
}

/// Signs checkpoint preimages with the supervisor key. Production implementations come from
/// `a2a-security` through the RFC-0007 `Signer` seam.
pub trait CheckpointSigner: Send + Sync + Debug {
    /// Identifier written to `keyId`, formatted `<agent-id>:<K>` per RFC-0007.
    fn key_id(&self) -> &str;
    /// Signs `message` and returns the raw signature bytes.
    fn sign(&self, message: &[u8]) -> Vec<u8>;
}

/// Verifies checkpoint signatures for `verify`. Returns `false` for unknown keys.
pub trait CheckpointVerifier: Send + Sync + Debug {
    fn verify(&self, key_id: &str, message: &[u8], signature: &[u8]) -> bool;
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn genesis_is_64_hex_zeros() {
        assert_eq!(GENESIS_HASH.len(), 64);
        assert!(GENESIS_HASH.bytes().all(|b| b == b'0'));
    }

    #[test]
    fn preimage_excludes_chain_hash_and_signature() {
        let with = json!({"seq": "1", "chainHash": "aa", "signature": "bb"});
        let without = json!({"seq": "1"});
        assert_eq!(preimage(&with).ok(), preimage(&without).ok());
    }

    #[test]
    fn signature_round_trips_unpadded() {
        let text = encode_signature(&[1, 2, 3, 250]);
        assert!(!text.contains('='));
        assert_eq!(decode_signature(&text), Some(vec![1, 2, 3, 250]));
    }
}
