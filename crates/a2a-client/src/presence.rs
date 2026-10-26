//! Presence records (RFC-0004 0004-008 to 0004-011).
//!
//! The record has no protobuf schema in M1, so it is read as proto-JSON with camelCase names.

use serde::Deserialize;

use crate::error::ClientError;

/// Largest presence record a reader accepts (RFC-0004 0004-009).
pub const MAX_PRESENCE_BYTES: usize = 1024;

/// Lifecycle state of a worker instance (RFC-0004 0004-008).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresenceState {
    /// Started but not yet serving.
    Starting,
    /// Serving and routable (RFC-0004 0004-014).
    Ready,
    /// Finishing existing work; not routable.
    Draining,
}

/// A live presence record for one agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Presence {
    /// Agent identifier; equals the KV key.
    pub agent_id: String,
    /// Worker instance that wrote the record (RFC-0004 0004-018).
    pub instance_id: String,
    /// Card version the instance serves.
    pub card_version: String,
    /// Lifecycle state.
    pub state: PresenceState,
    /// Number of active tasks.
    pub active_tasks: u32,
}

impl Presence {
    /// Parses a record stored under `key` in `tenant`. Oversized, malformed, or mismatched
    /// records are refused (RFC-0004 0004-009).
    pub fn parse(key: &str, tenant: &str, value: &[u8]) -> Result<Self, ClientError> {
        if value.len() > MAX_PRESENCE_BYTES {
            return Err(ClientError::Discovery("presence record too large"));
        }
        let wire: Wire = serde_json::from_slice(value)
            .map_err(|_| ClientError::Discovery("presence record malformed"))?;
        if wire.agent_id != key || wire.tenant != tenant {
            return Err(ClientError::Discovery("presence identity mismatch"));
        }
        let state = match wire.state.as_str() {
            "PRESENCE_STATE_STARTING" => PresenceState::Starting,
            "PRESENCE_STATE_READY" => PresenceState::Ready,
            "PRESENCE_STATE_DRAINING" => PresenceState::Draining,
            _ => return Err(ClientError::Discovery("presence state unknown")),
        };
        Ok(Self {
            agent_id: wire.agent_id,
            instance_id: wire.instance_id,
            card_version: wire.card_version,
            state,
            active_tasks: wire.active_tasks,
        })
    }

    /// Returns whether the agent is READY (RFC-0004 0004-014).
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.state == PresenceState::Ready
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Wire {
    agent_id: String,
    tenant: String,
    instance_id: String,
    card_version: String,
    state: String,
    #[serde(default)]
    active_tasks: u32,
}
