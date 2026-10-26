//! HTTP transport: reqwest client, bearer auth, JSON-RPC posts and SSE streams (RFC-0009 4.6; 0009-042..053).

use std::collections::VecDeque;
use std::time::Duration;

use a2a_types::vocab::A2A_VERSION;
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use reqwest::{Response, StatusCode};
use serde_json::Value;
use url::Url;

use crate::config::{PeerSettings, STREAM_IDLE, Secret};
use crate::namespace;
use crate::rpc::{self, Envelope, Malformed, RpcError};
use crate::sse::SseDecoder;

/// Builds the HTTP client: no proxy, no redirects (a redirect could move the bearer), bounded connect.
///
/// # Errors
/// Returns the reqwest builder error.
pub fn build_http(timeout: Duration) -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(timeout)
        .build()
}

/// Transport failure classes seen by the driver.
#[derive(Debug)]
pub enum CallError {
    /// No response: connect refused, reset, or DNS failure.
    Unreachable,
    /// Request or stream idle timeout elapsed.
    Timeout,
    /// Non-success HTTP status, with the JSON-RPC error when the body carried one.
    Status {
        /// HTTP status code.
        status: u16,
        /// JSON-RPC error from the body, if any.
        rpc: Option<RpcError>,
    },
    /// HTTP 200 carrying a JSON-RPC error.
    Rpc(RpcError),
    /// Body did not match the expected shape.
    Malformed(Malformed),
}

/// Peer HTTP endpoint with its authentication and tenant settings.
#[derive(Debug)]
pub struct Transport {
    http: reqwest::Client,
    rpc_url: Url,
    bearer: Option<Secret>,
    tenant: Option<String>,
    timeout: Duration,
}

impl Transport {
    /// Creates a transport for `rpc_url`. The bearer is attached only when configured (0008-069).
    pub fn new(http: reqwest::Client, rpc_url: Url, settings: &PeerSettings) -> Self {
        Self {
            http,
            rpc_url,
            bearer: settings.bearer.clone(),
            tenant: settings.tenant.clone(),
            timeout: settings.timeout,
        }
    }

    /// Sends a unary JSON-RPC call and returns its `result`.
    ///
    /// # Errors
    /// Returns [`CallError`] for transport, status, RPC, or shape failures.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, CallError> {
        let response = self.post(method, params, false).await?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(map_transport)?;
        if !status.is_success() {
            return Err(status_error(status, &bytes));
        }
        match rpc::parse_envelope(&bytes) {
            Ok(Envelope::Result(value)) => Ok(value),
            Ok(Envelope::Error(error)) => Err(CallError::Rpc(error)),
            Err(malformed) => Err(CallError::Malformed(malformed)),
        }
    }

    /// Opens a streaming JSON-RPC call (`text/event-stream`). A plain JSON answer is
    /// returned as a single payload (tolerance for servers that ignore `Accept`).
    ///
    /// # Errors
    /// Returns [`CallError`] for transport or status failures before the stream opens.
    pub async fn open_stream(&self, method: &str, params: Value) -> Result<EventStream, CallError> {
        let response = self.post(method, params, true).await?;
        let status = response.status();
        if !status.is_success() {
            let bytes = response.bytes().await.map_err(map_transport)?;
            return Err(status_error(status, &bytes));
        }
        let is_json = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("application/json"));
        if is_json {
            let bytes = response.bytes().await.map_err(map_transport)?;
            let text = String::from_utf8_lossy(&bytes).into_owned();
            return Ok(EventStream {
                response: None,
                decoder: SseDecoder::new(),
                pending: VecDeque::from([text]),
            });
        }
        Ok(EventStream {
            response: Some(response),
            decoder: SseDecoder::new(),
            pending: VecDeque::new(),
        })
    }

    async fn post(
        &self,
        method: &str,
        params: Value,
        streaming: bool,
    ) -> Result<Response, CallError> {
        let body = rpc::request(&namespace::request_id(), method, self.with_tenant(params));
        let accept = if streaming {
            "text/event-stream"
        } else {
            "application/json"
        };
        let mut request = self
            .http
            .post(self.rpc_url.clone())
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, accept)
            .header("A2A-Version", A2A_VERSION)
            .json(&body);
        if let Some(bearer) = &self.bearer {
            request = request.bearer_auth(bearer.expose());
        }
        if !streaming {
            request = request.timeout(self.timeout);
        }
        request.send().await.map_err(map_transport)
    }

    /// Adds `tenant` to `params` when configured (0009-042).
    fn with_tenant(&self, mut params: Value) -> Value {
        if let (Some(tenant), Value::Object(map)) = (&self.tenant, &mut params) {
            map.insert("tenant".to_owned(), Value::String(tenant.clone()));
        }
        params
    }
}

