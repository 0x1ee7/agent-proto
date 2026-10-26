//! Health and readiness responses (RFC-0013 0013-033, 0013-036, 0013-037).
//!
//! The crate builds the status code and JSON body; the supervisor owns the listener. The body
//! names only states: no paths, config values, or credential names (0013-037).

use std::collections::BTreeMap;
use std::net::IpAddr;

use serde_json::{Value, json};

/// Largest spool fill ratio at which the process is still ready (0013-036: spool at most 80%).
pub const READY_MAX_SPOOL_RATIO: f64 = 0.8;

/// Process state reported on the health endpoints.
#[derive(Debug, Clone, PartialEq)]
pub struct Health {
    /// Whether the bus connection is up.
    pub bus_connected: bool,
    /// Whether the sink is accepting writes.
    pub sink_writing: bool,
    /// Spool fill ratio in `[0, 1]`.
    pub spool_ratio: f64,
    /// Agent name to state label (for example `running`), for the body's `agents` map.
    pub agents: BTreeMap<String, String>,
}

/// Status code and JSON body for one response.
#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    /// HTTP status code.
    pub status: u16,
    /// JSON body, always an object of the same shape (0013-037).
    pub body: String,
}

/// Errors from the loopback guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HealthError {
    /// The address is not a loopback address (0013-033).
    #[error("metrics and health listener must bind a loopback address, not {0}")]
    NonLoopback(IpAddr),
}

/// Refuses any address that is not loopback (0013-033).
pub fn require_loopback(address: IpAddr) -> Result<(), HealthError> {
    if address.is_loopback() {
        Ok(())
    } else {
        Err(HealthError::NonLoopback(address))
    }
}

/// Whether the process is ready: bus connected, sink writable, spool at most 80% full (0013-036).
pub fn is_ready(health: &Health) -> bool {
    health.bus_connected && health.sink_writing && health.spool_ratio <= READY_MAX_SPOOL_RATIO
}

/// `/healthz`: 200 while the process loop runs, regardless of dependency state (0013-036).
pub fn healthz(health: &Health) -> Response {
    Response {
        status: 200,
        body: body(health),
    }
}

/// `/readyz`: 200 only when ready, otherwise 503 (0013-036).
pub fn readyz(health: &Health) -> Response {
    Response {
        status: if is_ready(health) { 200 } else { 503 },
        body: body(health),
    }
}

fn body(health: &Health) -> String {
    let status = if is_ready(health) {
        "ok"
    } else {
        "unavailable"
    };
    let bus = if health.bus_connected {
        "connected"
    } else {
        "disconnected"
    };
    let sink = if health.sink_writing {
        "writing"
    } else {
        "unavailable"
    };
    let agents: Value = health
        .agents
        .iter()
        .map(|(name, state)| (name.clone(), Value::String(state.clone())))
        .collect::<serde_json::Map<String, Value>>()
        .into();
    json!({
        "status": status,
        "bus": bus,
        "sink": sink,
        "agents": agents,
    })
    .to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;

    fn healthy() -> Health {
        Health {
            bus_connected: true,
            sink_writing: true,
            spool_ratio: 0.0,
            agents: BTreeMap::from([("agent-a".to_owned(), "running".to_owned())]),
        }
    }

    #[test]
    fn req_0013_033_loopback_addresses_are_accepted() {
        assert!(require_loopback(IpAddr::V4(Ipv4Addr::LOCALHOST)).is_ok());
        assert!(require_loopback(IpAddr::V6(Ipv6Addr::LOCALHOST)).is_ok());
    }

    #[test]
    fn req_0013_033_non_loopback_addresses_are_refused() {
        let wildcard = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
        assert_eq!(
            require_loopback(wildcard),
            Err(HealthError::NonLoopback(wildcard))
        );
        let lan = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10));
        assert_eq!(require_loopback(lan), Err(HealthError::NonLoopback(lan)));
        let v6 = IpAddr::V6(Ipv6Addr::UNSPECIFIED);
        assert!(require_loopback(v6).is_err());
    }

    #[test]
    fn req_0013_036_readyz_is_200_when_all_dependencies_are_good() {
        assert_eq!(readyz(&healthy()).status, 200);
    }

    #[test]
    fn req_0013_036_readyz_is_503_when_bus_disconnected() {
        let mut health = healthy();
        health.bus_connected = false;
        assert_eq!(readyz(&health).status, 503);
    }

    #[test]
    fn req_0013_036_readyz_is_503_when_sink_not_writable() {
        let mut health = healthy();
        health.sink_writing = false;
        assert_eq!(readyz(&health).status, 503);
    }

    #[test]
    fn req_0013_036_readyz_spool_boundary_is_80_percent() {
        let mut health = healthy();
        health.spool_ratio = 0.8;
        assert_eq!(readyz(&health).status, 200);
        health.spool_ratio = 0.81;
        assert_eq!(readyz(&health).status, 503);
    }

    #[test]
    fn req_0013_036_healthz_is_200_even_when_unready() {
        let health = Health {
            bus_connected: false,
            sink_writing: false,
            spool_ratio: 1.0,
            agents: BTreeMap::new(),
        };
        assert_eq!(healthz(&health).status, 200);
        assert_eq!(readyz(&health).status, 503);
    }

    #[test]
    fn req_0013_037_body_has_fixed_fields_and_states_only() {
        let response = readyz(&healthy());
        let value: Value = serde_json::from_str(&response.body).unwrap();
        let object = value.as_object().unwrap();
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["agents", "bus", "sink", "status"]);
        assert_eq!(value["status"], "ok");
        assert_eq!(value["bus"], "connected");
        assert_eq!(value["sink"], "writing");
        assert_eq!(value["agents"]["agent-a"], "running");
    }

    #[test]
    fn req_0013_037_unready_body_reports_states() {
        let mut health = healthy();
        health.bus_connected = false;
        let value: Value = serde_json::from_str(&readyz(&health).body).unwrap();
        assert_eq!(value["status"], "unavailable");
        assert_eq!(value["bus"], "disconnected");
        assert!(!readyz(&health).body.contains('/'));
    }
}
