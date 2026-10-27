//! Bus-facing work for the console: directory sync, task follow, audit feed, and decisions.
//!
//! The driver is generic over `bus_api::bus::Bus` (RFC-0001 0001-001). Only the binary names a
//! concrete binding. Background work reports to the console through [`Msg`]; the console sends
//! [`Command`]s back.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::Duration;

use a2a_client::approval::PendingApproval;
use a2a_client::{
    CallOptions, Client, ClientError, Config, Directory, Event, Listing, PresenceState, SigningHook,
};
use a2a_core::envelope::SystemClock;
use a2a_types::ids::{AgentId, TaskId, TenantId};
use a2a_types::proto::a2a as pb;
use a2a_types::proto::a2abus::AuditEvent;
use bus_api::bus::Bus;
use serde_json::Value;
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::JoinHandle;

use crate::app::Command;
use crate::audit_feed::{AuditFeed, FeedError};
use crate::model::agents::{self, Observed, approval_mode_of};
use crate::model::approvals::DecisionOutcome;
use crate::operator::console_agent;

/// Interval between directory syncs when no refresh is requested.
pub const SYNC_INTERVAL: Duration = Duration::from_secs(1);
/// Pause before retrying a failing audit consumer.
pub const RETRY_BACKOFF: Duration = Duration::from_secs(1);
/// Upper bound on task-list pages read per agent in one sync.
const LIST_PAGE_LIMIT: usize = 100;

/// Messages from background work to the console.
#[derive(Debug)]
pub enum Msg {
    /// A directory snapshot; the link is healthy.
    Synced(Vec<Observed>),
    /// The directory sync failed; the link is not healthy.
    LinkFailed(String),
    /// Task snapshots reported by one agent.
    Tasks { agent: String, tasks: Vec<pb::Task> },
    /// One stream event of a followed task.
    Event { agent: String, event: Event },
    /// One audit record.
    Audit(AuditEvent),
    /// The outcome of a decision sent for `request_id`.
    Decision {
        request_id: String,
        decision_id: String,
        outcome: DecisionOutcome,
    },
    /// A line for the status bar.
    Notice(String),
}

/// Runs the bus-facing tasks until the console sends [`Command::Quit`] or the UI goes away.
///
/// `control` carries directory, task, and decision traffic as the console identity (`tui`);
/// `audit` is the audit tail. `signing` is set when the operator key loaded (RFC-0012 0012-043).
///
/// # Errors
/// Returns the client construction error text when the client cannot be built.
pub async fn run<B, F>(
    control: B,
    audit: F,
    tenant: TenantId,
    signing: Option<Arc<dyn SigningHook>>,
    ui: Sender<Msg>,
    commands: UnboundedReceiver<Command>,
) -> Result<(), String>
where
    B: Bus + 'static,
    F: AuditFeed + 'static,
{
    let operator = console_agent();
    let decide_hook = signing.clone();
    let config = Config {
        signing,
        ..Config::default()
    };
    let client = Client::new(control, Arc::new(SystemClock), config, tenant, operator)
        .map_err(|e| format!("client: {e}"))?;
    let client = Arc::new(client);
    let refresh = Arc::new(Notify::new());
    tokio::select! {
        () = sync_loop(client.clone(), ui.clone(), refresh.clone()) => {}
        () = audit_loop(audit, ui.clone()) => {}
        () = command_loop(client, ui, refresh, decide_hook, commands) => {}
    }
    Ok(())
}

async fn sync_loop<B: Bus + 'static>(
    client: Arc<Client<B>>,
    ui: Sender<Msg>,
    refresh: Arc<Notify>,
) {
    let mut directory = Directory::new(&client);
    let mut followers: HashMap<String, JoinHandle<()>> = HashMap::new();
    loop {
        followers.retain(|_, handle| !handle.is_finished());
        match directory.sync().await {
            Ok(()) => {
                if !publish(&client, &directory, &ui, &mut followers).await {
                    return;
                }
            }
            Err(e) => {
                if ui.send(Msg::LinkFailed(e.to_string())).is_err() {
                    return;
                }
            }
        }
        tokio::select! {
            () = tokio::time::sleep(SYNC_INTERVAL) => {}
            () = refresh.notified() => {}
        }
    }
}

