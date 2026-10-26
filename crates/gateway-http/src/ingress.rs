//! Message ingress and egress rules on the JSON form of requests and results (RFC-0008 4.5, 4.7,
//! 4.8). Rules run on the proto-JSON value so that the decoded messages carry exactly the
//! metadata the gateway decided on.

use a2a_core::budget::Allowance;
use a2a_core::errors::WireError;
use serde_json::{Map, Value, json};

use crate::error::{GwError, INVALID_PARAMS};

/// Sequence extension key, removed from every egress `metadata` map (0008-044).
pub const EXT_SEQUENCE: &str = "urn:a2abus:ext:sequence:1";
/// Provenance extension key, overwritten on every inbound message (0008-032).
pub const EXT_PROVENANCE: &str = "urn:a2abus:ext:provenance:1";
/// Budget extension key in `Message.metadata` (0008-033).
pub const EXT_BUDGET: &str = "urn:a2abus:ext:budget:1";

/// Metadata prefixes that peers may never set (0008-031).
pub(crate) const FORBIDDEN_PREFIXES: [&str; 2] =
    ["urn:a2abus:ext:approval:", "urn:a2abus:ext:delegation:"];

/// Largest integer representable exactly in an IEEE double (0008-043).
const MAX_EXACT: u64 = 1 << 53;

/// Budget limit fields in proto-JSON spelling, in the order of [`limits_of`].
const LIMIT_FIELDS: [&str; 6] = [
    "maxTokens",
    "maxCostUnits",
    "maxWallMs",
    "maxHops",
    "maxDepth",
    "maxChildren",
];

/// Applies every inbound rule to a `SendMessage` or `SendStreamingMessage` parameter object.
///
/// Rejects forbidden extension metadata and out-of-range integers, overwrites the provenance
/// extension, and caps the budget at the peer's ingress allowance (never raising it).
///
/// # Errors
/// Returns `INVALID_PARAMS` family errors for any violated rule; the value is left unchanged
/// only when an error is returned before the first mutation.
pub fn prepare_send(
    params: &mut Value,
    origin: &str,
    ingress: &Allowance,
    alias: bool,
) -> Result<(), GwError> {
    if alias && has_pre_1_0_shape(params) {
        return Err(GwError::invalid_params(
            "pre-1.0 parameter shape not accepted",
        ));
    }
    let root = params
        .as_object_mut()
        .ok_or_else(|| GwError::invalid_params("params must be an object"))?;
    check_tenant(root)?;
    let message = root
        .get_mut("message")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| GwError::invalid_params("message is required"))?;
    check_message_metadata(message)?;
    let slot = message.entry("metadata").or_insert(Value::Null);
    if slot.is_null() {
        *slot = Value::Object(Map::new());
    }
    let metadata = slot
        .as_object_mut()
        .ok_or_else(|| GwError::wire(WireError::ExtensionDataInvalid))?;
    metadata.insert(
        EXT_PROVENANCE.to_owned(),
        json!({"source": "http-jsonrpc", "peer": origin, "transport": "http"}),
    );
    cap_budget(metadata, ingress)
}

/// Removes the sequence extension from every `metadata` map of an egress value (0008-044).
pub fn strip_sequence(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                if key.as_str() == "metadata" {
                    if let Some(metadata) = child.as_object_mut() {
                        metadata.remove(EXT_SEQUENCE);
                    }
                }
                strip_sequence(child);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(strip_sequence),
        _ => {}
    }
}

/// Rejects forbidden extension keys and out-of-range integers in the message and its parts.
fn check_message_metadata(message: &Map<String, Value>) -> Result<(), GwError> {
    check_metadata(message.get("metadata"))?;
    if let Some(parts) = message.get("parts").and_then(Value::as_array) {
        for part in parts {
            check_metadata(part.get("metadata"))?;
        }
    }
    Ok(())
}

