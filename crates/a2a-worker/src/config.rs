//! Worker configuration and builder.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use a2a_core::envelope;
use a2a_core::task;
use a2a_core::task::TaskStore;
use a2a_types::ids::{AgentId, TenantId};
use a2a_workspace::WorkspaceMode;
use bus_api::bus::Bus;
use bus_api::name::Principal;

use crate::audit::AuditRecorder;
use crate::backend::Backend;
use crate::error::WorkerError;
use crate::presence;
use crate::worker::Worker;
use crate::workspace::WorkspaceBinding;

/// Default number of turns open at once across all contexts (0005-054).
pub const DEFAULT_MAX_ACTIVE: usize = 64;

/// Default number of tasks waiting in one context before admission answers QUEUE_FULL
/// (0005-028).
pub const DEFAULT_MAX_WAITING_PER_CONTEXT: usize = 8;

/// Approval modes the worker can enforce (RFC-0006 section 4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalMode {
    /// Every tier prompts the operator (`ask-all`, 0006-004).
    AskAll,
    /// Reads and writes run without a prompt; exec and delete-or-move prompt (`ask-exec`).
    AskExec,
}

/// Immutable worker configuration.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// Tenant this worker serves.
    pub tenant: TenantId,
    /// Agent identity this worker serves.
    pub agent: AgentId,
    /// Bus principal used to serve RPC subjects and publish events.
    pub principal: Principal,
    /// Envelope validation settings.
    pub wire: envelope::Config,
    /// Task engine settings. `owner` always equals the agent identity.
    pub tasks: task::Config,
    /// Approval mode in effect.
    pub approval: ApprovalMode,
    /// Turns open at once across all contexts (0005-054).
    pub max_active: usize,
    /// Tasks waiting in one (origin, context_id) at which admission answers QUEUE_FULL
    /// (0005-028). A context's first task is never refused for queue depth.
    pub max_waiting_per_context: usize,
    /// Largest payload carried inline; larger ones go through blobs.
    pub max_inline: usize,
    /// Workspace lease binding (RFC-0012 sections 4.8 and 4.9); `None` runs turns without leases.
    pub workspace: Option<WorkspaceBinding>,
    /// Card and presence publication (RFC-0004). `None` publishes neither.
    pub publication: Option<presence::Publication>,
    /// Directory approvals report when no workspace lease applies (RFC-0006 0006-006).
    /// Set with [`WorkerBuilder::working_directory`]; the worker never reads the process cwd.
    pub working_directory: PathBuf,
    /// Workspace mode approvals report when no workspace lease applies.
    pub working_mode: WorkspaceMode,
    /// Operator-decision signing (RFC-0010 [0010-079]). `None` keeps signing off. Set with
    /// [`WorkerBuilder::signing`].
    pub signing: Option<Arc<a2a_security::trust::TrustStore>>,
}

/// Builder for [`Worker`]. Required values are supplied to [`WorkerBuilder::new`].
#[derive(Debug, Clone)]
pub struct WorkerBuilder {
    config: WorkerConfig,
    audit_sink: Option<Arc<Mutex<dyn AuditRecorder>>>,
}

impl WorkerBuilder {
    /// Starts a builder with defaults: `ask-all`, the default envelope configuration and
    /// `DEFAULT_MAX_WAITING_PER_CONTEXT` queue quota.
    #[must_use]
    pub fn new(
        tenant: TenantId,
        agent: AgentId,
        principal: Principal,
        mut tasks: task::Config,
    ) -> Self {
        tasks.owner = agent.as_str().to_owned();
        Self {
            config: WorkerConfig {
                tenant,
                agent,
                principal,
                wire: envelope::Config::default(),
                tasks,
                approval: ApprovalMode::AskAll,
                max_active: DEFAULT_MAX_ACTIVE,
                max_waiting_per_context: DEFAULT_MAX_WAITING_PER_CONTEXT,
                max_inline: envelope::MAX_INLINE_BYTES,
                workspace: None,
                publication: None,
                working_directory: PathBuf::new(),
                working_mode: WorkspaceMode::PerSession,
                signing: None,
            },
            audit_sink: None,
        }
    }

