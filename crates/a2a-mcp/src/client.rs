//! IPC client actor (feature `runtime`): connects to the worker endpoint,
//! runs the hello/auth handshake, routes responses to waiting calls, sends
//! `cancel-call`, and reconnects on connection loss (RFC-0015 [0015-040..045]).
//!
//! Pending calls survive a connection loss: they are resent on the next
//! connection with their original ids. Fatal handshake results
//! (`IPC_VERSION_UNSUPPORTED`, `UNAUTHENTICATED`, `GRANT_REVOKED`) are not
//! retried; every pending and future call then fails with that error.

use std::collections::HashMap;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::endpoint::{self, Stream, read_message, write_frame};
use crate::ipc::{
    AuthReply, ClientFrame, Hello, HelloReply, IPC_VERSION, IpcError, Response, Secret, reason,
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const BACKOFF_MIN: Duration = Duration::from_millis(100);
const BACKOFF_MAX: Duration = Duration::from_secs(2);
const COMMAND_CAPACITY: usize = 64;

/// Connection parameters for the actor.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub endpoint: String,
    /// Base64url grant id, as given on the command line.
    pub grant_id: String,
    /// Grant credential from the environment; `None` means it was absent or
    /// malformed and every call fails with `UNAUTHENTICATED`.
    pub credential: Option<Secret>,
    /// Base64url session slot, as given on the command line.
    pub session_slot: String,
}

/// Outcome of [`IpcClient::call`].
#[derive(Debug)]
pub enum CallOutcome {
    /// The worker answered, or the call failed locally (deadline, unavailable).
    Done(Result<Response, IpcError>),
    /// The call was cancelled; a `cancel-call` frame was sent and no response is
    /// produced for it.
    Cancelled,
}

enum Command {
    Call {
        frame: ClientFrame,
        reply: oneshot::Sender<Result<Response, IpcError>>,
        id_tx: oneshot::Sender<u64>,
    },
    CancelCall {
        call_id: u64,
    },
    Forget {
        id: u64,
    },
}

/// Handle to the IPC actor. Cloning is not needed; share it behind `Arc`.
#[derive(Debug)]
pub struct IpcClient {
    commands: mpsc::Sender<Command>,
}

/// Starts the actor on the current tokio runtime.
pub fn spawn(config: ClientConfig) -> IpcClient {
    let (commands, rx) = mpsc::channel(COMMAND_CAPACITY);
    tokio::spawn(Actor::new(config, rx).run());
    IpcClient { commands }
}

impl IpcClient {
    /// Sends `frame` (its `id` is replaced by the actor) and waits for the
    /// response, the deadline, or `cancel`.
    ///
    /// On deadline the call fails locally with `DEADLINE_EXCEEDED`. On cancel a
    /// `cancel-call` frame is sent and [`CallOutcome::Cancelled`] is returned.
    pub async fn call(
        &self,
        frame: ClientFrame,
        deadline: Instant,
        mut cancel: oneshot::Receiver<()>,
    ) -> CallOutcome {
        let (reply, mut reply_rx) = oneshot::channel();
        let (id_tx, id_rx) = oneshot::channel();
        if self
            .commands
            .send(Command::Call {
                frame,
                reply,
                id_tx,
            })
            .await
            .is_err()
        {
            return CallOutcome::Done(Err(IpcError::new(reason::UNAVAILABLE)));
        }
        let Ok(id) = id_rx.await else {
            return CallOutcome::Done(Err(IpcError::new(reason::UNAVAILABLE)));
        };
        tokio::select! {
            biased;
            result = &mut reply_rx => {
                CallOutcome::Done(result.unwrap_or_else(|_| Err(IpcError::new(reason::UNAVAILABLE))))
            }
            () = wait_cancel(&mut cancel) => {
                let _ = self.commands.send(Command::CancelCall { call_id: id }).await;
                CallOutcome::Cancelled
            }
            () = tokio::time::sleep_until(deadline) => {
                let _ = self.commands.send(Command::Forget { id }).await;
                CallOutcome::Done(Err(IpcError::new(reason::DEADLINE_EXCEEDED)))
            }
        }
    }
}

