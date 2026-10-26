//! HTTP admission, routing, card serving, and the listener (RFC-0008 4.1 and 0008-020; RFC-0004).
//!
//! Every request passes the admission steps of 0008-020 in order before any bus publish. Plain
//! HTTP is served on loopback; any other bind requires TLS (0008-005, 0008-068).

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use a2a_core::errors::WireError;
use a2a_types::ids::AgentId;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{ConnectInfo, Request, State};
use axum::middleware::{Next, from_fn};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use bus_api::bus::Bus;
use http::header::{
    CACHE_CONTROL, CONTENT_TYPE, ETAG, HOST, IF_NONE_MATCH, ORIGIN, RETRY_AFTER, WWW_AUTHENTICATE,
};
use http::request::Parts;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder;
use hyper_util::service::TowerToHyperService;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

use crate::auth::{AuthError, Caller, authenticate};
use crate::cards::{CardView, Lookup};
use crate::config::{GatewayConfig, Profile};
use crate::error::{
    GwError, INVALID_REQUEST, METHOD_NOT_FOUND, PEER_NOT_TRUSTED, RATE_LIMITED, UNAUTHORIZED,
};
use crate::gateway::{HttpGateway, Shared};
use crate::ingress::FORBIDDEN_PREFIXES;
use crate::jsonrpc;
use crate::ops::{self, Frames, Principal};
use crate::tls::{self, TlsError};

/// Largest accepted request body in bytes (0008-026).
const MAX_BODY_BYTES: usize = 1024 * 1024;
/// Interval between keepalive comments on an open stream (0008-055).
const KEEPALIVE: Duration = Duration::from_secs(15);
/// Pause after a failed accept before the next attempt.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);
/// Cache lifetime advertised for agent cards (RFC-0004).
const CARD_CACHE: &str = "max-age=60";
/// Well-known card path for the default agent (RFC-0004).
const CARD_PATH: &str = "/.well-known/agent-card.json";
/// Suffix of the per-agent card path (RFC-0004).
const CARD_SUFFIX: &str = "/.well-known/agent-card.json";
/// Protocol version header (0008-027).
const VERSION_HEADER: &str = "a2a-version";
/// Extension negotiation header (0008-028).
const EXTENSIONS_HEADER: &str = "a2a-extensions";

/// Failure to start or run the listener.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    /// The listen address could not be bound.
    #[error("binding the listener failed: {0}")]
    Bind(#[from] std::io::Error),
    /// The TLS material could not be loaded.
    #[error(transparent)]
    Tls(#[from] TlsError),
    /// The initial card snapshot could not be built.
    #[error("initial card refresh failed: {0}")]
    Refresh(#[from] a2a_client::ClientError),
}

/// Builds the gateway router. Every request is dispatched by admission in 0008-020 order.
pub fn router<B: Bus + Send + Sync + 'static>(gateway: HttpGateway<B>) -> Router {
    Router::new().fallback(handle::<B>).with_state(gateway)
}

/// Runs the gateway listener until the process ends (0008-005).
///
/// Binds the configured address, builds the TLS acceptor when a certificate is configured, builds
/// the initial card snapshot, starts the periodic refresh, and then accepts connections.
///
/// # Errors
/// Returns an error when the address cannot be bound, the TLS material is unusable, or the
/// initial card snapshot cannot be built.
pub async fn serve<B: Bus + Send + Sync + 'static>(
    gateway: HttpGateway<B>,
) -> Result<(), ServeError> {
    let (listen, acceptor) = {
        let config = &gateway.shared().config;
        let acceptor = config.tls.as_ref().map(tls::acceptor).transpose()?;
        (config.listen, acceptor)
    };
    let listener = TcpListener::bind(listen).await?;
    gateway.refresh().await?;
    let _refresh = gateway.spawn_refresh();
    let app = router(gateway);
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(_) => {
                tokio::time::sleep(ACCEPT_BACKOFF).await;
                continue;
            }
        };
        let connection_app = app.clone().layer(from_fn(
            move |mut request: Request, next: Next| async move {
                request.extensions_mut().insert(ConnectInfo(peer));
                next.run(request).await
            },
        ));
        match acceptor.clone() {
            Some(acceptor) => {
                tokio::spawn(async move {
                    // A failed handshake only ends this connection.
                    if let Ok(stream) = acceptor.accept(stream).await {
                        serve_connection(stream, connection_app).await;
                    }
                });
            }
            None => {
                tokio::spawn(serve_connection(stream, connection_app));
            }
        }
    }
}

