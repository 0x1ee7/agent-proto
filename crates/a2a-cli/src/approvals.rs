//! `a2a approvals`: lists open approval requests and records operator decisions.
//! Decisions are signed by the operator hook; the CLI never sends an unsigned decision.

use a2a_client::{SendReply, approval::PendingApproval};
use a2a_types::ids::AgentId;
use a2a_types::proto::a2a as pb;
use a2a_types::proto::a2abus::{ApprovalOptionKind, ApprovalRequest};
use bus_api::bus::Bus;

use crate::card;
use crate::cli::{ApprovalsAction, DecideArgs, Scope};
use crate::commands::{Session, state_name};
use crate::config::parse_agent;
use crate::error::{CliError, Exit};
use crate::output::{Io, line, write_json};

/// An approval request together with the agent that raised it.
#[derive(Debug)]
pub struct Open {
    pub agent: AgentId,
    pub pending: PendingApproval,
}

/// Whether a request's expiry is at or before `now_ms`. A missing or unreadable expiry counts as
/// expired, so an approval that cannot be bounded is never offered as pending.
#[must_use]
pub fn is_expired(request: &ApprovalRequest, now_ms: u64) -> bool {
    let Some(expires) = &request.expires_at else {
        return false;
    };
    let millis = expires
        .seconds
        .saturating_mul(1000)
        .saturating_add(i64::from(expires.nanos) / 1_000_000);
    u64::try_from(millis).map_or(true, |millis| millis <= now_ms)
}

/// Chooses the option id a decision names.
///
/// # Errors
/// Returns a rejection when the request does not offer the option the decision needs.
pub fn select_option(
    request: &ApprovalRequest,
    approve: bool,
    scope: Scope,
) -> Result<String, CliError> {
    let wanted: &[ApprovalOptionKind] = match (approve, scope) {
        (true, Scope::Once) => &[ApprovalOptionKind::AllowOnce],
        (true, Scope::Task) => &[ApprovalOptionKind::AllowAlways],
        (false, _) => &[
            ApprovalOptionKind::RejectOnce,
            ApprovalOptionKind::RejectAlways,
        ],
    };
    for kind in wanted {
        if let Some(option) = request
            .options
            .iter()
            .find(|option| option.kind == *kind as i32)
        {
            return Ok(option.option_id.clone());
        }
    }
    Err(CliError::Rejected(format!(
        "request {} does not offer the required option",
        request.request_id
    )))
}

/// Requires explicit confirmation before a decision. `--yes` skips the prompt. Without a
/// terminal and without `--yes`, nothing is decided.
///
/// # Errors
/// Returns usage when no terminal can confirm, or rejected when the operator declines.
pub fn confirm(io: &mut Io<'_>, approve: bool, yes: bool) -> Result<(), CliError> {
    if yes {
        return Ok(());
    }
    if !io.interactive {
        return Err(CliError::Usage(
            "confirmation needed but stdin is not a terminal; pass --yes to decide".to_owned(),
        ));
    }
    let verb = if approve { "approve" } else { "deny" };
    write!(io.err, "{verb} this request? [y/N]: ")
        .and_then(|()| io.err.flush())
        .map_err(|error| CliError::Failed(format!("writing prompt: {error}")))?;
    let mut answer = String::new();
    io.input
        .read_line(&mut answer)
        .map_err(|error| CliError::Failed(format!("reading confirmation: {error}")))?;
    let answer = answer.trim();
    if answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes") {
        Ok(())
    } else {
        Err(CliError::Rejected(
            "not confirmed; no decision sent".to_owned(),
        ))
    }
}

/// Agents to search: the explicit `--agent`, or every configured agent.
fn candidate_agents<B: Bus>(
    session: &Session<B>,
    explicit: Option<&str>,
) -> Result<Vec<AgentId>, CliError> {
    if let Some(agent) = explicit {
        return Ok(vec![parse_agent(agent)?]);
    }
    if session.settings.agents.is_empty() {
        return Err(CliError::Usage(
            "no --agent given and no [[agent]] configured to search".to_owned(),
        ));
    }
    Ok(session.settings.agents.clone())
}

/// Collects open approvals from the candidate agents, following every page.
///
/// # Errors
/// Returns the mapped client error of a failed list, or of a malformed approval.
pub async fn open_approvals<B: Bus>(
    session: &Session<B>,
    explicit: Option<&str>,
) -> Result<Vec<Open>, CliError> {
    let opts = session.opts();
    let mut found = Vec::new();
    for agent in candidate_agents(session, explicit)? {
        let mut page_token = String::new();
        loop {
            let request = pb::ListTasksRequest {
                status: pb::TaskState::InputRequired as i32,
                page_token: page_token.clone(),
                ..Default::default()
            };
            let reply = session.client.list_tasks(&agent, request, &opts).await?;
            for task in &reply.value.tasks {
                if let Some(pending) = PendingApproval::from_task(task)? {
                    found.push(Open {
                        agent: agent.clone(),
                        pending,
                    });
                }
            }
            if reply.value.next_page_token.is_empty() {
                break;
            }
            page_token.clone_from(&reply.value.next_page_token);
        }
    }
    Ok(found)
}