/// Resolves on an explicit cancel. A dropped sender means "never cancelled".
async fn wait_cancel(cancel: &mut oneshot::Receiver<()>) {
    if cancel.await.is_err() {
        std::future::pending::<()>().await;
    }
}

struct Pending {
    frame: ClientFrame,
    reply: oneshot::Sender<Result<Response, IpcError>>,
}

enum ConnectError {
    /// Transient: connect or I/O failure; retry with backoff.
    Retry,
    /// Fatal: do not retry; fail every call with this error.
    Fatal(IpcError),
}

enum SessionEnd {
    /// The command channel closed: the handle was dropped.
    Closed,
    /// The connection was lost; reconnect.
    Lost,
}

struct Actor {
    config: ClientConfig,
    commands: mpsc::Receiver<Command>,
    pending: HashMap<u64, Pending>,
    next_id: u64,
}

impl Actor {
    fn new(config: ClientConfig, commands: mpsc::Receiver<Command>) -> Self {
        Self {
            config,
            commands,
            pending: HashMap::new(),
            next_id: 1,
        }
    }

    fn allocate_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    async fn run(mut self) {
        let mut delay = Duration::ZERO;
        let mut backoff = BACKOFF_MIN;
        loop {
            if !delay.is_zero() && self.idle(delay).await.is_err() {
                return;
            }
            match handshake(&self.config).await {
                Ok(stream) => {
                    backoff = BACKOFF_MIN;
                    match self.session(stream).await {
                        SessionEnd::Closed => return,
                        // Reconnect immediately once; failures then back off.
                        SessionEnd::Lost => delay = Duration::ZERO,
                    }
                }
                Err(ConnectError::Fatal(error)) => {
                    self.fail_all(error).await;
                    return;
                }
                Err(ConnectError::Retry) => {
                    delay = backoff;
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                }
            }
        }
    }

    /// Waits `delay` while accepting commands. Returns `Err` when the handle
    /// has been dropped.
    async fn idle(&mut self, delay: Duration) -> Result<(), ()> {
        let sleep = tokio::time::sleep(delay);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                () = &mut sleep => return Ok(()),
                command = self.commands.recv() => {
                    let Some(command) = command else { return Err(()) };
                    self.handle_offline(command);
                }
            }
        }
    }

    /// Command handling while no connection is up: calls queue, cancels drop
    /// the pending entry (no frame can be sent).
    fn handle_offline(&mut self, command: Command) {
        match command {
            Command::Call {
                frame,
                reply,
                id_tx,
            } => {
                let id = self.allocate_id();
                let _ = id_tx.send(id);
                self.pending.insert(
                    id,
                    Pending {
                        frame: with_id(frame, id),
                        reply,
                    },
                );
            }
            Command::CancelCall { call_id } => {
                self.pending.remove(&call_id);
            }
            Command::Forget { id } => {
                self.pending.remove(&id);
            }
        }
    }

    /// Fatal state: answer every pending and future call with `error`.
    async fn fail_all(&mut self, error: IpcError) {
        for (_, pending) in self.pending.drain() {
            let _ = pending.reply.send(Err(error.clone()));
        }
        while let Some(command) = self.commands.recv().await {
            match command {
                Command::Call { reply, id_tx, .. } => {
                    let id = self.allocate_id();
                    let _ = id_tx.send(id);
                    let _ = reply.send(Err(error.clone()));
                }
                Command::CancelCall { .. } | Command::Forget { .. } => {}
            }
        }
    }

    async fn session(&mut self, stream: Box<dyn Stream>) -> SessionEnd {
        let (mut reader, mut writer) = tokio::io::split(stream);
        let (events_tx, mut events) = mpsc::channel::<Response>(COMMAND_CAPACITY);
        let read_task = tokio::spawn(async move {
            while let Ok(response) = read_message::<_, Response>(&mut reader).await {
                if events_tx.send(response).await.is_err() {
                    break;
                }
            }
        });
        let end = self.drive(&mut writer, &mut events).await;
        read_task.abort();
        end
    }

    async fn drive(
        &mut self,
        writer: &mut tokio::io::WriteHalf<Box<dyn Stream>>,
        events: &mut mpsc::Receiver<Response>,
    ) -> SessionEnd {
        for pending in self.pending.values() {
            if write_frame(writer, &pending.frame).await.is_err() {
                return SessionEnd::Lost;
            }
        }
        loop {
            tokio::select! {
                command = self.commands.recv() => {
                    let Some(command) = command else { return SessionEnd::Closed };
                    match command {
                        Command::Call { frame, reply, id_tx } => {
                            let id = self.allocate_id();
                            let _ = id_tx.send(id);
                            let frame = with_id(frame, id);
                            let write = write_frame(writer, &frame).await;
                            self.pending.insert(id, Pending { frame, reply });
                            if write.is_err() {
                                return SessionEnd::Lost;
                            }
                        }
                        Command::CancelCall { call_id } => {
                            self.pending.remove(&call_id);
                            let id = self.allocate_id();
                            let frame = ClientFrame::CancelCall { id, call_id };
                            if write_frame(writer, &frame).await.is_err() {
                                return SessionEnd::Lost;
                            }
                        }
                        Command::Forget { id } => {
                            self.pending.remove(&id);
                        }
                    }
                }
                response = events.recv() => {
                    let Some(response) = response else { return SessionEnd::Lost };
                    if let Some(pending) = self.pending.remove(&response.id) {
                        let _ = pending.reply.send(Ok(response));
                    }
                }
            }
        }
    }
}

