//! Loopback health and metrics endpoints of the supervisor (RFC-0013 sec 4.x, 0013-033..037).
//! The server is a blocking, std-only HTTP/1.1 responder. It serves `GET /healthz`,
//! `GET /readyz` and `GET /metrics`. The state it reports is held in a [`HealthHandle`]
//! that the supervisor updates as the bus, the audit sink and the agents change.
//! Only loopback addresses can be bound (0013-033). No endpoint requires credentials and
//! no body carries paths, configuration or credential names (0013-037).

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use a2a_audit::health::{self, Health, HealthError, Response};
use a2a_audit::metrics::{Metrics, MetricsError};

/// How often the accept loop re-checks the stop flag.
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Read and write timeout for one connection.
const IO_TIMEOUT: Duration = Duration::from_secs(2);
/// Largest request head accepted; anything larger is refused.
const MAX_REQUEST_HEAD: usize = 8 * 1024;

/// Errors from binding or running the health listener.
#[derive(Debug, thiserror::Error)]
pub enum HealthServeError {
    /// The requested address is not loopback (0013-033).
    #[error(transparent)]
    Loopback(#[from] HealthError),
    /// The listener could not be bound.
    #[error("cannot bind health listener on {address}: {source}")]
    Bind {
        /// Requested listen address.
        address: SocketAddr,
        /// Underlying OS error.
        source: io::Error,
    },
    /// The accept loop failed.
    #[error("health listener failed: {0}")]
    Io(#[from] io::Error),
}

#[derive(Debug)]
struct Inner {
    health: Health,
    metrics: Metrics,
}

/// Shared state read by the health endpoints and written by the supervisor.
///
/// Cloning is cheap; all clones observe the same state.
#[derive(Debug, Clone)]
pub struct HealthHandle {
    inner: Arc<Mutex<Inner>>,
}

impl HealthHandle {
    /// Creates a handle with the bus disconnected, the sink not writing, an empty spool and no
    /// agents. The supervisor updates the state as it starts.
    #[must_use]
    pub fn new() -> Self {
        let health = Health {
            bus_connected: false,
            sink_writing: false,
            spool_ratio: 0.0,
            agents: std::collections::BTreeMap::new(),
        };
        Self {
            inner: Arc::new(Mutex::new(Inner {
                health,
                metrics: Metrics::new(),
            })),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Applies `change` to the health state.
    pub fn update(&self, change: impl FnOnce(&mut Health)) {
        change(&mut self.lock().health);
    }

    /// Copy of the current health state.
    #[must_use]
    pub fn snapshot(&self) -> Health {
        self.lock().health.clone()
    }

    /// Adds `by` to a declared counter series.
    ///
    /// # Errors
    /// Returns [`MetricsError`] for an undeclared metric or a label mismatch.
    pub fn inc(&self, name: &str, labels: &[&str], by: u64) -> Result<(), MetricsError> {
        self.lock().metrics.inc(name, labels, by)
    }

    /// Sets a declared gauge series.
    ///
    /// # Errors
    /// Returns [`MetricsError`] for an undeclared metric, a label mismatch or a non-finite value.
    pub fn set(&self, name: &str, labels: &[&str], value: f64) -> Result<(), MetricsError> {
        self.lock().metrics.set(name, labels, value)
    }

    /// Records one observation in a declared histogram series.
    ///
    /// # Errors
    /// Returns [`MetricsError`] for an undeclared metric, a label mismatch or a non-finite value.
    pub fn observe(&self, name: &str, labels: &[&str], value: f64) -> Result<(), MetricsError> {
        self.lock().metrics.observe(name, labels, value)
    }

    /// Renders every declared metric in the Prometheus text exposition format.
    #[must_use]
    pub fn render_metrics(&self) -> String {
        self.lock().metrics.render()
    }
}

impl Default for HealthHandle {
    fn default() -> Self {
        Self::new()
    }
}

/// A bound, loopback-only health listener that has not started accepting yet.
#[derive(Debug)]
pub struct HealthServer {
    listener: TcpListener,
}

impl HealthServer {
    /// Binds the listener. Non-loopback addresses are refused before any socket is opened
    /// (0013-033).
    ///
    /// # Errors
    /// Returns [`HealthServeError::Loopback`] for a non-loopback address and
    /// [`HealthServeError::Bind`] when the OS refuses the bind.
    pub fn bind(address: SocketAddr) -> Result<Self, HealthServeError> {
        health::require_loopback(address.ip())?;
        let listener = TcpListener::bind(address)
            .map_err(|source| HealthServeError::Bind { address, source })?;
        Ok(Self { listener })
    }

    /// Address actually bound (useful when port 0 was requested).
    ///
    /// # Errors
    /// Returns the OS error if the socket address cannot be read.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Accepts connections until `stop` is set. Connections are handled one at a time with a
    /// short I/O timeout, which is enough for loopback probes.
    ///
    /// # Errors
    /// Returns [`HealthServeError::Io`] when the listener fails.
    pub fn run(self, handle: &HealthHandle, stop: &AtomicBool) -> Result<(), HealthServeError> {
        self.listener.set_nonblocking(true)?;
        while !stop.load(Ordering::SeqCst) {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    // Per-connection failures (bad client, timeout) must not stop the probe
                    // endpoint, so they are ignored here.
                    let _ = handle_connection(stream, handle);
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(POLL_INTERVAL);
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => return Err(HealthServeError::Io(err)),
            }
        }
        Ok(())
    }
}

/// Binds `listen` and serves until `stop` is set.
///
/// # Errors
/// See [`HealthServer::bind`] and [`HealthServer::run`].
pub fn serve(
    listen: SocketAddr,
    handle: &HealthHandle,
    stop: &AtomicBool,
) -> Result<(), HealthServeError> {
    HealthServer::bind(listen)?.run(handle, stop)
}

/// Body and headers of one HTTP response.
struct Reply {
    status: u16,
    content_type: &'static str,
    body: String,
    allow_get: bool,
}

impl Reply {
    fn json(response: Response) -> Self {
        Self {
            status: response.status,
            content_type: "application/json",
            body: response.body,
            allow_get: false,
        }
    }

    fn text(status: u16, body: &str) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: body.to_owned(),
            allow_get: status == 405,
        }
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    }
}

/// Maps a request line to a reply. Only `GET` is served (0013-036).
fn route(handle: &HealthHandle, method: &str, target: &str) -> Reply {
    let path = target.split('?').next().unwrap_or(target);
    if method != "GET" {
        return Reply::text(405, "method not allowed\n");
    }
    match path {
        "/healthz" => Reply::json(health::healthz(&handle.snapshot())),
        "/readyz" => Reply::json(health::readyz(&handle.snapshot())),
        "/metrics" => Reply {
            status: 200,
            content_type: "text/plain; version=0.0.4; charset=utf-8",
            body: handle.render_metrics(),
            allow_get: false,
        },
        _ => Reply::text(404, "not found\n"),
    }
}

/// Reads the request head up to the blank line and returns `(method, target)`.
fn read_request_line(stream: &mut TcpStream) -> io::Result<Option<(String, String)>> {
    let mut head = Vec::with_capacity(512);
    let mut chunk = [0_u8; 512];
    loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        head.extend_from_slice(&chunk[..read]);
        if head.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if head.len() > MAX_REQUEST_HEAD {
            return Ok(None);
        }
    }
    let text = String::from_utf8_lossy(&head);
    let Some(line) = text.lines().next() else {
        return Ok(None);
    };
    let mut parts = line.split(' ');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(method), Some(target), Some(version))
            if version.starts_with("HTTP/") && !method.is_empty() && !target.is_empty() =>
        {
            Ok(Some((method.to_owned(), target.to_owned())))
        }
        _ => Ok(None),
    }
}

