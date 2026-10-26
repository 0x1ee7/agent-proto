//! JSON-RPC 2.0 envelope handling and the method table (RFC-0008 4.3, 4.4, 4.6).

use serde_json::{Map, Value, json};

use http::StatusCode;

use crate::error::{GwError, INVALID_PARAMS, INVALID_REQUEST, METHOD_NOT_FOUND, PARSE_ERROR};

/// Type URL of the `google.rpc.ErrorInfo` detail (0008-045).
pub const ERROR_INFO_TYPE: &str = "type.googleapis.com/google.rpc.ErrorInfo";

/// Operations the gateway dispatches (0008-037).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// `SendMessage`.
    SendMessage,
    /// `SendStreamingMessage`.
    SendStreamingMessage,
    /// `GetTask`.
    GetTask,
    /// `ListTasks`.
    ListTasks,
    /// `CancelTask`.
    CancelTask,
    /// `SubscribeToTask`.
    SubscribeToTask,
    /// `CreateTaskPushNotificationConfig`.
    CreatePushConfig,
    /// `GetTaskPushNotificationConfig`.
    GetPushConfig,
    /// `ListTaskPushNotificationConfigs`.
    ListPushConfigs,
    /// `DeleteTaskPushNotificationConfig`.
    DeletePushConfig,
    /// `GetExtendedAgentCard`.
    GetExtendedCard,
}

impl Method {
    /// Resolves a canonical method name or an ingress alias (0008-029).
    ///
    /// Returns the method and whether the name was an alias.
    #[must_use]
    pub fn resolve(name: &str) -> Option<(Self, bool)> {
        let canonical = match name {
            "SendMessage" => Self::SendMessage,
            "SendStreamingMessage" => Self::SendStreamingMessage,
            "GetTask" => Self::GetTask,
            "ListTasks" => Self::ListTasks,
            "CancelTask" => Self::CancelTask,
            "SubscribeToTask" => Self::SubscribeToTask,
            "CreateTaskPushNotificationConfig" => Self::CreatePushConfig,
            "GetTaskPushNotificationConfig" => Self::GetPushConfig,
            "ListTaskPushNotificationConfigs" => Self::ListPushConfigs,
            "DeleteTaskPushNotificationConfig" => Self::DeletePushConfig,
            "GetExtendedAgentCard" => Self::GetExtendedCard,
            "message/send" => return Some((Self::SendMessage, true)),
            "message/stream" => return Some((Self::SendStreamingMessage, true)),
            "tasks/get" => return Some((Self::GetTask, true)),
            "tasks/list" => return Some((Self::ListTasks, true)),
            "tasks/cancel" => return Some((Self::CancelTask, true)),
            "tasks/resubscribe" | "tasks/subscribe" => return Some((Self::SubscribeToTask, true)),
            "tasks/pushNotificationConfig/set"
            | "tasks/pushNotificationConfig/create"
            | "tasks/pushNotification/set" => return Some((Self::CreatePushConfig, true)),
            "tasks/pushNotificationConfig/get" => return Some((Self::GetPushConfig, true)),
            "tasks/pushNotificationConfig/list" => return Some((Self::ListPushConfigs, true)),
            "tasks/pushNotificationConfig/delete" => return Some((Self::DeletePushConfig, true)),
            "agent/getAuthenticatedExtendedCard" => return Some((Self::GetExtendedCard, true)),
            _ => return None,
        };
        Some((canonical, false))
    }
}

/// A parsed JSON-RPC request.
#[derive(Debug, Clone)]
pub struct Request {
    /// Request identifier, echoed in the response.
    pub id: Value,
    /// Resolved operation.
    pub method: Method,
    /// Whether the method name was an ingress alias (0008-029).
    pub alias: bool,
    /// Parameters; an object (absent parameters become an empty object).
    pub params: Value,
}

/// A request that failed envelope validation, with the identifier to echo.
#[derive(Debug)]
pub struct Rejected {
    /// Identifier to echo (`null` when unknown).
    pub id: Value,
    /// Error to return.
    pub error: GwError,
}

/// Parses and validates the JSON-RPC envelope of a request body.
///
/// # Errors
/// Returns `PARSE_ERROR` for invalid JSON, `INVALID_REQUEST` for a batch or a malformed
/// envelope, `INVALID_PARAMS` for non-object parameters, and `METHOD_NOT_FOUND` for an
/// unknown method.
pub fn parse(body: &[u8]) -> Result<Request, Rejected> {
    let value: Value = serde_json::from_slice(body).map_err(|_| Rejected {
        id: Value::Null,
        error: GwError::http(
            StatusCode::BAD_REQUEST,
            PARSE_ERROR,
            "PARSE_ERROR",
            "request body is not valid JSON",
        ),
    })?;
    let Value::Object(mut object) = value else {
        return Err(Rejected {
            id: Value::Null,
            error: GwError::http(
                StatusCode::BAD_REQUEST,
                INVALID_REQUEST,
                "INVALID_REQUEST",
                "request must be a single JSON object; batches are not supported",
            ),
        });
    };
    let id = object.remove("id").unwrap_or(Value::Null);
    let invalid = |message: &'static str| Rejected {
        id: id.clone(),
        error: GwError::http(
            StatusCode::BAD_REQUEST,
            INVALID_REQUEST,
            "INVALID_REQUEST",
            message,
        ),
    };
    if !matches!(id, Value::Null | Value::String(_) | Value::Number(_)) {
        return Err(invalid("request id must be a string, number, or null"));
    }
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(invalid("jsonrpc must be \"2.0\""));
    }
    let Some(name) = object.get("method").and_then(Value::as_str) else {
        return Err(invalid("method must be a string"));
    };
    let Some((method, alias)) = Method::resolve(name) else {
        return Err(Rejected {
            id: id.clone(),
            error: GwError::new(METHOD_NOT_FOUND, "METHOD_NOT_FOUND", "method not found"),
        });
    };
    let params = match object.remove("params") {
        None | Some(Value::Null) => Value::Object(Map::new()),
        Some(value @ Value::Object(_)) => value,
        Some(_) => {
            return Err(Rejected {
                id: id.clone(),
                error: GwError::new(INVALID_PARAMS, "INVALID_PARAMS", "params must be an object"),
            });
        }
    };
    Ok(Request {
        id,
        method,
        alias,
        params,
    })
}

/// Builds a success response.
#[must_use]
pub fn success(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// Builds an error response carrying one `google.rpc.ErrorInfo` detail (0008-045).
#[must_use]
pub fn failure(id: &Value, error: &GwError) -> Value {
    let detail = json!({
        "@type": ERROR_INFO_TYPE,
        "reason": error.reason,
        "domain": error.domain,
        "metadata": {},
    });
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": error.code,
            "message": error.message,
            "data": [detail],
        },
    })
}
