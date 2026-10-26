//! Worker-side IPC endpoint server (feature `runtime`).
//!
//! The worker owns the endpoint. This module accepts shim connections, runs
//! hello/auth against a worker-supplied [`GrantTable`], validates frame shape,
//! and forwards every authenticated frame to the worker as a [`ServerRequest`].
//! The worker answers with [`ServerRequest::respond`], which is callable from
//! any thread and needs no async runtime (RFC-0015 [0015-040..045]).

use std::io;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{ReadHalf, WriteHalf};
use tokio::sync::{Mutex, oneshot};

use crate::endpoint::{Listener, Stream, read_message, write_frame};
use crate::ipc::{
    self, Auth, AuthReply, ClientFrame, Hello, HelloReply, IPC_VERSION, IpcError, Response, Secret,
    reason,
};

/// Time allowed from accept to a successful auth (RFC-0015 [0015-043]).
const AUTH_TIMEOUT: Duration = Duration::from_secs(5);
/// Failed auth attempts on one connection before it is closed.
const MAX_AUTH_FAILURES: u32 = 5;
const ACCEPT_RETRY: Duration = Duration::from_millis(50);

/// Worker-supplied credential check.
pub trait GrantTable: Send + Sync {
    /// Verifies a grant id, credential, and session slot.
    ///
    /// Unknown grant and wrong credential must both return
    /// [`reason::UNAUTHENTICATED`] with the same message. Revoked grants return
    /// [`reason::GRANT_REVOKED`]; an unbound slot returns
    /// [`reason::SLOT_UNBOUND`]. Use [`ipc::constant_time_eq`] for credential
    /// comparison.
    fn authenticate(
        &self,
        grant_id: &str,
        credential: &Secret,
        session_slot: &str,
    ) -> Result<(), IpcError>;
}

/// One authenticated, shape-valid request forwarded to the worker.
#[derive(Debug)]
pub struct ServerRequest {
    /// Session slot the connection authenticated with.
    pub slot: String,
    /// The request frame, including `cancel-call`.
    pub frame: ClientFrame,
    responder: oneshot::Sender<Response>,
}

impl ServerRequest {
    /// Sends the response to the shim. Drops silently if the connection is gone.
    pub fn respond(self, response: Response) {
        let _ = self.responder.send(response);
    }
}

/// Keeps the listener alive. Dropping it stops accepting and closes the
/// server thread.
#[derive(Debug)]
pub struct ServerHandle {
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Binds `endpoint` and serves shim connections on a dedicated thread.
///
/// Binding happens before this returns, so bind errors surface here. Every
/// authenticated frame is sent on `requests` in arrival order.
pub fn spawn(
    endpoint: &str,
    grants: Arc<dyn GrantTable>,
    requests: std::sync::mpsc::Sender<ServerRequest>,
) -> io::Result<ServerHandle> {
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<io::Result<()>>(1);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let endpoint = endpoint.to_owned();
    let thread = std::thread::Builder::new()
        .name("a2a-mcp-ipc-server".to_owned())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    let _ = ready_tx.send(Err(err));
                    return;
                }
            };
            runtime.block_on(async move {
                let listener = match Listener::bind(&endpoint) {
                    Ok(listener) => {
                        let _ = ready_tx.send(Ok(()));
                        listener
                    }
                    Err(err) => {
                        let _ = ready_tx.send(Err(err));
                        return;
                    }
                };
                accept_loop(listener, grants, requests, shutdown_rx).await;
            });
        })?;
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(ServerHandle {
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
        }),
        Ok(Err(err)) => {
            let _ = thread.join();
            Err(err)
        }
        Err(_) => Err(io::Error::other("ipc server thread exited before binding")),
    }
}