/// Incremental SSE payload reader with an idle timeout (0008-068; 30 s).
#[derive(Debug)]
pub struct EventStream {
    response: Option<Response>,
    decoder: SseDecoder,
    pending: VecDeque<String>,
}

impl EventStream {
    /// Returns the next event payload, `Ok(None)` at a clean end of stream.
    ///
    /// # Errors
    /// Returns [`CallError::Timeout`] when no bytes arrive within the idle window, or a transport class.
    pub async fn next_payload(&mut self) -> Result<Option<String>, CallError> {
        loop {
            if let Some(payload) = self.pending.pop_front() {
                return Ok(Some(payload));
            }
            let Some(response) = self.response.as_mut() else {
                return Ok(None);
            };
            let chunk = match tokio::time::timeout(STREAM_IDLE, response.chunk()).await {
                Err(_) => return Err(CallError::Timeout),
                Ok(Err(error)) => return Err(map_transport(error)),
                Ok(Ok(None)) => return Ok(None),
                Ok(Ok(Some(bytes))) => bytes,
            };
            self.pending.extend(self.decoder.push(&chunk));
        }
    }
}

fn map_transport(error: reqwest::Error) -> CallError {
    if error.is_timeout() {
        CallError::Timeout
    } else {
        CallError::Unreachable
    }
}

fn status_error(status: StatusCode, body: &[u8]) -> CallError {
    let rpc = match rpc::parse_envelope(body) {
        Ok(Envelope::Error(error)) => Some(error),
        _ => None,
    };
    CallError::Status {
        status: status.as_u16(),
        rpc,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// 0009-042: tenant is added to params only when configured.
    #[test]
    fn req_0009_042_tenant_is_injected_when_configured() {
        let settings = PeerSettings {
            agent_id: a2a_types::ids::AgentId::parse("hermes-peer").unwrap(),
            base_url: Url::parse("http://127.0.0.1:9911/").unwrap(),
            bearer: None,
            tenant: Some("tenant-a".to_owned()),
            timeout: Duration::from_secs(5),
            streaming: false,
        };
        let transport = Transport::new(
            build_http(Duration::from_secs(5)).unwrap(),
            settings.base_url.clone(),
            &settings,
        );
        let params = transport.with_tenant(serde_json::json!({"message": {}}));
        assert_eq!(params["tenant"], "tenant-a");
    }

    /// 0008-069: a non-HTTPS endpoint never receives the bearer; this module only attaches it when configured.
    #[test]
    fn req_0008_069_no_bearer_without_configuration() {
        let settings = PeerSettings {
            agent_id: a2a_types::ids::AgentId::parse("hermes-peer").unwrap(),
            base_url: Url::parse("http://127.0.0.1:9911/").unwrap(),
            bearer: None,
            tenant: None,
            timeout: Duration::from_secs(5),
            streaming: false,
        };
        let transport = Transport::new(
            build_http(Duration::from_secs(5)).unwrap(),
            settings.base_url.clone(),
            &settings,
        );
        assert!(transport.bearer.is_none());
    }

    #[test]
    fn status_error_keeps_rpc_body_when_present() {
        let body =
            br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32050,"message":"unauthorized"}}"#;
        match status_error(StatusCode::UNAUTHORIZED, body) {
            CallError::Status { status, rpc } => {
                assert_eq!(status, 401);
                assert_eq!(rpc.map(|e| e.code), Some(-32050));
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
