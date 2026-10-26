//! Derived HTTP agent cards and the card cache fed by bus refresh (RFC-0003, RFC-0008 4.2, 4.3).

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use a2a_types::ids::AgentId;
use bytes::Bytes;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::config::GatewayConfig;

/// Protocol binding advertised for every derived card (0008-014).
const BINDING: &str = "JSONRPC";
/// Protocol version advertised for every derived card (0008-014).
const PROTOCOL_VERSION: &str = "1.0";
/// Name of the single HTTP bearer security scheme (0008-013).
pub const BEARER_SCHEME: &str = "bearer";

/// Card fields copied from the bus card; everything else is dropped (0008-015, 0008-016).
const COPIED_FIELDS: [&str; 9] = [
    "name",
    "description",
    "version",
    "provider",
    "documentationUrl",
    "iconUrl",
    "defaultOutputModes",
    "skills",
    "defaultInputModes",
];

/// One derived, serveable card and the facts the request path needs without re-parsing it.
#[derive(Debug)]
pub struct CardView {
    /// Serialized card body.
    pub body: Bytes,
    /// Strong ETag of the body (quoted SHA-256 hex).
    pub etag: String,
    /// Whether the bus card declares `extendedAgentCard` true (0008-019).
    pub extended: bool,
    /// Extension URIs the card marks `required` (RFC-0003 0003-018).
    pub required_extensions: Vec<String>,
    /// Every extension URI the card advertises (echoed in the response `A2A-Extensions` header).
    pub extensions: Vec<String>,
}

/// Outcome of a card lookup.
#[derive(Debug, Clone)]
pub enum Lookup {
    /// No snapshot has been built yet; the caller answers 503 (0008-018).
    NotReady,
    /// The agent is not exposed or has no bus card (0008-017).
    Missing,
    /// The derived card.
    Found(Arc<CardView>),
}

/// Immutable snapshot of every exposed agent's card.
pub type Snapshot = HashMap<String, Arc<CardView>>;

/// Card cache: the last complete snapshot, replaced atomically on refresh (0008-018, 0008-080).
#[derive(Debug, Default)]
pub struct Cards {
    snapshot: RwLock<Option<Arc<Snapshot>>>,
}

impl Cards {
    /// Creates an empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the snapshot.
    pub fn replace(&self, snapshot: Snapshot) {
        let mut slot = match self.snapshot.write() {
            Ok(slot) => slot,
            Err(poisoned) => poisoned.into_inner(),
        };
        *slot = Some(Arc::new(snapshot));
    }

    /// Looks up one agent's card.
    #[must_use]
    pub fn lookup(&self, agent: &str) -> Lookup {
        let current = match self.snapshot.read() {
            Ok(slot) => slot.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        match current {
            None => Lookup::NotReady,
            Some(snapshot) => snapshot
                .get(agent)
                .map_or(Lookup::Missing, |view| Lookup::Found(Arc::clone(view))),
        }
    }
}

/// Derives the HTTP card for one exposed agent from its bus card (RFC-0003, 0008-013..016).
///
/// `serves_peers` is true when at least one peer may reach the agent; only then does the card
/// carry the bearer security requirement.
#[must_use]
pub fn derive(
    bus: &Value,
    agent: &AgentId,
    public_base_url: &str,
    config: &GatewayConfig,
    serves_peers: bool,
) -> Value {
    let mut card = Map::new();
    for field in COPIED_FIELDS {
        if let Some(value) = bus.get(field) {
            card.insert(field.to_owned(), value.clone());
        }
    }
    let bus_capabilities = bus.get("capabilities");
    let extended = bus_capabilities
        .and_then(|caps| caps.get("extendedAgentCard"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut capabilities = Map::new();
    capabilities.insert("streaming".to_owned(), Value::Bool(true));
    capabilities.insert(
        "pushNotifications".to_owned(),
        Value::Bool(config.push_notifications),
    );
    capabilities.insert("extendedAgentCard".to_owned(), Value::Bool(extended));
    if let Some(extensions) = bus_capabilities.and_then(|caps| caps.get("extensions")) {
        capabilities.insert("extensions".to_owned(), extensions.clone());
    }
    card.insert("capabilities".to_owned(), Value::Object(capabilities));
    card.insert(
        "securitySchemes".to_owned(),
        json!({BEARER_SCHEME: {"httpAuthSecurityScheme": {"scheme": "Bearer"}}}),
    );
    if serves_peers {
        card.insert(
            "securityRequirements".to_owned(),
            json!([{"schemes": {BEARER_SCHEME: {"list": []}}}]),
        );
    }
    card.insert(
        "supportedInterfaces".to_owned(),
        json!([interface(public_base_url, agent)]),
    );
    Value::Object(card)
}

/// The single canonical interface entry (0008-014).
#[must_use]
pub fn interface(public_base_url: &str, agent: &AgentId) -> Value {
    json!({
        "url": format!("{}/agents/{}", public_base_url.trim_end_matches('/'), agent.as_str()),
        "protocolBinding": BINDING,
        "protocolVersion": PROTOCOL_VERSION,
    })
}

/// Builds the serveable view of a derived card.
#[must_use]
pub fn view(card: &Value) -> CardView {
    let body = serde_json::to_vec(card).unwrap_or_default();
    let digest = Sha256::digest(&body);
    let etag = format!("\"{}\"", hex(&digest));
    let extended = card
        .get("capabilities")
        .and_then(|caps| caps.get("extendedAgentCard"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let advertised: &[Value] = card
        .get("capabilities")
        .and_then(|caps| caps.get("extensions"))
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice);
    let extensions = advertised
        .iter()
        .filter_map(|ext| ext.get("uri").and_then(Value::as_str))
        .map(str::to_owned)
        .collect();
    let required_extensions = advertised
        .iter()
        .filter(|ext| ext.get("required").and_then(Value::as_bool) == Some(true))
        .filter_map(|ext| ext.get("uri").and_then(Value::as_str))
        .map(str::to_owned)
        .collect();
    CardView {
        body: Bytes::from(body),
        etag,
        extended,
        required_extensions,
        extensions,
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}