async fn serve_connection<I>(io: I, app: Router)
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // Connection-level errors (client resets, malformed framing) end only this connection.
    let _ = Builder::new(TokioExecutor::new())
        .serve_connection(TokioIo::new(io), TowerToHyperService::new(app))
        .await;
}

async fn handle<B: Bus + Send + Sync + 'static>(
    State(gateway): State<HttpGateway<B>>,
    request: Request,
) -> Response {
    respond(gateway.shared(), request).await
}

/// Route targets recognised by path (RFC-0002 section 4, RFC-0004).
enum Target<'a> {
    /// `GET /.well-known/agent-card.json`: card of the default agent.
    RootCard,
    /// `GET /agents/{id}/.well-known/agent-card.json`.
    AgentCard(&'a str),
    /// `POST /` or `POST /agents/{id}` (optionally with a trailing slash).
    Rpc(Option<&'a str>),
}

fn classify(path: &str) -> Option<Target<'_>> {
    if path == CARD_PATH {
        return Some(Target::RootCard);
    }
    if path == "/" {
        return Some(Target::Rpc(None));
    }
    let rest = path.strip_prefix("/agents/")?;
    if let Some(agent) = rest.strip_suffix(CARD_SUFFIX) {
        return segment(agent).map(Target::AgentCard);
    }
    let agent = rest.strip_suffix('/').unwrap_or(rest);
    segment(agent).map(|agent| Target::Rpc(Some(agent)))
}

fn segment(value: &str) -> Option<&str> {
    (!value.is_empty() && !value.contains('/')).then_some(value)
}

async fn respond<B: Bus + Send + Sync + 'static>(
    shared: &Arc<Shared<B>>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let Some(source) = parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip())
    else {
        return error_response(&Value::Null, &GwError::internal());
    };
    if let Some(error) = origin_or_host_error(&shared.config, &parts.headers) {
        return error_response(&Value::Null, &error);
    }
    let Some(target) = classify(parts.uri.path()) else {
        return status_response(StatusCode::NOT_FOUND);
    };
    let expected = match target {
        Target::Rpc(_) => Method::POST,
        Target::RootCard | Target::AgentCard(_) => Method::GET,
    };
    if parts.method != expected {
        return status_response(StatusCode::METHOD_NOT_ALLOWED);
    }
    match target {
        Target::RootCard => card_response(shared, None, &parts.headers),
        Target::AgentCard(agent) => card_response(shared, Some(agent), &parts.headers),
        Target::Rpc(agent) => rpc_response(shared, &parts, source, agent, body).await,
    }
}

/// Steps 1 of 0008-020: browser-origin and Host checks.
fn origin_or_host_error(config: &GatewayConfig, headers: &HeaderMap) -> Option<GwError> {
    if headers.contains_key(ORIGIN) {
        return Some(GwError::http(
            StatusCode::FORBIDDEN,
            INVALID_REQUEST,
            "ORIGIN_NOT_ALLOWED",
            "requests with an Origin header are refused",
        ));
    }
    if config.listen.ip().is_loopback() && !host_allowed(config, headers) {
        return Some(GwError::http(
            StatusCode::BAD_REQUEST,
            INVALID_REQUEST,
            "HOST_NOT_ALLOWED",
            "Host header is not an allowed name for this listener",
        ));
    }
    None
}