async fn accept_loop(
    mut listener: Listener,
    grants: Arc<dyn GrantTable>,
    requests: std::sync::mpsc::Sender<ServerRequest>,
    mut shutdown: oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => match accepted {
                Ok(stream) => {
                    tokio::spawn(connection(
                        stream,
                        Arc::clone(&grants),
                        requests.clone(),
                    ));
                }
                Err(_) => tokio::time::sleep(ACCEPT_RETRY).await,
            },
        }
    }
}

type Reader = ReadHalf<Box<dyn Stream>>;
type Writer = Arc<Mutex<WriteHalf<Box<dyn Stream>>>>;

async fn connection(
    stream: Box<dyn Stream>,
    grants: Arc<dyn GrantTable>,
    requests: std::sync::mpsc::Sender<ServerRequest>,
) {
    let (mut reader, writer) = tokio::io::split(stream);
    let writer: Writer = Arc::new(Mutex::new(writer));
    let slot = match tokio::time::timeout(
        AUTH_TIMEOUT,
        handshake(&mut reader, &writer, grants.as_ref()),
    )
    .await
    {
        Ok(Some(slot)) => slot,
        _ => return,
    };

    loop {
        let frame: ClientFrame = match read_message(&mut reader).await {
            Ok(frame) => frame,
            Err(_) => return,
        };
        let id = frame.id();
        if let Err(error) = validate_frame(&frame) {
            if write_locked(&writer, &Response::failure(id, error))
                .await
                .is_none()
            {
                return;
            }
            continue;
        }
        let (responder, response_rx) = oneshot::channel();
        let request = ServerRequest {
            slot: slot.clone(),
            frame,
            responder,
        };
        if requests.send(request).is_err() {
            let error = IpcError::new(reason::UNAVAILABLE);
            let _ = write_locked(&writer, &Response::failure(id, error)).await;
            return;
        }
        let writer = Arc::clone(&writer);
        tokio::spawn(async move {
            if let Ok(response) = response_rx.await {
                let _ = write_locked(&writer, &response).await;
            }
        });
    }
}

/// Runs hello and auth. Returns the authenticated session slot.
async fn handshake(
    reader: &mut Reader,
    writer: &Writer,
    grants: &dyn GrantTable,
) -> Option<String> {
    let hello: Hello = read_message(reader).await.ok()?;
    if hello.ipc_version != IPC_VERSION {
        let reply = HelloReply::reject(IpcError::new(reason::IPC_VERSION_UNSUPPORTED));
        write_locked(writer, &reply).await?;
        return None;
    }
    write_locked(writer, &HelloReply::accept()).await?;

    let mut failures = 0u32;
    loop {
        let auth: Auth = read_message(reader).await.ok()?;
        match grants.authenticate(&auth.grant_id, &auth.grant_credential, &auth.session_slot) {
            Ok(()) => {
                write_locked(writer, &AuthReply::accept()).await?;
                return Some(auth.session_slot);
            }
            Err(error) => {
                write_locked(writer, &AuthReply::reject(error)).await?;
                failures += 1;
                if failures >= MAX_AUTH_FAILURES {
                    return None;
                }
            }
        }
    }
}

/// Shape checks before forwarding: deadline range and params validators.
fn validate_frame(frame: &ClientFrame) -> Result<(), IpcError> {
    if let Some(deadline_ms) = frame.deadline_ms() {
        ipc::validate_deadline(deadline_ms)?;
    }
    match frame {
        ClientFrame::Discover { params, .. } => ipc::validate_discover(params),
        ClientFrame::Send { params, .. } => ipc::validate_send(params),
        ClientFrame::Get { params, .. } => ipc::validate_get(params),
        ClientFrame::Cancel { params, .. } => ipc::validate_cancel(params),
        ClientFrame::CancelCall { .. } => Ok(()),
    }
}

async fn write_locked<T>(writer: &Writer, value: &T) -> Option<()>
where
    T: serde::Serialize + ?Sized,
{
    let mut guard = writer.lock().await;
    write_frame(&mut *guard, value).await.ok()
}
