//! Command-line grammar (RFC-0012 [0012-054]).

use std::path::PathBuf;

use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};

/// `a2a`: operator commands for the A2A-Bus host.
#[derive(Debug, Parser)]
#[command(name = "a2a", version, about = "Operator CLI for the A2A-Bus host")]
pub struct Cli {
    /// Config file. Overrides `A2A_CONFIG` and the platform default.
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,
    /// NATS URL. Overrides `A2A_BUS_URL`.
    #[arg(long, global = true, value_name = "URL")]
    pub bus_url: Option<String>,
    /// Tenant. Overrides `A2A_TENANT`.
    #[arg(long, global = true, value_name = "TENANT")]
    pub tenant: Option<String>,
    /// Operator agent id that sends requests and signs decisions. Overrides `A2A_OPERATOR`.
    #[arg(long, global = true, value_name = "ID")]
    pub operator: Option<String>,
    /// State directory holding `creds/` and the default trust file. Overrides `A2AB_STATE_DIR`.
    #[arg(long, global = true, value_name = "DIR")]
    pub state_dir: Option<PathBuf>,
    /// Trust file. Defaults to `[security] trust_file` from the CLI config, else
    /// `<state-dir>/trust.toml`. The a2ad config is not read: an operator whose a2ad config
    /// sets `[security] trust_file` must pass `--trust` with the same path.
    #[arg(long, global = true, value_name = "PATH")]
    pub trust: Option<PathBuf>,
    /// Bound on each request, in seconds.
    #[arg(long, global = true, value_name = "SECS")]
    pub timeout: Option<u64>,
    /// Print proto-JSON of the A2A messages instead of text ([0012-056]).
    #[arg(long, global = true)]
    pub json: bool,
    /// Disable ANSI color ([0012-059]).
    #[arg(long, global = true)]
    pub no_color: bool,
    /// The command to run.
    #[command(subcommand)]
    pub command: Command,
}

/// Top-level commands ([0012-054]).
#[derive(Debug, Subcommand)]
pub enum Command {
    /// List agents, optionally only those whose card matches one filter.
    Discover(DiscoverArgs),
    /// Send a message to an agent.
    Send(SendArgs),
    /// Follow a task's events.
    Watch(WatchArgs),
    /// Cancel a task.
    Cancel(CancelArgs),
    /// List open approval requests and decide them.
    Approvals {
        /// The approvals action.
        #[command(subcommand)]
        action: ApprovalsAction,
    },
    /// Start an agent. Requires the supervisor control channel (not yet defined by an RFC).
    Start {
        /// Agent id.
        agent: String,
    },
    /// Stop an agent, or the whole host when no agent is named.
    Stop {
        /// Agent id.
        agent: Option<String>,
    },
    /// Audit log tools.
    Audit {
        /// The audit action.
        #[command(subcommand)]
        action: AuditAction,
    },
}

/// Filter for `discover`; at most one may be given. Without one, every card is listed.
#[derive(Debug, Args)]
#[command(group(ArgGroup::new("filter").required(false).multiple(false)))]
pub struct DiscoverArgs {
    /// Exact agent id.
    #[arg(long, value_name = "ID", group = "filter")]
    pub agent: Option<String>,
    /// Exact skill id.
    #[arg(long, value_name = "SKILL", group = "filter")]
    pub skill: Option<String>,
    /// Tag of any skill.
    #[arg(long, value_name = "TAG", group = "filter")]
    pub tag: Option<String>,
    /// Extension URI in the card.
    #[arg(long, value_name = "URI", group = "filter")]
    pub extension: Option<String>,
    /// Input mode of the card or a skill.
    #[arg(long, value_name = "MODE", group = "filter")]
    pub input_mode: Option<String>,
    /// Output mode of the card or a skill.
    #[arg(long, value_name = "MODE", group = "filter")]
    pub output_mode: Option<String>,
    /// Include agents that are not READY.
    #[arg(long)]
    pub include_offline: bool,
}

/// Arguments of `send`.
#[derive(Debug, Args)]
pub struct SendArgs {
    /// Target agent id.
    pub agent: String,
    /// Message text, or `-` to read it from standard input.
    pub text: String,
    /// Context id to continue.
    #[arg(long, value_name = "ID")]
    pub context: Option<String>,
    /// Block until the task is terminal or interrupted, or the bound elapses.
    #[arg(long, conflicts_with = "follow")]
    pub wait: bool,
    /// Print task events as they arrive.
    #[arg(long)]
    pub follow: bool,
}

/// Arguments of `watch`.
#[derive(Debug, Args)]
pub struct WatchArgs {
    /// Task id.
    pub task_id: String,
    /// Agent that owns the task. When absent, the configured agents are searched.
    #[arg(long, value_name = "ID")]
    pub agent: Option<String>,
    /// Replay events after this sequence number.
    #[arg(long, value_name = "SEQ")]
    pub after_seq: Option<u64>,
}

/// Arguments of `cancel`.
#[derive(Debug, Args)]
pub struct CancelArgs {
    /// Task id.
    pub task_id: String,
    /// Agent that owns the task. When absent, the configured agents are searched.
    #[arg(long, value_name = "ID")]
    pub agent: Option<String>,
}

/// Approval actions.
#[derive(Debug, Subcommand)]
pub enum ApprovalsAction {
    /// List open approval requests.
    List {
        /// Show only requests that have not expired.
        #[arg(long)]
        pending: bool,
        /// Limit the search to one agent.
        #[arg(long, value_name = "ID")]
        agent: Option<String>,
    },
    /// Approve a request.
    Approve(DecideArgs),
    /// Deny a request.
    Deny(DecideArgs),
}

/// Arguments shared by `approve` and `deny`.
#[derive(Debug, Args)]
pub struct DecideArgs {
    /// Approval request id.
    pub request_id: String,
    /// Scope of an approval: `once` or `task`. Ignored by `deny`.
    #[arg(long, value_enum, default_value_t = Scope::Once)]
    pub scope: Scope,
    /// Confirm without a prompt. Required when standard input is not a terminal.
    #[arg(long)]
    pub yes: bool,
    /// Limit the search to one agent.
    #[arg(long, value_name = "ID")]
    pub agent: Option<String>,
    /// Free-text note. Refused: `ApprovalDecision` carries no note field (RFC-0006).
    #[arg(long, value_name = "TEXT")]
    pub note: Option<String>,
}

/// Approval scope ([0012-054]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Scope {
    /// This request only.
    Once,
    /// The whole task.
    Task,
}

/// Audit actions.
#[derive(Debug, Subcommand)]
pub enum AuditAction {
    /// Verify audit files for chain, sequence and checkpoint breaks ([0013-028]).
    Verify {
        /// Audit files, verified in name order.
        #[arg(required = true, value_name = "FILE")]
        files: Vec<PathBuf>,
        /// Trust file with checkpoint keys. Overrides the configured path.
        #[arg(long, value_name = "PATH")]
        trust: Option<PathBuf>,
    },
}