/// Accepts loopback names for the listener and the host of the public base URL (0008-020 step 1).
fn host_allowed(config: &GatewayConfig, headers: &HeaderMap) -> bool {
    let Some(host) = headers.get(HOST).and_then(|value| value.to_str().ok()) else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    let port = config.listen.port();
    let mut allowed = vec![
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
    ];
    if let Ok(public) = url::Url::parse(&config.public_base_url) {
        allowed.extend(public.host_str().map(|name| match public.port() {
            Some(port) => format!("{name}:{port}"),
            None => name.to_owned(),
        }));
    }
    allowed.contains(&host)
}

async fn rpc_response<B: Bus + Send + Sync + 'static>(
    shared: &Arc<Shared<B>>,
    parts: &Parts,
    source: IpAddr,
    agent: Option<&str>,
    body: Body,
) -> Response {
    if !is_json(&parts.headers) {
        return error_response(
            &Value::Null,
            &GwError::http(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                INVALID_REQUEST,
                "UNSUPPORTED_MEDIA_TYPE",
                "Content-Type must be application/json",
            ),
        );
    }
    let bytes = match to_bytes(body, MAX_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return error_response(
                &Value::Null,
                &GwError::http(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    INVALID_REQUEST,
                    "REQUEST_TOO_LARGE",
                    "request body exceeds 1 MiB",
                ),
            );
        }
    };
    let caller = match authenticate_request(shared, &parts.headers, source) {
        Ok(caller) => caller,
        Err(response) => return *response,
    };
    let config = &shared.config;
    let agent = match resolve_agent(config, &caller, agent) {
        Ok(agent) => agent,
        Err(error) => return error_response(&Value::Null, &error),
    };
    let view = match shared.cards.lookup(agent.as_str()) {
        Lookup::NotReady => return error_response(&Value::Null, &GwError::unavailable()),
        Lookup::Missing => return error_response(&Value::Null, &ops::agent_not_found()),
        Lookup::Found(view) => view,
    };
    let hermes = is_hermes(config, &caller);
    if hermes && !view.required_extensions.is_empty() {
        return error_response(&Value::Null, &ops::agent_not_found());
    }
    let request = match jsonrpc::parse(&bytes) {
        Ok(request) => request,
        Err(rejected) => return error_response(&rejected.id, &rejected.error),
    };
    if !version_ok(&parts.headers) {
        return error_response(&request.id, &GwError::wire(WireError::VersionNotSupported));
    }
    let requested = requested_extensions(&parts.headers);
    if !hermes
        && view
            .required_extensions
            .iter()
            .any(|uri| !requested.contains(uri))
    {
        return error_response(
            &request.id,
            &GwError::wire(WireError::ExtensionSupportRequired),
        );
    }
    if !trusted(config, &caller, source) {
        return error_response(
            &request.id,
            &GwError::http(
                StatusCode::FORBIDDEN,
                PEER_NOT_TRUSTED,
                "PEER_NOT_TRUSTED",
                "caller is not listed in trusted_peers",
            ),
        );
    }
    let limiter = match caller {
        Caller::Loopback => &shared.loopback_limit,
        Caller::Peer(_) => &shared.peer_limit,
    };
    if let Err(delay) = limiter.hit(caller.origin(config), Instant::now()) {
        return error_response(&request.id, &rate_limited(delay));
    }
    let echo = parts
        .headers
        .contains_key(EXTENSIONS_HEADER)
        .then(|| activated(&requested, &view));
    let Some(principal) = Principal::resolve(config, caller) else {
        return error_response(&request.id, &GwError::internal());
    };
    let id = request.id.clone();
    let mut response = if ops::is_streaming(request.method) {
        stream_response(shared, agent, &principal, request).await
    } else {
        match ops::unary(shared, &agent, &principal, request).await {
            Ok(result) => json_response(StatusCode::OK, &jsonrpc::success(&id, result)),
            Err(error) => error_response(&id, &error),
        }
    };
    if let Some(value) = echo {
        response = with_header(response, HeaderName::from_static(EXTENSIONS_HEADER), &value);
    }
    response
}