/// Prints one approval card to the diagnostic stream, before any decision.
fn write_card(io: &mut Io<'_>, request: &ApprovalRequest) -> Result<(), CliError> {
    let rendered = card::render(&card::build(request), io.color);
    writeln!(io.err, "{rendered}")
        .map_err(|error| CliError::Failed(format!("writing approval card: {error}")))
}

/// `a2a approvals list`.
///
/// # Errors
/// Returns usage, client, or output errors.
pub async fn list<B: Bus>(
    session: &Session<B>,
    pending_only: bool,
    agent: Option<&str>,
    io: &mut Io<'_>,
    now_ms: u64,
) -> Result<Exit, CliError> {
    let mut open = open_approvals(session, agent).await?;
    if pending_only {
        open.retain(|item| !is_expired(item.pending.request(), now_ms));
    }
    if io.json {
        let requests: Vec<&ApprovalRequest> =
            open.iter().map(|item| item.pending.request()).collect();
        write_json(io.out, &requests)?;
        return Ok(Exit::Success);
    }
    for item in &open {
        line(io.out, &format!("agent {}", item.agent))?;
        let rendered = card::render(&card::build(item.pending.request()), io.color);
        line(io.out, &rendered)?;
    }
    Ok(Exit::Success)
}

/// `a2a approvals approve` or `deny`: prints the card, confirms, then sends a signed decision.
///
/// # Errors
/// Returns usage for an unsupported `--note` or missing terminal confirmation, rejected for an
/// absent option, an expired request, or a declined confirmation, failure when the session has no
/// signing key, not-found for an unknown request, or the mapped client error.
pub async fn decide<B: Bus>(
    session: &Session<B>,
    args: &DecideArgs,
    approve: bool,
    io: &mut Io<'_>,
    now_ms: u64,
) -> Result<Exit, CliError> {
    if !approve && args.note.is_some() {
        return Err(CliError::Usage(
            "--note is not supported: ApprovalDecision carries no note field (RFC-0006)".to_owned(),
        ));
    }
    let Some(operator) = session.operator.as_deref() else {
        return Err(CliError::Failed(
            "no operator signing key configured; decisions are never sent unsigned (0012-058)"
                .to_owned(),
        ));
    };
    let open = open_approvals(session, args.agent.as_deref()).await?;
    let found = open
        .into_iter()
        .find(|item| item.pending.request().request_id == args.request_id)
        .ok_or_else(|| {
            CliError::NotFound(format!("no open approval request {}", args.request_id))
        })?;
    let request = found.pending.request();
    if is_expired(request, now_ms) {
        return Err(CliError::Rejected(format!(
            "approval request {} has expired",
            args.request_id
        )));
    }
    let option = select_option(request, approve, args.scope)?;
    write_card(io, request)?;
    confirm(io, approve, args.yes)?;
    let reply = session
        .client
        .decide_signed(
            &found.agent,
            &found.pending,
            &option,
            operator,
            &session.opts(),
        )
        .await?;
    match reply.value {
        SendReply::Task(task) => {
            if io.json {
                write_json(io.out, &task)?;
            } else {
                line(
                    io.out,
                    &format!(
                        "decided {} with {option}: task {} {}",
                        args.request_id,
                        task.id,
                        state_name(&task)
                    ),
                )?;
            }
        }
        SendReply::Message(message) => {
            if io.json {
                write_json(io.out, &message)?;
            } else {
                line(
                    io.out,
                    &format!("decided {} with {option}", args.request_id),
                )?;
            }
        }
    }
    Ok(Exit::Success)
}

/// Dispatches `a2a approvals <action>`.
///
/// # Errors
/// Returns the errors of the selected action.
pub async fn run<B: Bus>(
    session: &Session<B>,
    action: &ApprovalsAction,
    io: &mut Io<'_>,
    now_ms: u64,
) -> Result<Exit, CliError> {
    match action {
        ApprovalsAction::List { pending, agent } => {
            list(session, *pending, agent.as_deref(), io, now_ms).await
        }
        ApprovalsAction::Approve(args) => decide(session, args, true, io, now_ms).await,
        ApprovalsAction::Deny(args) => decide(session, args, false, io, now_ms).await,
    }
}