fn check_metadata(metadata: Option<&Value>) -> Result<(), GwError> {
    let Some(metadata) = metadata else {
        return Ok(());
    };
    if let Some(map) = metadata.as_object() {
        let forbidden = map.keys().any(|key| {
            FORBIDDEN_PREFIXES
                .iter()
                .any(|prefix| key.starts_with(prefix))
        });
        if forbidden {
            return Err(GwError::new(
                INVALID_PARAMS,
                "EXTENSION_NOT_ALLOWED_FROM_PEER",
                "extension is not allowed from peers",
            ));
        }
    }
    if within_exact_range(metadata) {
        Ok(())
    } else {
        Err(GwError::wire(WireError::ExtensionDataInvalid))
    }
}

/// Whether every integer in the value fits the exact double range (0008-043).
fn within_exact_range(value: &Value) -> bool {
    match value {
        Value::Number(number) => {
            if let Some(unsigned) = number.as_u64() {
                unsigned <= MAX_EXACT
            } else if let Some(signed) = number.as_i64() {
                signed.unsigned_abs() <= MAX_EXACT
            } else {
                true
            }
        }
        Value::Array(items) => items.iter().all(within_exact_range),
        Value::Object(map) => map.values().all(within_exact_range),
        _ => true,
    }
}

/// Whether a parameter object uses pre-1.0 shapes: `kind` discriminators or lowercase roles
/// outside `metadata` and `data` (0008-030).
fn has_pre_1_0_shape(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(key, child)| match key.as_str() {
            "kind" => true,
            "role" => child
                .as_str()
                .is_some_and(|role| !role.starts_with("ROLE_")),
            "metadata" | "data" => false,
            _ => has_pre_1_0_shape(child),
        }),
        Value::Array(items) => items.iter().any(has_pre_1_0_shape),
        _ => false,
    }
}

/// Caps each caller budget limit at the peer allowance, or installs the allowance when the
/// caller supplies no budget (0008-033). The caller's lineage (`chain`) is left untouched.
fn cap_budget(metadata: &mut Map<String, Value>, ingress: &Allowance) -> Result<(), GwError> {
    let caps = limits_of(ingress);
    let slot = metadata
        .entry(EXT_BUDGET)
        .or_insert_with(|| Value::Object(Map::new()));
    let budget = slot
        .as_object_mut()
        .ok_or_else(|| GwError::wire(WireError::ExtensionDataInvalid))?;
    for (index, field) in LIMIT_FIELDS.iter().enumerate() {
        let cap = caps[index];
        let merged = match budget.get(*field) {
            None => cap,
            Some(value) => parse_limit(value)?.min(cap),
        };
        let encoded = if index < 3 {
            Value::String(merged.to_string())
        } else {
            Value::from(merged)
        };
        budget.insert((*field).to_owned(), encoded);
    }
    Ok(())
}

fn limits_of(allowance: &Allowance) -> [u64; 6] {
    [
        allowance.max_tokens,
        allowance.max_cost_units,
        allowance.max_wall_ms,
        u64::from(allowance.max_hops),
        u64::from(allowance.max_depth),
        u64::from(allowance.max_children),
    ]
}

fn parse_limit(value: &Value) -> Result<u64, GwError> {
    let parsed = match value {
        Value::String(text) => text.parse::<u64>().ok(),
        Value::Number(number) => number.as_u64(),
        _ => None,
    };
    parsed.ok_or_else(|| GwError::wire(WireError::ExtensionDataInvalid))
}

/// Rejects any tenant other than the empty string or `local`; the gateway has one tenant.
///
/// # Errors
/// Returns `TenantMismatch` for any other tenant.
pub fn check_tenant(root: &Map<String, Value>) -> Result<(), GwError> {
    match root.get("tenant").and_then(Value::as_str) {
        None | Some("" | "local") => Ok(()),
        Some(_) => Err(GwError::wire(WireError::TenantMismatch)),
    }
}