/// Steps 4 of 0008-020: source-rate check, credential check, and failure accounting.
fn authenticate_request<B: Bus>(
    shared: &Shared<B>,
    headers: &HeaderMap,
    source: IpAddr,
) -> Result<Caller, Box<Response>> {
    let key = source.to_string();
    let now = Instant::now();
    shared
        .failed_auth
        .check(&key, now)
        .map_err(|delay| Box::new(error_response(&Value::Null, &rate_limited(delay))))?;
    match authenticate(&shared.config, headers, source) {
        Ok(caller) => Ok(caller),
        Err(AuthError::Missing | AuthError::Invalid) => {
            let failure = match shared.failed_auth.hit(&key, now) {
                Ok(()) => unauthorized(),
                Err(delay) => rate_limited(delay),
            };
            Err(Box::new(error_response(&Value::Null, &failure)))
        }
    }
}

fn unauthorized() -> GwError {
    GwError::http(
        StatusCode::UNAUTHORIZED,
        UNAUTHORIZED,
        "UNAUTHORIZED",
        "missing or invalid bearer token",
    )
    .challenged()
}

fn rate_limited(delay: Duration) -> GwError {
    GwError::http(
        StatusCode::TOO_MANY_REQUESTS,
        RATE_LIMITED,
        "RATE_LIMITED",
        "rate limit exceeded",
    )
    .with_retry_after(delay)
}

/// Step 5 of 0008-020: resolves the target agent and checks the caller may reach it.
fn resolve_agent(
    config: &GatewayConfig,
    caller: &Caller,
    raw: Option<&str>,
) -> Result<AgentId, GwError> {
    let candidate = match raw {
        Some(raw) => AgentId::parse(raw).ok(),
        None => config.default_agent.clone(),
    };
    match candidate {
        Some(agent) if allowed(config, caller, &agent) => Ok(agent),
        _ if raw.is_none() => Err(GwError::http(
            StatusCode::NOT_FOUND,
            METHOD_NOT_FOUND,
            "NO_AGENT_SELECTED",
            "no agent selected; POST to /agents/{id}",
        )),
        _ => Err(ops::agent_not_found()),
    }
}

fn allowed(config: &GatewayConfig, caller: &Caller, agent: &AgentId) -> bool {
    if !config.is_exposed(agent) {
        return false;
    }
    match caller {
        Caller::Loopback => true,
        Caller::Peer(index) => config
            .peers
            .get(*index)
            .is_some_and(|peer| peer.agents.contains(agent)),
    }
}

fn is_hermes(config: &GatewayConfig, caller: &Caller) -> bool {
    match caller {
        Caller::Loopback => false,
        Caller::Peer(index) => config
            .peers
            .get(*index)
            .is_some_and(|peer| peer.profile == Profile::Hermes),
    }
}

/// Non-loopback sources must be listed in `trusted_peers` (RFC-0008 4.x trust gate).
fn trusted(config: &GatewayConfig, caller: &Caller, source: IpAddr) -> bool {
    match caller {
        Caller::Loopback => true,
        Caller::Peer(index) => {
            source.is_loopback()
                || config
                    .peers
                    .get(*index)
                    .is_some_and(|peer| config.trusted_peers.contains(peer.name()))
        }
    }
}

/// Step 7 of 0008-020: exact protocol version check (0008-027).
fn version_ok(headers: &HeaderMap) -> bool {
    headers
        .get(VERSION_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| matches!(value.trim(), "1.0" | "1.0.0"))
}

