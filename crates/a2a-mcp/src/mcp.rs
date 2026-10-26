//! JSON-RPC 2.0 and MCP message handling (RFC-0015 section 4.1). Pure: parses
//! one stdio line and builds the response values. The stdio loop lives in
//! `server`.
//!
//! Stdout carries only MCP frames, one JSON object per line (RFC-0015 [0015-013]).

use serde_json::{Map, Value, json};

use crate::tools::{self, ToolOutput};

/// Largest accepted stdin line in bytes. Longer lines are discarded and
/// answered with `-32600`.
pub const MAX_LINE_BYTES: usize = 1_048_576;

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;

/// One classified inbound line.
#[derive(Debug, Clone, PartialEq)]
pub enum Inbound {
    /// A request with an id; the server must answer it.
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// A notification (no id); never answered.
    Notification { method: String, params: Value },
    /// A response to a server-initiated request; the server issues none, so it
    /// is ignored.
    Response,
    /// Valid JSON that is not a JSON-RPC message; answered with `-32600`.
    Invalid { id: Value },
    /// Not JSON; answered with `-32700`.
    Unparseable,
}

/// Classifies one stdin line.
pub fn parse_line(line: &[u8]) -> Inbound {
    let Ok(value) = serde_json::from_slice::<Value>(line) else {
        return Inbound::Unparseable;
    };
    let Value::Object(obj) = value else {
        return Inbound::Invalid { id: Value::Null };
    };
    let id = obj.get("id").cloned();
    if obj.get("jsonrpc") != Some(&json!("2.0")) {
        return Inbound::Invalid {
            id: id.unwrap_or(Value::Null),
        };
    }
    let method = obj.get("method").and_then(Value::as_str).map(str::to_owned);
    let params = obj.get("params").cloned().unwrap_or(Value::Null);
    match (method, id) {
        (Some(method), Some(id)) => {
            if id.is_string() || id.is_number() {
                Inbound::Request { id, method, params }
            } else {
                Inbound::Invalid { id: Value::Null }
            }
        }
        (Some(method), None) => Inbound::Notification { method, params },
        (None, Some(_)) if obj.contains_key("result") || obj.contains_key("error") => {
            Inbound::Response
        }
        (None, id) => Inbound::Invalid {
            id: id.unwrap_or(Value::Null),
        },
    }
}

/// Successful JSON-RPC response.
pub fn result_message(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// JSON-RPC error response. `id` is `null` when the request id is unknown.
pub fn error_message(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Result of `initialize`: pinned protocol version and the tools capability
/// only (RFC-0015 [0015-010], [0015-011]).
pub fn initialize_result() -> Value {
    json!({
        "protocolVersion": crate::PROTOCOL_VERSION,
        "capabilities": {"tools": {}},
        "serverInfo": {
            "name": crate::SERVER_NAME,
            "version": env!("CARGO_PKG_VERSION"),
        },
    })
}

/// Result of `tools/list`.
pub fn tools_list_result() -> Value {
    json!({"tools": tools::definitions()})
}

/// Result of `tools/call` built from shaped output (RFC-0015 [0015-016]).
pub fn tool_call_result(output: &ToolOutput) -> Value {
    json!({
        "content": [{"type": "text", "text": output.text}],
        "structuredContent": output.structured,
        "isError": output.is_error,
    })
}

/// Extracts `name` and `arguments` from `tools/call` params.
pub fn tools_call_params(params: &Value) -> Option<(String, Value)> {
    let obj: &Map<String, Value> = params.as_object()?;
    let name = obj.get("name")?.as_str()?.to_owned();
    let arguments = obj.get("arguments").cloned().unwrap_or(Value::Null);
    Some((name, arguments))
}

/// Serializes one outbound message as a single stdout line.
pub fn to_line(message: &Value) -> Vec<u8> {
    let mut out = serde_json::to_vec(message).unwrap_or_else(|_| b"{}".to_vec());
    out.push(b'\n');
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    // RFC-0015 [0015-013]: classification of inbound lines.
    #[test]
    fn classifies_lines_0015_013() {
        assert!(matches!(
            parse_line(br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#),
            Inbound::Request { .. }
        ));
        assert!(matches!(
            parse_line(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
            Inbound::Notification { .. }
        ));
        assert!(matches!(parse_line(b"not json"), Inbound::Unparseable));
        assert!(matches!(
            parse_line(br#"{"id":1,"method":"ping"}"#),
            Inbound::Invalid { .. }
        ));
        assert!(matches!(parse_line(b"[1,2]"), Inbound::Invalid { .. }));
        assert!(matches!(
            parse_line(br#"{"jsonrpc":"2.0","id":5,"result":{}}"#),
            Inbound::Response
        ));
    }

    // RFC-0015 [0015-010]: pinned protocol version, tools only.
    #[test]
    fn initialize_pins_version_0015_010() {
        let v = initialize_result();
        assert_eq!(v["protocolVersion"], "2025-06-18");
        assert_eq!(v["capabilities"], json!({"tools": {}}));
    }

    // RFC-0015 [0015-020]: four tools.
    #[test]
    fn tools_list_has_four_tools_0015_020() {
        let v = tools_list_result();
        assert_eq!(v["tools"].as_array().map(Vec::len), Some(4));
    }
}
