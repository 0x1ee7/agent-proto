//! Pure translation between ACP messages and worker seam types.
//!
//! Nothing here performs I/O or holds state, so every rule is unit-testable.

use std::path::Path;

use a2a_worker::backend::{
    Cost, Gauges, Outcome, PeerTools, PermissionOption, ReasonToken, UsageSample, UsageSource,
};
use serde_json::{Value, json};

use crate::redact::{detail, redact};

/// Progress metadata key (RFC-0009 progress extension).
pub(crate) const PROGRESS_KEY: &str = "urn:a2abus:ext:progress:1";

/// Currencies with two minor-unit digits; costs in other currencies are dropped.
const TWO_DIGIT_CURRENCIES: [&str; 6] = ["USD", "EUR", "GBP", "CAD", "AUD", "CHF"];

/// `initialize` request params (0009-018).
pub(crate) fn initialize_params() -> Value {
    json!({
        "protocolVersion": 1,
        "clientCapabilities": {
            "fs": {"readTextFile": false, "writeTextFile": false},
            "terminal": false,
            "auth": {"terminal": false},
            "elicitation": {"form": {}}
        },
        "clientInfo": {"name": "a2a-bus-driver-acp", "version": env!("CARGO_PKG_VERSION")}
    })
}

/// Params for `session/new`, `session/load` and `session/resume` (0009-017).
pub(crate) fn session_params(cwd: &Path, mcp_servers: &[Value], session_id: Option<&str>) -> Value {
    let mut params = json!({
        "cwd": cwd.to_string_lossy(),
        "mcpServers": mcp_servers,
    });
    if let (Some(id), Some(map)) = (session_id, params.as_object_mut()) {
        map.insert("sessionId".to_owned(), Value::String(id.to_owned()));
    }
    params
}

/// The `mcpServers` array for a session (RFC-0015 [0015-084], [0015-085]). With a peer-tools
/// binding it is exactly the shim entry; without one it is the operator list (empty by default).
pub(crate) fn mcp_servers(config_servers: &[Value], peer: Option<&PeerTools>) -> Vec<Value> {
    match peer {
        Some(peer) => vec![peer_tools_entry(peer)],
        None => config_servers.to_vec(),
    }
}

/// The stdio entry for the peer-tools shim (RFC-0015 [0015-084]). The grant credential appears
/// only in `env`; the returned value must not be logged.
pub(crate) fn peer_tools_entry(peer: &PeerTools) -> Value {
    json!({
        "name": "a2abus",
        "command": peer.shim_path.to_string_lossy(),
        "args": [
            "--grant", peer.grant_id,
            "--endpoint", peer.endpoint.to_string_lossy(),
            "--slot", peer.slot,
        ],
        "env": [{"name": "A2ABUS_MCP_GRANT", "value": peer.grant_credential.expose()}],
    })
}

/// Why a peer-tools binding cannot be used with this configuration, or `None` if it can.
/// Refusals are checked at Start, before any child is spawned (RFC-0015 [0015-084]).
pub(crate) fn peer_tools_refusal(
    operator_servers: &[Value],
    peer: Option<&PeerTools>,
) -> Option<&'static str> {
    let peer = peer?;
    if !operator_servers.is_empty() {
        return Some("operator mcp_servers must be empty when peer tools are bound");
    }
    if !peer.shim_path.is_absolute() || !peer.endpoint.is_absolute() {
        return Some("peer tools shim and endpoint paths must be absolute");
    }
    let base64url = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    };
    if !base64url(&peer.slot) || !base64url(&peer.grant_id) {
        return Some("peer tools slot and grant id must be base64url");
    }
    if !base64url(peer.grant_credential.expose()) {
        return Some("peer tools grant credential must be base64url");
    }
    None
}

/// Params for `session/prompt` with a single text block.
pub(crate) fn prompt_params(session_id: &str, text: &str) -> Value {
    json!({
        "sessionId": session_id,
        "prompt": [{"type": "text", "text": text}]
    })
}

/// Capabilities the agent advertised in its `initialize` result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ServerCaps {
    pub(crate) load_session: bool,
    pub(crate) resume: bool,
}

/// Reads the negotiated protocol version from an `initialize` result.
pub(crate) fn protocol_version(init: &Value) -> Option<u64> {
    init.get("protocolVersion").and_then(Value::as_u64)
}