fn requested_extensions(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(EXTENSIONS_HEADER)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|uri| !uri.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Extensions to echo: requested URIs the card advertises, excluding those a peer may never
/// activate (0008-031). The gateway never adds URIs on a peer's behalf.
fn activated(requested: &[String], view: &CardView) -> String {
    requested
        .iter()
        .filter(|uri| {
            view.extensions.iter().any(|known| known == *uri)
                && !FORBIDDEN_PREFIXES
                    .iter()
                    .any(|prefix| uri.starts_with(prefix))
        })
        .cloned()
        .collect::<Vec<_>>()
        .join(", ")
}

async fn stream_response<B: Bus + Send + Sync + 'static>(
    shared: &Arc<Shared<B>>,
    agent: AgentId,
    principal: &Principal,
    request: jsonrpc::Request,
) -> Response {
    let id = request.id.clone();
    let mut frames = match ops::open_stream(shared, agent, principal, request) {
        Ok(frames) => frames,
        Err(error) => return error_response(&id, &error),
    };
    match frames.recv().await {
        Some(Ok(first)) => sse_response(id, first, frames),
        Some(Err(error)) => error_response(&id, &error),
        None => error_response(&id, &GwError::internal()),
    }
}

/// Serves a stream as `text/event-stream`: one event per frame, keepalive comments while idle.
fn sse_response(id: Value, first: Value, frames: Frames) -> Response {
    let events = tokio_stream::once(Ok::<Value, GwError>(first))
        .chain(ReceiverStream::new(frames))
        .map(move |frame| {
            let data = match frame {
                Ok(value) => value,
                Err(error) => jsonrpc::failure(&id, &error),
            };
            Ok::<Event, Infallible>(Event::default().data(data.to_string()))
        });
    Sse::new(events)
        .keep_alive(KeepAlive::new().interval(KEEPALIVE).text("keepalive"))
        .into_response()
}

fn card_response<B: Bus>(shared: &Shared<B>, agent: Option<&str>, headers: &HeaderMap) -> Response {
    let config = &shared.config;
    let candidate = match agent {
        Some(raw) => AgentId::parse(raw).ok(),
        None => config.default_agent.clone(),
    };
    let Some(agent) = candidate.filter(|agent| config.is_exposed(agent)) else {
        return error_response(&Value::Null, &ops::agent_not_found());
    };
    match shared.cards.lookup(agent.as_str()) {
        Lookup::NotReady => error_response(&Value::Null, &GwError::unavailable()),
        Lookup::Missing => error_response(&Value::Null, &ops::agent_not_found()),
        Lookup::Found(view) => view_response(&view, headers),
    }
}

/// Serves a card with its ETag, honouring `If-None-Match` (RFC-0004).
fn view_response(view: &CardView, headers: &HeaderMap) -> Response {
    let fresh = headers
        .get_all(IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .any(|tag| tag == "*" || tag.trim_start_matches("W/") == view.etag);
    let mut response = Response::new(if fresh {
        Body::empty()
    } else {
        Body::from(view.body.clone())
    });
    if fresh {
        *response.status_mut() = StatusCode::NOT_MODIFIED;
    } else {
        response
            .headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    }
    let response = with_header(response, ETAG, &view.etag);
    with_header(response, CACHE_CONTROL, CARD_CACHE)
}

fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
}

fn json_response(status: StatusCode, body: &Value) -> Response {
    let mut response = Response::new(Body::from(body.to_string()));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}

fn error_response(id: &Value, error: &GwError) -> Response {
    let mut response = json_response(error.status, &jsonrpc::failure(id, error));
    if let Some(seconds) = error.retry_after {
        response = with_header(response, RETRY_AFTER, &seconds.to_string());
    }
    if error.challenge {
        response = with_header(response, WWW_AUTHENTICATE, "Bearer");
    }
    response
}

fn status_response(status: StatusCode) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    response
}

fn with_header(mut response: Response, name: HeaderName, value: &str) -> Response {
    if let Ok(value) = HeaderValue::from_str(value) {
        response.headers_mut().insert(name, value);
    }
    response
}