fn with_id(frame: ClientFrame, id: u64) -> ClientFrame {
    match frame {
        ClientFrame::Discover {
            deadline_ms,
            params,
            ..
        } => ClientFrame::Discover {
            id,
            deadline_ms,
            params,
        },
        ClientFrame::Send {
            deadline_ms,
            params,
            ..
        } => ClientFrame::Send {
            id,
            deadline_ms,
            params,
        },
        ClientFrame::Get {
            deadline_ms,
            params,
            ..
        } => ClientFrame::Get {
            id,
            deadline_ms,
            params,
        },
        ClientFrame::Cancel {
            deadline_ms,
            params,
            ..
        } => ClientFrame::Cancel {
            id,
            deadline_ms,
            params,
        },
        ClientFrame::CancelCall { call_id, .. } => ClientFrame::CancelCall { id, call_id },
    }
}

/// Connects and completes hello/auth (RFC-0015 [0015-041..045]).
async fn handshake(config: &ClientConfig) -> Result<Box<dyn Stream>, ConnectError> {
    let Some(credential) = config.credential.clone() else {
        return Err(ConnectError::Fatal(IpcError::new(reason::UNAUTHENTICATED)));
    };
    let attempt = async {
        let mut stream = endpoint::connect(&config.endpoint)
            .await
            .map_err(|_| ConnectError::Retry)?;
        write_frame(
            &mut stream,
            &Hello {
                ipc_version: IPC_VERSION,
                shim: env!("CARGO_PKG_VERSION").to_owned(),
            },
        )
        .await
        .map_err(|_| ConnectError::Retry)?;
        let hello: HelloReply = read_message(&mut stream)
            .await
            .map_err(|_| ConnectError::Retry)?;
        if !hello.ok || hello.ipc_version != Some(IPC_VERSION) {
            let error = hello
                .error
                .unwrap_or_else(|| IpcError::new(reason::IPC_VERSION_UNSUPPORTED));
            return Err(ConnectError::Fatal(error));
        }
        write_frame(
            &mut stream,
            &crate::ipc::Auth {
                grant_id: config.grant_id.clone(),
                grant_credential: credential,
                session_slot: config.session_slot.clone(),
            },
        )
        .await
        .map_err(|_| ConnectError::Retry)?;
        let reply: AuthReply = read_message(&mut stream)
            .await
            .map_err(|_| ConnectError::Retry)?;
        if !reply.ok {
            let error = reply
                .error
                .unwrap_or_else(|| IpcError::new(reason::UNAUTHENTICATED));
            return Err(ConnectError::Fatal(error));
        }
        Ok(stream)
    };
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, attempt).await {
        Ok(result) => result,
        Err(_) => Err(ConnectError::Retry),
    }
}
