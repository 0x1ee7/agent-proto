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

/// An agent that could not be listed, and the error that stopped it.
#[derive(Debug)]
pub struct AgentFailure {
    pub agent: AgentId,
    pub error: CliError,
}

/// The approvals of every candidate agent that answered, and one failure per agent that did not.
#[derive(Debug, Default)]
pub struct Listing {
    pub open: Vec<Open>,
    pub failures: Vec<AgentFailure>,
    /// Candidates that answered, whether or not they held an approval.
    pub answered: usize,
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

/// Collects open approvals from the candidate agents, following every page of each. An agent that
/// fails on any page contributes nothing and is reported as a failure, so a partial list is never
/// shown as complete.
///
/// # Errors
/// Returns usage when no agent is named or configured. Agent failures are not errors here.
pub async fn open_approvals<B: Bus>(
    session: &Session<B>,
    explicit: Option<&str>,
) -> Result<Listing, CliError> {
    let mut listing = Listing::default();
    for agent in candidate_agents(session, explicit)? {
        match agent_approvals(session, &agent).await {
            Ok(pending) => {
                listing.answered += 1;
                listing.open.extend(pending.into_iter().map(|pending| Open {
                    agent: agent.clone(),
                    pending,
                }));
            }
            Err(error) => listing.failures.push(AgentFailure { agent, error }),
        }
    }
    Ok(listing)
}

/// Every pending approval one agent holds, following all of its pages.
///
/// # Errors
/// Returns the mapped client error of a failed page, or of a malformed approval.
async fn agent_approvals<B: Bus>(
    session: &Session<B>,
    agent: &AgentId,
) -> Result<Vec<PendingApproval>, CliError> {
    let opts = session.opts();
    let mut found = Vec::new();
    let mut page_token = String::new();
    loop {
        let request = list_request(&page_token);
        let reply = session.client.list_tasks(agent, request, &opts).await?;
        for task in &reply.value.tasks {
            if let Some(pending) = PendingApproval::from_task(task)? {
                found.push(pending);
            }
        }
        if reply.value.next_page_token.is_empty() {
            break;
        }
        page_token.clone_from(&reply.value.next_page_token);
    }
    Ok(found)
}

/// Writes one diagnostic line per agent that could not be listed.
fn report_failures(io: &mut Io<'_>, failures: &[AgentFailure]) -> Result<(), CliError> {
    for failure in failures {
        writeln!(io.err, "agent {}: {}", failure.agent, failure.error)
            .map_err(|error| CliError::Failed(format!("writing agent error: {error}")))?;
    }
    Ok(())
}

/// Exit class of a listing in which no agent answered: the class of its first failure.
fn exit_of_failures(failures: &[AgentFailure]) -> Exit {
    failures
        .first()
        .map_or(Exit::Failure, |failure| failure.error.exit())
}

/// The ListTasks request for one page. The worker rejects a status or context filter with
/// `LIST_FILTER_UNSUPPORTED`, so the request is unfiltered; [`PendingApproval::from_task`] keeps
/// only `INPUT_REQUIRED` tasks that carry an approval request.
fn list_request(page_token: &str) -> pb::ListTasksRequest {
    pb::ListTasksRequest {
        page_token: page_token.to_owned(),
        ..Default::default()
    }
}

/// Prints one approval card to the diagnostic stream, before any decision.
fn write_card(io: &mut Io<'_>, request: &ApprovalRequest) -> Result<(), CliError> {
    let rendered = card::render(&card::build(request), io.color);
    writeln!(io.err, "{rendered}")
        .map_err(|error| CliError::Failed(format!("writing approval card: {error}")))
}

/// `a2a approvals list`. Lists the approvals of every agent that answers; an agent that cannot be
/// listed is reported on stderr. Success when at least one agent answered, otherwise the exit class
/// of the first failure (0012-055).
///
/// # Errors
/// Returns usage, or output errors.
pub async fn list<B: Bus>(
    session: &Session<B>,
    pending_only: bool,
    agent: Option<&str>,
    io: &mut Io<'_>,
    now_ms: u64,
) -> Result<Exit, CliError> {
    let Listing {
        mut open,
        failures,
        answered,
    } = open_approvals(session, agent).await?;
    report_failures(io, &failures)?;
    if answered == 0 {
        return Ok(exit_of_failures(&failures));
    }
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
/// signing key, not-found for an unknown request once every candidate agent answered, or the
/// mapped client error.
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
    let Listing { open, failures, .. } = open_approvals(session, args.agent.as_deref()).await?;
    report_failures(io, &failures)?;
    let Some(found) = open
        .into_iter()
        .find(|item| item.pending.request().request_id == args.request_id)
    else {
        // An agent that could not be listed may hold the request, so its absence is not proven.
        if failures.is_empty() {
            return Err(CliError::NotFound(format!(
                "no open approval request {}",
                args.request_id
            )));
        }
        return Ok(exit_of_failures(&failures));
    };
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Req [0005-004], worker `list_tasks`: a status or context filter is rejected, so the
    /// approval listing must send an unfiltered request and keep the page token.
    #[test]
    fn req_0005_004_approval_listing_sends_unfiltered_pages() {
        let request = list_request("page-2");
        assert_eq!(request.status, 0);
        assert!(request.context_id.is_empty());
        assert_eq!(request.page_token, "page-2");
    }
}