/// Publishes one directory snapshot, reads task lists, and starts followers for active tasks.
/// Returns false when the console has gone away.
async fn publish<B: Bus + 'static>(
    client: &Arc<Client<B>>,
    directory: &Directory<'_, B>,
    ui: &Sender<Msg>,
    followers: &mut HashMap<String, JoinHandle<()>>,
) -> bool {
    let listings = match directory.all(true) {
        Ok(listings) => listings,
        Err(e) => return ui.send(Msg::LinkFailed(e.to_string())).is_ok(),
    };
    let observed: Vec<Observed> = listings.iter().map(observe).collect();
    let ready: Vec<AgentId> = listings
        .iter()
        .filter(|listing| listing.is_ready())
        .map(|listing| listing.card.agent().clone())
        .collect();
    if ui.send(Msg::Synced(observed)).is_err() {
        return false;
    }
    for agent in ready {
        match list_all_tasks(client, &agent).await {
            Ok(tasks) => {
                for task in tasks.iter().filter(|task| is_active(task)) {
                    let key = format!("{}/{}", agent.as_str(), task.id);
                    if followers.contains_key(&key) {
                        continue;
                    }
                    if let Ok(task_id) = TaskId::parse(&task.id) {
                        let handle = follow(Arc::clone(client), agent.clone(), task_id, ui.clone());
                        followers.insert(key, handle);
                    }
                }
                let msg = Msg::Tasks {
                    agent: agent.as_str().to_owned(),
                    tasks,
                };
                if ui.send(msg).is_err() {
                    return false;
                }
            }
            Err(e) => {
                let note = Msg::Notice(format!("{}: list tasks: {e}", agent.as_str()));
                if ui.send(note).is_err() {
                    return false;
                }
            }
        }
    }
    true
}

fn observe(listing: &Listing<'_>) -> Observed {
    let card = listing.card;
    Observed {
        agent: card.agent().as_str().to_owned(),
        card_name: card
            .card()
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_owned),
        card_version: Some(card.version().to_owned()),
        presence: listing.presence.map(|p| match p.state {
            PresenceState::Starting => agents::Presence::Starting,
            PresenceState::Ready => agents::Presence::Ready,
            PresenceState::Draining => agents::Presence::Draining,
        }),
        active_tasks: listing.presence.map_or(0, |p| p.active_tasks),
        approval_mode: approval_mode_of(card.card()),
    }
}

/// A task still needs following unless its state is terminal.
fn is_active(task: &pb::Task) -> bool {
    let state = task
        .status
        .as_ref()
        .map_or(pb::TaskState::Unspecified, |s| {
            pb::TaskState::try_from(s.state).unwrap_or(pb::TaskState::Unspecified)
        });
    !matches!(
        state,
        pb::TaskState::Completed | pb::TaskState::Failed | pb::TaskState::Canceled
    )
}

async fn list_all_tasks<B: Bus>(
    client: &Client<B>,
    agent: &AgentId,
) -> Result<Vec<pb::Task>, ClientError> {
    let opts = CallOptions::default();
    let mut tasks = Vec::new();
    let mut token = String::new();
    for _ in 0..LIST_PAGE_LIMIT {
        let request = pb::ListTasksRequest {
            page_token: token.clone(),
            ..Default::default()
        };
        let page = client.list_tasks(agent, request, &opts).await?.value;
        tasks.extend(page.tasks);
        if page.next_page_token.is_empty() {
            break;
        }
        token = page.next_page_token;
    }
    Ok(tasks)
}

/// Follows one task's event stream until it ends, reporting the snapshot first.
fn follow<B: Bus + 'static>(
    client: Arc<Client<B>>,
    agent: AgentId,
    task: TaskId,
    ui: Sender<Msg>,
) -> JoinHandle<()> {
    let agent_name = agent.as_str().to_owned();
    tokio::spawn(async move {
        let opts = CallOptions::default();
        let (snapshot, mut stream) = match client.subscribe(&agent, &task, None, &opts).await {
            Ok(pair) => pair,
            Err(e) => {
                let note = Msg::Notice(format!("{agent_name}/{}: subscribe: {e}", task.as_str()));
                // The console may already be gone; nothing else to report.
                drop(ui.send(note));
                return;
            }
        };
        let initial = Msg::Tasks {
            agent: agent_name.clone(),
            tasks: vec![snapshot],
        };
        if ui.send(initial).is_err() {
            return;
        }
        loop {
            match stream.next().await {
                Ok(Some(event)) => {
                    let msg = Msg::Event {
                        agent: agent_name.clone(),
                        event,
                    };
                    if ui.send(msg).is_err() {
                        return;
                    }
                }
                Ok(None) => return,
                Err(e) => {
                    let note = Msg::Notice(format!("{agent_name}/{}: stream: {e}", task.as_str()));
                    drop(ui.send(note));
                    return;
                }
            }
        }
    })
}

