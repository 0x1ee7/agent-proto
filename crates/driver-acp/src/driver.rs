//! Public handle implementing the worker `Backend` seam (RFC-0009 4.2, 4.9).
//!
//! Every call is a message to the actor thread in [`crate::core`]. The handle
//! holds no protocol state; it only waits for replies.

use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use a2a_worker::backend::{
    Backend, BackendError, BackendSink, Capabilities, Health, PermissionAnswer, Protocol,
    RequestId, StartTurn, TurnRef, UsageSource,
};

use crate::config::{AcpDriverConfig, ConfigError, Profile};
use crate::core::{Command, Core, Msg};
use crate::mode;
use crate::process::{OsSpawner, Spawner};

/// ACP stdio driver for one agent instance.
///
/// Dropping the driver sends a graceful shutdown (close stdin, then kill the
/// tree after `shutdown_grace`) and joins the actor thread.
pub struct AcpDriver {
    tx: Sender<Msg>,
    thread: Option<JoinHandle<()>>,
    capabilities: Capabilities,
    shutdown_grace: Duration,
}

impl std::fmt::Debug for AcpDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpDriver")
            .field("capabilities", &self.capabilities)
            .field("shutdown_grace", &self.shutdown_grace)
            .finish_non_exhaustive()
    }
}

impl AcpDriver {
    /// Validates `config` and starts the driver actor. The agent process is
    /// spawned lazily when the first turn starts.
    pub fn new(config: AcpDriverConfig) -> Result<Self, ConfigError> {
        Self::with_spawner(config, Box::new(OsSpawner))
    }

    pub(crate) fn with_spawner(
        config: AcpDriverConfig,
        spawner: Box<dyn Spawner>,
    ) -> Result<Self, ConfigError> {
        config.validate()?;
        let capabilities = capabilities(config.profile);
        let shutdown_grace = config.shutdown_grace;
        let (tx, rx) = mpsc::channel();
        let core = Core::new(config, spawner, tx.clone(), rx);
        let thread = thread::Builder::new()
            .name("driver-acp".to_owned())
            .spawn(move || core.run())
            .map_err(|error| ConfigError::ThreadStart(error.to_string()))?;
        Ok(Self {
            tx,
            thread: Some(thread),
            capabilities,
            shutdown_grace,
        })
    }

    /// Sends `Shutdown`, waits for the actor to finish, and joins its thread.
    fn stop(&mut self, grace: Duration) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        let (reply, done) = mpsc::channel();
        if self
            .tx
            .send(Msg::Command(Command::Shutdown(grace, reply)))
            .is_ok()
        {
            let _ = done.recv();
        }
        let _ = thread.join();
    }
}

/// Capability record for a profile (0009-011). Approval modes are the modes that run without
/// operator acknowledgement; dangerous modes need `acknowledge_dangerous`.
fn capabilities(profile: Profile) -> Capabilities {
    Capabilities {
        protocol: Protocol::Acp,
        resume: true,
        cancel: true,
        usage_sources: vec![UsageSource::AcpUsage, UsageSource::DriverCounted],
        approval_modes: mode::safe_modes(profile)
            .iter()
            .map(|m| (*m).to_owned())
            .collect(),
        streaming_text: true,
    }
}

impl Backend for AcpDriver {
    fn capabilities(&self) -> Capabilities {
        self.capabilities.clone()
    }

    fn health(&self) -> Health {
        let dead = Health {
            live: false,
            restarts: 0,
        };
        let (reply, answer) = mpsc::channel();
        if self.tx.send(Msg::Command(Command::Health(reply))).is_err() {
            return dead;
        }
        answer.recv().unwrap_or(dead)
    }

    fn attach(&mut self, sink: Box<dyn BackendSink>) {
        let _ = self.tx.send(Msg::Command(Command::Attach(sink)));
    }

    fn start(&mut self, cmd: StartTurn) -> Result<(), BackendError> {
        let (reply, answer) = mpsc::channel();
        if self
            .tx
            .send(Msg::Command(Command::Start(Box::new(cmd), reply)))
            .is_err()
        {
            return Err(BackendError::Closed);
        }
        answer.recv().unwrap_or(Err(BackendError::Closed))
    }

    /// Delivers the operator's answer to a pending permission or elicitation (0009-033).
    /// Unknown or already-answered request ids are ignored.
    fn resolve_permission(
        &mut self,
        request_id: &RequestId,
        answer: PermissionAnswer,
    ) -> Result<(), BackendError> {
        self.tx
            .send(Msg::Command(Command::Resolve(request_id.clone(), answer)))
            .map_err(|_| BackendError::Closed)
    }

    fn cancel(&mut self, turn: &TurnRef) -> Result<(), BackendError> {
        let (reply, answer) = mpsc::channel();
        if self
            .tx
            .send(Msg::Command(Command::Cancel(turn.clone(), reply)))
            .is_err()
        {
            return Err(BackendError::Closed);
        }
        answer.recv().unwrap_or(Err(BackendError::Closed))
    }

    fn shutdown(&mut self, grace: Duration) -> Result<(), BackendError> {
        self.stop(grace);
        Ok(())
    }
}

impl Drop for AcpDriver {
    fn drop(&mut self) {
        let grace = self.shutdown_grace;
        self.stop(grace);
    }
}
