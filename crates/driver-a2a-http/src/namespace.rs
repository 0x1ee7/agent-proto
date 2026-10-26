//! Per-origin contextId namespacing and request identifiers (RFC-0008 0008-097; RFC-0009 0009-076,
//! 0009-045, 0009-078).
//!
//! The peer contextId is a keyed HMAC-SHA256 over the bus origin and the bus contextId. Two
//! origins that reuse one bus contextId therefore never share a peer context (0009-076), and the
//! raw bus contextId is never sent. The key is a per-process random value unless the operator
//! configures a secret, in which case the mapping is stable across restarts (0008-096). The peer
//! contextId is never derived from a taskId.

use sha2::{Digest, Sha256};

/// SHA-256 block size in bytes, the HMAC key padding width.
const BLOCK_LEN: usize = 64;

/// Returns the peer contextId for `origin` and `bus_context` under `key`.
///
/// The inputs are length-prefixed before MACing so that no pair of origin and context can
/// collide with another pair.
pub fn peer_context_id(key: &[u8], origin: &str, bus_context: &str) -> String {
    let mut message = Vec::with_capacity(16 + origin.len() + bus_context.len());
    message.extend_from_slice(
        &u64::try_from(origin.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    message.extend_from_slice(origin.as_bytes());
    message.extend_from_slice(
        &u64::try_from(bus_context.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    message.extend_from_slice(bus_context.as_bytes());
    format!("ctx-{}", hex(&hmac_sha256(key, &message)))
}

/// HMAC-SHA256 (RFC 2104) built on the sha2 crate.
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; BLOCK_LEN];
    if key.len() > BLOCK_LEN {
        block[..32].copy_from_slice(Sha256::digest(key).as_slice());
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(block.map(|b| b ^ 0x36));
    inner.update(message);
    let inner_digest = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(block.map(|b| b ^ 0x5c));
    outer.update(inner_digest.as_slice());
    let mut out = [0u8; 32];
    out.copy_from_slice(outer.finalize().as_slice());
    out
}

/// Generates a fresh per-process namespacing key: 64 hex characters from two random UUIDs.
pub fn random_key() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// Returns a fresh 32-character lowercase hex `messageId` (0009-045).
pub fn message_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Returns a JSON-RPC request id of the form `task-<16 hex>`, as Hermes clients send.
pub fn request_id() -> String {
    let full = uuid::Uuid::new_v4().simple().to_string();
    format!("task-{}", &full[..16])
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// RFC 4231 test case 1 (20-byte key, short data).
    #[test]
    fn hmac_matches_rfc4231_case1() {
        let mac = hmac_sha256(&[0x0b; 20], b"Hi There");
        assert_eq!(
            hex(&mac),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    /// RFC 4231 test case 6 (key longer than the block size is hashed first).
    #[test]
    fn hmac_matches_rfc4231_case6_long_key() {
        let mac = hmac_sha256(
            &[0xaa; 131],
            b"Test Using Larger Than Block-Size Key - Hash Key First",
        );
        assert_eq!(
            hex(&mac),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    /// 0009-076: the same bus context under two origins maps to different peer contexts.
    #[test]
    fn req_0009_076_context_is_namespaced_per_origin() {
        let key = b"fixed-test-key";
        let a = peer_context_id(key, "origin-a", "ctx-bus-1");
        let b = peer_context_id(key, "origin-b", "ctx-bus-1");
        assert_ne!(a, b);
        assert_ne!(a, "ctx-bus-1");
        assert_eq!(a, peer_context_id(key, "origin-a", "ctx-bus-1"));
    }

    /// 0009-076: the mapping depends on the key, so a different process key gives other contexts.
    #[test]
    fn req_0009_076_context_depends_on_key() {
        let a = peer_context_id(b"key-one", "origin-a", "ctx-bus-1");
        let b = peer_context_id(b"key-two", "origin-a", "ctx-bus-1");
        assert_ne!(a, b);
    }

    /// 0008-096: a configured key keeps the mapping stable (follow-up reuse across restarts).
    #[test]
    fn req_0008_096_configured_key_is_stable() {
        assert_eq!(
            peer_context_id(b"configured", "hermes-peer", "bus-ctx"),
            peer_context_id(b"configured", "hermes-peer", "bus-ctx")
        );
    }

    /// Length prefixes keep distinct (origin, context) pairs apart.
    #[test]
    fn origin_and_context_boundaries_do_not_collide() {
        assert_ne!(
            peer_context_id(b"k", "ab", "c"),
            peer_context_id(b"k", "a", "bc")
        );
    }

    #[test]
    fn random_key_is_64_hex_and_fresh() {
        let a = random_key();
        assert_eq!(a.len(), 64);
        assert_ne!(a, random_key());
    }

    /// messageId is 32 lowercase hex characters, the Hermes client shape.
    #[test]
    fn message_id_is_32_hex() {
        let id = message_id();
        assert_eq!(id.len(), 32);
        assert!(
            id.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
    }

    #[test]
    fn request_id_has_task_prefix() {
        let id = request_id();
        assert!(id.starts_with("task-") && id.len() == 21, "{id}");
    }
}