fn write_reply(stream: &mut TcpStream, reply: &Reply) -> io::Result<()> {
    let mut out = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n",
        reply.status,
        reason(reply.status),
        reply.content_type,
        reply.body.len(),
    );
    if reply.allow_get {
        out.push_str("Allow: GET\r\n");
    }
    out.push_str("\r\n");
    out.push_str(&reply.body);
    stream.write_all(out.as_bytes())?;
    stream.flush()
}

fn handle_connection(mut stream: TcpStream, handle: &HealthHandle) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let reply = match read_request_line(&mut stream)? {
        Some((method, target)) => route(handle, &method, &target),
        None => Reply::text(400, "bad request\n"),
    };
    write_reply(&mut stream, &reply)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn loopback() -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
    }

    fn ready_state(handle: &HealthHandle) {
        handle.update(|health| {
            health.bus_connected = true;
            health.sink_writing = true;
            health.spool_ratio = 0.1;
            health.agents.insert("omp".to_owned(), "running".to_owned());
        });
    }

    /// Runs one request against a live listener and returns the raw response text.
    fn exchange(handle: &HealthHandle, request: &str) -> String {
        let server = HealthServer::bind(loopback()).unwrap();
        let address = server.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_handle = handle.clone();
        let worker_stop = Arc::clone(&stop);
        let join = thread::spawn(move || server.run(&worker_handle, &worker_stop));
        let mut client = TcpStream::connect(address).unwrap();
        client.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        stop.store(true, Ordering::SeqCst);
        join.join().unwrap().unwrap();
        response
    }

    fn status_line(response: &str) -> &str {
        response.lines().next().unwrap_or_default()
    }

    fn body_of(response: &str) -> &str {
        response.split_once("\r\n\r\n").map_or("", |(_, body)| body)
    }

    // 0013-033: the listener refuses any non-loopback address before binding.
    #[test]
    fn req_0013_033_refuses_non_loopback_bind() {
        let address = SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0));
        let err = HealthServer::bind(address).unwrap_err();
        assert!(matches!(err, HealthServeError::Loopback(_)));
    }

    #[test]
    fn req_0013_033_accepts_loopback_bind() {
        let server = HealthServer::bind(loopback()).unwrap();
        assert!(server.local_addr().unwrap().ip().is_loopback());
    }

    // 0013-036: /healthz is 200 regardless of dependency state; /readyz needs all three checks.
    #[test]
    fn req_0013_036_healthz_is_ok_while_not_ready() {
        let handle = HealthHandle::new();
        let response = exchange(&handle, "GET /healthz HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert_eq!(status_line(&response), "HTTP/1.1 200 OK");
    }

    #[test]
    fn req_0013_036_readyz_is_unavailable_when_bus_down() {
        let handle = HealthHandle::new();
        let response = exchange(&handle, "GET /readyz HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert_eq!(status_line(&response), "HTTP/1.1 503 Service Unavailable");
    }

    #[test]
    fn req_0013_036_readyz_is_ok_when_all_checks_pass() {
        let handle = HealthHandle::new();
        ready_state(&handle);
        let response = exchange(&handle, "GET /readyz HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert_eq!(status_line(&response), "HTTP/1.1 200 OK");
    }

    #[test]
    fn req_0013_036_readyz_is_unavailable_when_spool_over_80_percent() {
        let handle = HealthHandle::new();
        ready_state(&handle);
        handle.update(|health| health.spool_ratio = 0.9);
        let response = exchange(&handle, "GET /readyz HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert_eq!(status_line(&response), "HTTP/1.1 503 Service Unavailable");
    }

    // 0013-037: the body has the fixed shape and carries no paths or credential names.
    #[test]
    fn req_0013_037_body_has_fixed_shape_without_paths() {
        let handle = HealthHandle::new();
        ready_state(&handle);
        let response = exchange(&handle, "GET /healthz HTTP/1.1\r\nHost: localhost\r\n\r\n");
        let body: serde_json::Value = serde_json::from_str(body_of(&response)).unwrap();
        assert!(body.get("status").is_some());
        assert!(body.get("bus").is_some());
        assert!(body.get("sink").is_some());
        assert!(body.get("agents").is_some());
        let text = body_of(&response);
        assert!(!text.contains('\\'));
        assert!(!text.contains('/'));
        assert!(!text.contains("ANTHROPIC"));
    }

    // 0013-036: only GET is served.
    #[test]
    fn req_0013_036_non_get_is_method_not_allowed() {
        let handle = HealthHandle::new();
        let response = exchange(&handle, "POST /healthz HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert_eq!(status_line(&response), "HTTP/1.1 405 Method Not Allowed");
        assert!(response.contains("Allow: GET\r\n"));
    }

    #[test]
    fn req_0013_036_unknown_path_is_not_found() {
        let handle = HealthHandle::new();
        let response = exchange(&handle, "GET /nope HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert_eq!(status_line(&response), "HTTP/1.1 404 Not Found");
    }

    #[test]
    fn req_0013_036_malformed_request_is_bad_request() {
        let handle = HealthHandle::new();
        let response = exchange(&handle, "garbage\r\n\r\n");
        assert_eq!(status_line(&response), "HTTP/1.1 400 Bad Request");
    }

    // 0013-033: /metrics renders the declared a2abus_ series on the loopback listener.
    #[test]
    fn req_0013_033_metrics_served_on_loopback() {
        let handle = HealthHandle::new();
        handle.inc("a2abus_restarts_total", &["omp"], 2).unwrap();
        let response = exchange(&handle, "GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert_eq!(status_line(&response), "HTTP/1.1 200 OK");
        let body = body_of(&response);
        assert!(body.contains("a2abus_restarts_total{agent=\"omp\"} 2"));
    }

    #[test]
    fn req_0013_035_undeclared_metric_rejected() {
        let handle = HealthHandle::new();
        assert!(handle.inc("not_declared_total", &[], 1).is_err());
    }

    // The stop flag ends the accept loop without a connection.
    #[test]
    fn stop_flag_ends_accept_loop() {
        let server = HealthServer::bind(loopback()).unwrap();
        let handle = HealthHandle::new();
        let stop = AtomicBool::new(true);
        server.run(&handle, &stop).unwrap();
    }
}