/// Reads restore capabilities from an `initialize` result.
pub(crate) fn server_caps(init: &Value) -> ServerCaps {
    let agent = init.get("agentCapabilities");
    let load_session = agent
        .and_then(|a| a.get("loadSession"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let resume = agent
        .and_then(|a| a.get("sessionCapabilities"))
        .and_then(|s| s.get("resume"))
        .is_some_and(|v| !v.is_null() && v.as_bool() != Some(false));
    ServerCaps {
        load_session,
        resume,
    }
}

/// Wraps a progress payload in the metadata key and serializes it (0009-027).
pub(crate) fn progress_text(payload: Value) -> String {
    redact(&json!({ PROGRESS_KEY: payload }).to_string())
}

/// Builds the progress payload for a tool-call update: id, kind, title, status; never raw output (0009-027).
pub(crate) fn tool_payload(update: &Value) -> Option<(String, Value)> {
    let id = update.get("toolCallId").and_then(Value::as_str)?.to_owned();
    let title = update.get("title").and_then(Value::as_str).unwrap_or("");
    let kind = update.get("kind").and_then(Value::as_str).unwrap_or("");
    let status = update.get("status").and_then(Value::as_str).unwrap_or("");
    let payload = json!({
        "toolCallId": id,
        "kind": kind,
        "title": redact(title),
        "status": status,
    });
    Some((id, payload))
}

/// What a `session/update` notification becomes on the seam.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Translated {
    /// Agent message text (0009-025), already redacted.
    Text(String),
    /// Progress text (tool calls, plan), already redacted (0009-027, 0009-028).
    Progress(String),
    /// Operator-visible notice text (0009-029), already redacted.
    Notice(String),
    /// Metering sample from `usage_update` (0009-029, 0009-054).
    Usage(UsageSample),
    /// Ignored: thoughts, replayed user text, unknown kinds (0009-026, 0009-030).
    Dropped,
}

/// Translates one `session/update` payload (the `update` object).
pub(crate) fn translate_update(update: &Value) -> Translated {
    match update.get("sessionUpdate").and_then(Value::as_str) {
        Some("agent_message_chunk") => match content_text(update) {
            Some(text) if !text.is_empty() => Translated::Text(redact(text)),
            _ => Translated::Dropped,
        },
        Some("tool_call" | "tool_call_update") => match tool_payload(update) {
            Some((_, payload)) => Translated::Progress(progress_text(payload)),
            None => Translated::Dropped,
        },
        Some("plan") => match update.get("entries") {
            Some(entries) => {
                Translated::Progress(progress_text(json!({"plan": {"entries": entries}})))
            }
            None => Translated::Dropped,
        },
        Some("usage_update") => usage_update(update).map_or(Translated::Dropped, Translated::Usage),
        Some("notice") => {
            let text = content_text(update)
                .map(str::to_owned)
                .or_else(|| update.get("content").map(Value::to_string))
                .unwrap_or_default();
            Translated::Notice(redact(&text))
        }
        _ => Translated::Dropped,
    }
}

fn content_text(update: &Value) -> Option<&str> {
    let content = update.get("content")?;
    if content.get("type").and_then(Value::as_str) == Some("text") {
        content.get("text").and_then(Value::as_str)
    } else {
        None
    }
}

/// Builds a usage sample from `usage_update` (gauges and cumulative cost; 0009-056).
pub(crate) fn usage_update(update: &Value) -> Option<UsageSample> {
    let gauges = match (
        update.get("used").and_then(Value::as_u64),
        update.get("size").and_then(Value::as_u64),
    ) {
        (Some(used), Some(size)) => Some(Gauges { used, size }),
        _ => None,
    };
    let cost = update.get("cost").and_then(cost_of);
    if gauges.is_none() && cost.is_none() {
        return None;
    }
    Some(UsageSample {
        source: UsageSource::AcpUsage,
        turns: None,
        wall_ms: None,
        tokens_in: None,
        tokens_out: None,
        cost,
        gauges,
    })
}

fn cost_of(cost: &Value) -> Option<Cost> {
    let amount = cost.get("amount").and_then(Value::as_f64)?;
    let currency = cost.get("currency").and_then(Value::as_str)?;
    if !amount.is_finite() || amount < 0.0 || !TWO_DIGIT_CURRENCIES.contains(&currency) {
        return None;
    }
    Some(Cost {
        units: (amount * 100.0).round() as u64,
        currency: currency.to_owned(),
    })
}

/// Token usage from a prompt result (0009-086). A cancelled turn reports zero.
pub(crate) fn prompt_usage(result: &Value, cancelled: bool) -> Option<UsageSample> {
    let (tokens_in, tokens_out) = if cancelled {
        (Some(0), Some(0))
    } else {
        let usage = result.get("usage")?;
        (
            usage.get("inputTokens").and_then(Value::as_u64),
            usage.get("outputTokens").and_then(Value::as_u64),
        )
    };
    if tokens_in.is_none() && tokens_out.is_none() {
        return None;
    }
    Some(UsageSample {
        source: UsageSource::AcpUsage,
        turns: None,
        wall_ms: None,
        tokens_in,
        tokens_out,
        cost: None,
        gauges: None,
    })
}

/// Maps a `session/prompt` stop reason to a terminal outcome (0009-021).
pub(crate) fn outcome_for_stop(stop: &str) -> Outcome {
    match stop {
        "end_turn" => Outcome::Completed,
        "cancelled" => Outcome::Canceled,
        "refusal" => Outcome::Refused {
            reason: ReasonToken::AgentRefused,
            health_class: None,
        },
        "max_tokens" | "max_turn_requests" => Outcome::Failed {
            reason: ReasonToken::AgentLimit,
            health_class: None,
            detail: detail(stop),
        },
        other => Outcome::Failed {
            reason: ReasonToken::BackendError,
            health_class: None,
            detail: detail(&format!("unknown stop reason: {other}")),
        },
    }
}

/// Permission options as the agent sent them, in order (0009-032).
pub(crate) fn permission_options(params: &Value) -> Vec<PermissionOption> {
    params
        .get("options")
        .and_then(Value::as_array)
        .map(|options| {
            options
                .iter()
                .filter_map(|o| {
                    let option_id = o.get("optionId").and_then(Value::as_str)?;
                    let kind = o.get("kind").and_then(Value::as_str).unwrap_or("");
                    Some(PermissionOption {
                        option_id: option_id.to_owned(),
                        kind: kind.to_owned(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Result of parsing an `elicitation/create` request (0009-082).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Elicitation {
    /// Decline without an operator event.
    Decline,
    /// Ask the operator. `allow_label` is the exact enum label that approves, if offered.
    Ask {
        tool_title: String,
        allow_label: Option<&'static str>,
    },
}

/// Exact enum label that approves a tool call under `always-ask-cached` (spike Q4).
pub(crate) const APPROVE_LABEL: &str = "Approve Once";

/// Parses an `elicitation/create` request. Only form elicitations whose first
/// message line is `Allow tool: <name>` become operator events (0009-082).
pub(crate) fn parse_elicitation(params: &Value) -> Elicitation {
    if params.get("mode").and_then(Value::as_str) != Some("form") {
        return Elicitation::Decline;
    }
    let message = params.get("message").and_then(Value::as_str).unwrap_or("");
    let first = message.lines().next().unwrap_or("").trim();
    let Some(tool) = first.strip_prefix("Allow tool:") else {
        return Elicitation::Decline;
    };
    if tool.trim().is_empty() {
        return Elicitation::Decline;
    }
    let enum_offers_approve = params
        .get("requestedSchema")
        .and_then(|s| s.get("properties"))
        .and_then(Value::as_object)
        .is_some_and(|props| {
            props.values().any(|p| {
                p.get("enum")
                    .and_then(Value::as_array)
                    .is_some_and(|labels| labels.iter().any(|l| l.as_str() == Some(APPROVE_LABEL)))
            })
        });
    Elicitation::Ask {
        tool_title: redact(first),
        allow_label: enum_offers_approve.then_some(APPROVE_LABEL),
    }
}

/// Whether a JSON-RPC error means the agent does not know the session (0009-013).
pub(crate) fn is_session_not_found(error: &Value) -> bool {
    if error.get("code").and_then(Value::as_i64) == Some(-32002) {
        return true;
    }
    let message = error.get("message").and_then(Value::as_str).unwrap_or("");
    message.contains("ACP session not found") || message.contains("ACP session load was cancelled")
}

/// Redacted, bounded message of a JSON-RPC error.
pub(crate) fn error_detail(error: &Value) -> String {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("agent error");
    detail(message)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]
    use super::*;

    #[test]
    fn stop_reasons_map_to_outcomes_0009_021() {
        assert_eq!(outcome_for_stop("end_turn"), Outcome::Completed);
        assert_eq!(outcome_for_stop("cancelled"), Outcome::Canceled);
        assert_eq!(
            outcome_for_stop("refusal"),
            Outcome::Refused {
                reason: ReasonToken::AgentRefused,
                health_class: None
            }
        );
        for limit in ["max_tokens", "max_turn_requests"] {
            assert!(matches!(
                outcome_for_stop(limit),
                Outcome::Failed {
                    reason: ReasonToken::AgentLimit,
                    ..
                }
            ));
        }
        assert!(matches!(
            outcome_for_stop("surprise"),
            Outcome::Failed {
                reason: ReasonToken::BackendError,
                ..
            }
        ));
    }

    #[test]
    fn text_chunks_become_text_and_thoughts_drop_0009_025() {
        let text = json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "hi"}});
        assert_eq!(translate_update(&text), Translated::Text("hi".into()));
        let image = json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "image", "data": "x"}});
        assert_eq!(translate_update(&image), Translated::Dropped);
        let thought = json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "hmm"}});
        assert_eq!(translate_update(&thought), Translated::Dropped);
    }

    #[test]
    fn tool_progress_omits_raw_output_0009_027() {
        let update = json!({
            "sessionUpdate": "tool_call", "toolCallId": "t1", "kind": "execute",
            "title": "run ls", "status": "pending", "rawOutput": "SECRET-BODY"
        });
        let Translated::Progress(text) = translate_update(&update) else {
            panic!("expected progress");
        };
        assert!(text.contains("toolCallId"));
        assert!(text.contains(PROGRESS_KEY));
        assert!(!text.contains("SECRET-BODY"));
    }

    #[test]
    fn plan_and_notice_map_0009_028_0009_029() {
        let plan =
            json!({"sessionUpdate": "plan", "entries": [{"content": "step", "status": "pending"}]});
        let Translated::Progress(text) = translate_update(&plan) else {
            panic!("expected plan progress");
        };
        assert!(text.contains("\"plan\""));
        let notice =
            json!({"sessionUpdate": "notice", "content": {"type": "text", "text": "heads up"}});
        assert_eq!(
            translate_update(&notice),
            Translated::Notice("heads up".into())
        );
    }

    #[test]
    fn unknown_kinds_drop_0009_030() {
        let unknown = json!({"sessionUpdate": "something_new", "x": 1});
        assert_eq!(translate_update(&unknown), Translated::Dropped);
    }

    #[test]
    fn usage_update_carries_gauges_and_cost_0009_056() {
        let update = json!({"sessionUpdate": "usage_update", "used": 10, "size": 100,
            "cost": {"amount": 0.42, "currency": "USD"}});
        let Translated::Usage(sample) = translate_update(&update) else {
            panic!("expected usage");
        };
        assert_eq!(
            sample.gauges,
            Some(Gauges {
                used: 10,
                size: 100
            })
        );
        assert_eq!(
            sample.cost,
            Some(Cost {
                units: 42,
                currency: "USD".into()
            })
        );
        assert_eq!(sample.tokens_in, None);
        assert_eq!(sample.turns, None);
    }

    #[test]
    fn unlisted_currency_cost_is_dropped_0009_056() {
        let update = json!({"sessionUpdate": "usage_update", "used": 1, "size": 2,
            "cost": {"amount": 1.0, "currency": "JPY"}});
        let Translated::Usage(sample) = translate_update(&update) else {
            panic!("expected usage");
        };
        assert_eq!(sample.cost, None);
    }

    #[test]
    fn prompt_usage_zero_on_cancel_0009_086() {
        let result =
            json!({"stopReason": "cancelled", "usage": {"inputTokens": 9, "outputTokens": 9}});
        let sample = prompt_usage(&result, true).unwrap_or_else(|| panic!("zero sample"));
        assert_eq!(sample.tokens_in, Some(0));
        assert_eq!(sample.tokens_out, Some(0));
        let completed = prompt_usage(&result, false).unwrap_or_else(|| panic!("sample"));
        assert_eq!(completed.tokens_in, Some(9));
    }

    #[test]
    fn elicitation_requires_form_and_allow_line_0009_082() {
        let form = json!({
            "mode": "form",
            "message": "Allow tool: write\nPath: a.txt",
            "requestedSchema": {"type": "object", "properties": {"value": {"type": "string",
                "enum": ["Approve", "Approve Once", "Deny"]}}}
        });
        assert_eq!(
            parse_elicitation(&form),
            Elicitation::Ask {
                tool_title: "Allow tool: write".into(),
                allow_label: Some(APPROVE_LABEL),
            }
        );
        let no_label = json!({"mode": "form", "message": "Allow tool: read",
            "requestedSchema": {"properties": {"value": {"enum": ["Deny"]}}}});
        assert_eq!(
            parse_elicitation(&no_label),
            Elicitation::Ask {
                tool_title: "Allow tool: read".into(),
                allow_label: None
            }
        );
        let other_text = json!({"mode": "form", "message": "Pick a color"});
        assert_eq!(parse_elicitation(&other_text), Elicitation::Decline);
        let url = json!({"mode": "url", "message": "Allow tool: write"});
        assert_eq!(parse_elicitation(&url), Elicitation::Decline);
    }

    #[test]
    fn session_not_found_variants_0009_013() {
        assert!(is_session_not_found(
            &json!({"code": -32002, "message": "Resource not found"})
        ));
        assert!(is_session_not_found(
            &json!({"code": -32000, "message": "ACP session not found"})
        ));
        assert!(is_session_not_found(
            &json!({"code": -32000, "message": "ACP session load was cancelled"})
        ));
        assert!(!is_session_not_found(
            &json!({"code": -32000, "message": "boom"})
        ));
    }

    #[test]
    fn server_caps_read_restore_flags_0009_013() {
        let init = json!({"protocolVersion": 1, "agentCapabilities": {
            "loadSession": true, "sessionCapabilities": {"resume": {}}}});
        assert_eq!(
            server_caps(&init),
            ServerCaps {
                load_session: true,
                resume: true
            }
        );
        let none = json!({"protocolVersion": 1, "agentCapabilities": {}});
        assert_eq!(
            server_caps(&none),
            ServerCaps {
                load_session: false,
                resume: false
            }
        );
    }

    #[test]
    fn initialize_declares_no_fs_or_terminal_0009_018() {
        let params = initialize_params();
        assert_eq!(params["protocolVersion"], 1);
        assert_eq!(params["clientCapabilities"]["fs"]["readTextFile"], false);
        assert_eq!(params["clientCapabilities"]["terminal"], false);
        assert!(params["clientCapabilities"]["elicitation"]["form"].is_object());
        assert!(
            params["clientCapabilities"]["elicitation"]
                .get("url")
                .is_none()
        );
    }

    #[test]
    fn session_params_carry_absolute_cwd_and_mcp_array_0009_017() {
        let params = session_params(Path::new("/work"), &[], Some("s1"));
        assert_eq!(params["cwd"], "/work");
        assert!(params["mcpServers"].is_array());
        assert_eq!(params["sessionId"], "s1");
    }

    #[test]
    fn peer_tools_refusal_names_bad_fields_0015_084() {
        let peer = |shim: &str, slot: &str, cred: &str| PeerTools {
            slot: slot.to_owned(),
            grant_id: "Z3JhbnQ".to_owned(),
            grant_credential: a2a_worker::backend::SecretText::new(cred),
            endpoint: std::env::temp_dir().join("ep"),
            shim_path: std::path::PathBuf::from(shim),
        };
        let abs = std::env::temp_dir()
            .join("shim")
            .to_string_lossy()
            .into_owned();
        assert_eq!(peer_tools_refusal(&[], None), None);
        assert_eq!(
            peer_tools_refusal(&[], Some(&peer(&abs, "c2xvdA", "Y3JlZA"))),
            None
        );
        assert!(peer_tools_refusal(&[], Some(&peer("shim", "c2xvdA", "Y3JlZA"))).is_some());
        assert!(peer_tools_refusal(&[], Some(&peer(&abs, "bad slot", "Y3JlZA"))).is_some());
        assert!(peer_tools_refusal(&[], Some(&peer(&abs, "c2xvdA", ""))).is_some());
    }

    #[test]
    fn mcp_servers_follow_binding_0015_085() {
        let operator = vec![json!({"name": "op"})];
        assert_eq!(mcp_servers(&[], None), Vec::<Value>::new());
        assert_eq!(mcp_servers(&operator, None), operator);
    }
}
