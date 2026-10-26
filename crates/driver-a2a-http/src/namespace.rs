//! Per-origin contextId namespacing and request identifiers (RFC-0008 0008-097; RFC-0009 0009-045, 0009-078).
//!
//! The peer contextId is a pure function of the bus origin and the bus contextId,
//! so a follow-up to an INPUT_REQUIRED turn on the same bus context reuses it
//! without any stored state (0008-096). It is never derived from a taskId.

use sha2::{Digest, Sha256};

/// Returns the peer contextId for `origin` (bus agent id) and `bus_context`.
pub fn peer_context_id(origin: &str, bus_context: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(origin.as_bytes());
    hasher.update([0u8]);
    hasher.update(bus_context.as_bytes());
    format!("ctx-{}", hex(hasher.finalize().as_slice()))
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

    /// 0008-097: the same bus context under two origins maps to different peer contexts.
    #[test]
    fn req_0008_097_context_is_namespaced_per_origin() {
        let a = peer_context_id("origin-a", "ctx-bus-1");
        let b = peer_context_id("origin-b", "ctx-bus-1");
        assert_ne!(a, b);
        assert_eq!(a, peer_context_id("origin-a", "ctx-bus-1"));
    }

    /// 0008-096, 0009-078: the peer context is stable for the same bus context (follow-up reuse).
    #[test]
    fn req_0009_078_follow_up_reuses_context() {
        assert_eq!(
            peer_context_id("hermes-peer", "bus-ctx"),
            peer_context_id("hermes-peer", "bus-ctx")
        );
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