async fn audit_loop<F: AuditFeed>(mut feed: F, ui: Sender<Msg>) {
    loop {
        match feed.next().await {
            Ok(event) => {
                if ui.send(Msg::Audit(event)).is_err() {
                    return;
                }
            }
            Err(FeedError::Malformed(e)) => {
                let note = Msg::Notice(format!("audit: skipped malformed record ({e})"));
                if ui.send(note).is_err() {
                    return;
                }
            }
            Err(FeedError::Bus(e)) => {
                let note = Msg::Notice(format!("audit feed: {e}"));
                if ui.send(note).is_err() {
                    return;
                }
                tokio::time::sleep(RETRY_BACKOFF).await;
            }
        }
    }
}

async fn command_loop<B: Bus + 'static>(
    client: Arc<Client<B>>,
    ui: Sender<Msg>,
    refresh: Arc<Notify>,
    operator: Option<Arc<dyn SigningHook>>,
    mut commands: UnboundedReceiver<Command>,
) {
    while let Some(command) = commands.recv().await {
        let keep_going = match command {
            Command::Decide {
                agent,
                pending,
                decision_id,
                option_id,
            } => {
                let outcome =
                    decide(&client, operator.as_deref(), &agent, &pending, &option_id).await;
                let request_id = pending.request().request_id.clone();
                ui.send(Msg::Decision {
                    request_id,
                    decision_id,
                    outcome,
                })
                .is_ok()
            }
            Command::Cancel { agent, task } => {
                let text = cancel(&client, &agent, &task).await;
                refresh.notify_one();
                ui.send(Msg::Notice(text)).is_ok()
            }
            Command::Refresh => {
                refresh.notify_one();
                true
            }
            Command::Quit => return,
        };
        if !keep_going {
            return;
        }
    }
}

/// Sends one signed operator decision (RFC-0006, RFC-0007 0007-095). Without the operator key
/// it is refused locally. Local refusals are `Refused`; anything that reached the bus without a
/// clear answer is `Unknown` and is never resent.
async fn decide<B: Bus>(
    client: &Client<B>,
    operator: Option<&dyn SigningHook>,
    agent: &str,
    pending: &PendingApproval,
    option_id: &str,
) -> DecisionOutcome {
    let Some(operator) = operator else {
        return DecisionOutcome::Refused("read-only: operator key not loaded".to_owned());
    };
    let agent = match AgentId::parse(agent) {
        Ok(agent) => agent,
        Err(e) => return DecisionOutcome::Refused(format!("agent id: {e}")),
    };
    match client
        .decide_signed(
            &agent,
            pending,
            option_id,
            operator,
            &CallOptions::default(),
        )
        .await
    {
        Ok(_) => DecisionOutcome::Accepted,
        Err(e @ ClientError::Approval(_)) => DecisionOutcome::Refused(e.to_string()),
        Err(e) => DecisionOutcome::Unknown(e.to_string()),
    }
}

async fn cancel<B: Bus>(client: &Client<B>, agent: &str, task: &str) -> String {
    let agent = match AgentId::parse(agent) {
        Ok(agent) => agent,
        Err(e) => return format!("cancel: agent id: {e}"),
    };
    let task_id = match TaskId::parse(task) {
        Ok(task_id) => task_id,
        Err(e) => return format!("cancel: task id: {e}"),
    };
    match client
        .cancel_task(&agent, &task_id, &CallOptions::default())
        .await
    {
        Ok(_) => format!("cancel requested for {task}"),
        Err(e) => format!("cancel failed for {task}: {e}"),
    }
}
