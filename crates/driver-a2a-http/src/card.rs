//! Peer card ingestion and sanitization (RFC-0009 0009-041; RFC-0008 0008-066..073, 0008-099).
//!
//! The card is advisory. The configured base URL and bus agent id are authoritative;
//! the card only chooses the JSON-RPC endpoint and the streaming capability.

use std::time::Duration;

use a2a_types::vocab::{A2A_VERSION, ext};
use serde_json::{Value, json};
use url::Url;

/// Well-known card path (A2A 1.0).
pub const CARD_PATH: &str = "/.well-known/agent-card.json";
/// Legacy card path (pre-1.0 Hermes serves it).
pub const LEGACY_CARD_PATH: &str = "/.well-known/agent.json";

/// Card fetch failure. Never fatal for the driver (0009-041).
#[derive(Debug, thiserror::Error)]
pub enum CardError {
    /// Neither card path exists.
    #[error("no agent card at either well-known path")]
    NotFound,
    /// A card path returned a non-success status.
    #[error("card fetch returned HTTP {0}")]
    Status(u16),
    /// The card could not be reached.
    #[error("card fetch failed to reach the peer")]
    Unreachable,
    /// The card body is not a JSON object.
    #[error("card is not a JSON object")]
    Malformed,
}

/// Fetches the card from the origin root, trying `agent-card.json` then `agent.json` on 404.
///
/// # Errors
/// Returns [`CardError`] when no card can be read. The caller falls back to the configured URL.
pub async fn fetch(
    http: &reqwest::Client,
    base: &Url,
    timeout: Duration,
) -> Result<Value, CardError> {
    for path in [CARD_PATH, LEGACY_CARD_PATH] {
        let url = base.join(path).map_err(|_| CardError::Malformed)?;
        let response = http
            .get(url)
            .timeout(timeout)
            .send()
            .await
            .map_err(|_| CardError::Unreachable)?;
        let status = response.status();
        if status.as_u16() == 404 {
            continue;
        }
        if !status.is_success() {
            return Err(CardError::Status(status.as_u16()));
        }
        let bytes = response.bytes().await.map_err(|_| CardError::Unreachable)?;
        return match serde_json::from_slice::<Value>(&bytes) {
            Ok(value @ Value::Object(_)) => Ok(value),
            _ => Err(CardError::Malformed),
        };
    }
    Err(CardError::NotFound)
}

/// Endpoint chosen from a card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// JSON-RPC endpoint.
    pub rpc_url: Url,
    /// Card advertises `capabilities.streaming`.
    pub streaming: bool,
    /// No JSONRPC interface with protocolVersion 1.0 was published (0008-071).
    pub version_mismatch: bool,
}

/// Chooses the JSON-RPC endpoint: a JSONRPC 1.0 interface, else any JSONRPC interface, else the card `url`,
/// else the configured base URL. Relative URLs resolve against `base`.
pub fn select(card: &Value, base: &Url) -> Selection {
    let interfaces: Vec<&Value> = card
        .get("supportedInterfaces")
        .and_then(Value::as_array)
        .map(|items| items.iter().collect())
        .unwrap_or_default();
    let jsonrpc: Vec<&Value> = interfaces
        .into_iter()
        .filter(|i| str_field(i, "protocolBinding").eq_ignore_ascii_case("JSONRPC"))
        .collect();
    let exact = jsonrpc
        .iter()
        .find(|i| str_field(i, "protocolVersion") == A2A_VERSION);
    let (candidate, version_mismatch) = match (exact, jsonrpc.first()) {
        (Some(interface), _) => (Some(str_field(interface, "url")), false),
        (None, Some(interface)) => (Some(str_field(interface, "url")), true),
        (None, None) => (
            card.get("url").and_then(Value::as_str).map(str::to_owned),
            true,
        ),
    };
    let rpc_url = candidate
        .filter(|text| !text.is_empty())
        .and_then(|text| base.join(&text).ok())
        .unwrap_or_else(|| base.clone());
    let streaming = card
        .get("capabilities")
        .and_then(|c| c.get("streaming"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Selection {
        rpc_url,
        streaming,
        version_mismatch,
    }
}

/// Builds the bus-facing card for a peer (RFC-0008 0008-073, 0008-099).
///
/// Built only from the bus agent id and fixed text. No external URL, security
/// schemes, tool-name skills, tags, or peer description text are copied.
pub fn sanitize(agent_id: &str) -> Value {
    json!({
        "name": agent_id,
        "description": "External A2A peer reached through driver-a2a-http",
        "version": A2A_VERSION,
        "defaultInputModes": ["text/plain"],
        "defaultOutputModes": ["text/plain"],
        "capabilities": {
            "streaming": false,
            "extendedAgentCard": false,
            "extensions": [{
                "uri": ext::PROVENANCE,
                "params": {"source": "external-a2a-http", "verified": false}
            }]
        },
        "skills": [{
            "id": "peer-task",
            "name": "Peer task",
            "description": "Free-form text task forwarded to the external peer"
        }]
    })
}

fn str_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("http://127.0.0.1:9911/").unwrap()
    }

    /// 0009-041: the JSONRPC 1.0 interface is chosen and streaming is read from the card.
    #[test]
    fn req_0009_041_selects_jsonrpc_one_zero_interface() {
        let card = json!({
            "supportedInterfaces": [
                {"url": "http://127.0.0.1:9911/grpc", "protocolBinding": "GRPC", "protocolVersion": "1.0"},
                {"url": "http://127.0.0.1:9911/", "protocolBinding": "JSONRPC", "protocolVersion": "1.0"}
            ],
            "capabilities": {"streaming": true}
        });
        let selection = select(&card, &base());
        assert_eq!(selection.rpc_url.as_str(), "http://127.0.0.1:9911/");
        assert!(selection.streaming);
        assert!(!selection.version_mismatch);
    }

    /// 0008-071: no 1.0 JSONRPC interface records a version mismatch but still selects an endpoint.
    #[test]
    fn req_0008_071_missing_one_zero_interface_is_a_mismatch() {
        let card = json!({"supportedInterfaces": [
            {"url": "http://127.0.0.1:9911/", "protocolBinding": "JSONRPC", "protocolVersion": "0.3"}
        ]});
        let selection = select(&card, &base());
        assert!(selection.version_mismatch);
        assert!(!selection.streaming);
    }

    #[test]
    fn missing_interfaces_fall_back_to_base_url() {
        let selection = select(&json!({}), &base());
        assert_eq!(selection.rpc_url, base());
        assert!(selection.version_mismatch);
    }

    /// 0008-073, 0008-099: the sanitized card carries no external URL, security, or tool skills.
    #[test]
    fn req_0008_099_sanitized_card_drops_external_detail() {
        let card = sanitize("hermes-peer");
        let text = card.to_string();
        assert!(!text.contains("supportedInterfaces"));
        assert!(!text.contains("127.0.0.1"));
        assert!(!text.contains("security"));
        assert!(text.contains("external-a2a-http"));
        assert!(text.contains("\"verified\":false"));
        assert_eq!(card["name"], "hermes-peer");
        assert_eq!(card["skills"].as_array().map(Vec::len), Some(1));
    }
}