    /// Replaces the envelope configuration.
    #[must_use]
    pub fn wire(mut self, wire: envelope::Config) -> Self {
        self.config.wire = wire;
        self
    }

    /// Sets the card and presence publication (RFC-0004). Omit to publish neither.
    #[must_use]
    pub fn publication(mut self, publication: presence::Publication) -> Self {
        self.config.publication = Some(publication);
        self
    }

    /// Sets the approval mode (RFC-0006 0006-004).
    #[must_use]
    pub fn approval(mut self, mode: ApprovalMode) -> Self {
        self.config.approval = mode;
        self
    }

    /// Sets the number of turns that may run at once across all contexts (0005-054).
    #[must_use]
    pub fn max_active(mut self, limit: usize) -> Self {
        self.config.max_active = limit;
        self
    }

    /// Sets the waiting-task quota per (origin, context_id) at which admission answers
    /// QUEUE_FULL (0005-028).
    #[must_use]
    pub fn max_waiting_per_context(mut self, limit: usize) -> Self {
        self.config.max_waiting_per_context = limit;
        self
    }

    /// Sets the inline payload limit in bytes.
    #[must_use]
    pub fn max_inline(mut self, limit: usize) -> Self {
        self.config.max_inline = limit;
        self
    }

    /// Binds the workspace lease hook (RFC-0012 sections 4.8 and 4.9). Without a binding,
    /// turns run without workspace leases.
    #[must_use]
    pub fn workspace(mut self, binding: WorkspaceBinding) -> Self {
        self.config.workspace = Some(binding);
        self
    }

    /// Sets the directory and workspace mode approvals report when no workspace lease applies
    /// (RFC-0006 0006-006). Required: the worker never reads the process working directory.
    #[must_use]
    pub fn working_directory(mut self, path: impl Into<PathBuf>, mode: WorkspaceMode) -> Self {
        self.config.working_directory = path.into();
        self.config.working_mode = mode;
        self
    }

    /// Enables operator-decision signing ([0010-079]): an approval decision is applied only when
    /// it verifies under an operator-role key in `trust`. Unsigned decisions are then refused
    /// with SIGNATURE_MISSING. Off by default, so decisions take the bus sender as before.
    #[must_use]
    pub fn signing(mut self, trust: Arc<a2a_security::trust::TrustStore>) -> Self {
        self.config.signing = Some(trust);
        self
    }

    /// Routes mapped audit records to `sink` (RFC-0013). Every note is also buffered for
    /// `take_audit`, whether or not a sink is attached.
    #[must_use]
    pub fn audit_sink(mut self, sink: Arc<Mutex<dyn AuditRecorder>>) -> Self {
        self.audit_sink = Some(sink);
        self
    }

    /// Returns the configuration accumulated so far.
    #[must_use]
    pub fn config(&self) -> &WorkerConfig {
        &self.config
    }

    /// Opens the task store, attaches the backend and starts serving the RPC subjects.
    ///
    /// # Errors
    /// Returns [`WorkerError`] when the store cannot be recovered, the backend refuses the
    /// sink, or the bus refuses the serve subscription.
    pub async fn build<B, D, S>(
        self,
        bus: B,
        backend: D,
        store: S,
        clock: Box<dyn envelope::Clock>,
    ) -> Result<Worker<B, D, S>, WorkerError>
    where
        B: Bus,
        D: Backend,
        S: TaskStore,
    {
        let Self { config, audit_sink } = self;
        let mut worker = Worker::open(config, bus, backend, store, clock).await?;
        if let Some(sink) = audit_sink {
            worker.attach_audit(sink);
        }
        Ok(worker)
    }
}
