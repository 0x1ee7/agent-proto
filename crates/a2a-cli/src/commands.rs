//! Bus-facing commands. Everything here is generic over `bus_api::Bus`; only the binary
//! names a concrete bus.

use std::sync::Arc;

use a2a_client::{
    CallOptions, Client, Directory, Event, Outbound, Outcome, Query, SendReply, SigningHook,
    StreamStart, stream::EventStream, text,
};
use a2a_types::ids::{AgentId, ContextId, TaskId};
use a2a_types::proto::a2a as pb;
use bus_api::bus::Bus;

use crate::cli::{CancelArgs, DiscoverArgs, SendArgs, WatchArgs};
use crate::config::{Settings, parse_agent};
use crate::error::{CliError, Exit, classify};
use crate::output::{Io, line, write_json};

/// Everything a bus-facing command needs: the client and resolved settings.
pub struct Session<B: Bus> {
    pub client: Client<B>,
    pub settings: Settings,
    /// Operator signing hook for decisions; decisions are never sent without one (0012-058).
    pub operator: Option<Arc<dyn SigningHook>>,
}

impl<B: Bus> std::fmt::Debug for Session<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("settings", &self.settings)
            .field("signed", &self.operator.is_some())
            .finish_non_exhaustive()
    }
}

impl<B: Bus> Session<B> {
    /// Call options derived from the resolved settings.
    #[must_use]
    pub fn opts(&self) -> CallOptions {
        CallOptions {
            timeout: self.settings.timeout,
            ..CallOptions::default()
        }
    }
}

/// Parses a task id from the command line.
///
/// # Errors
/// Returns a usage error when the id is not a valid task id.
pub fn parse_task(raw: &str) -> Result<TaskId, CliError> {
    TaskId::parse(raw).map_err(|error| CliError::Usage(format!("invalid task id `{raw}`: {error}")))
}

/// Parses a context id from the command line.
///
/// # Errors
/// Returns a usage error when the id is not a valid context id.
pub fn parse_context(raw: &str) -> Result<ContextId, CliError> {
    ContextId::parse(raw)
        .map_err(|error| CliError::Usage(format!("invalid context id `{raw}`: {error}")))
}

/// Human-readable name of a task's state.
#[must_use]
pub fn state_name(task: &pb::Task) -> String {
    task.status
        .as_ref()
        .and_then(|status| pb::TaskState::try_from(status.state).ok())
        .map_or_else(
            || "UNKNOWN".to_owned(),
            |state| state.as_str_name().to_owned(),
        )
}

/// Prints a task: proto-JSON with `--json`, one summary line otherwise.
///
/// # Errors
/// Returns a failure when writing output fails.
pub fn print_task(io: &mut Io<'_>, task: &pb::Task) -> Result<(), CliError> {
    if io.json {
        return write_json(io.out, task);
    }
    line(io.out, &format!("task {} {}", task.id, state_name(task)))
}

/// Prints a message: proto-JSON with `--json`, text parts otherwise.
///
/// # Errors
/// Returns a failure when writing output fails.
pub fn print_message(io: &mut Io<'_>, message: &pb::Message) -> Result<(), CliError> {
    if io.json {
        return write_json(io.out, message);
    }
    let body: Vec<&str> = message
        .parts
        .iter()
        .filter_map(|part| match &part.content {
            Some(pb::part::Content::Text(text)) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    line(
        io.out,
        &format!("message {}: {}", message.message_id, body.join(" ")),
    )
}

/// Prints one stream event: proto-JSON with `--json`, a summary line otherwise.
///
/// # Errors
/// Returns a failure when writing output fails.
pub fn print_event(io: &mut Io<'_>, event: &Event) -> Result<(), CliError> {
    if io.json {
        return write_json(io.out, &event.response);
    }
    let summary = match &event.response.payload {
        Some(pb::stream_response::Payload::Task(task)) => state_name(task),
        Some(pb::stream_response::Payload::Message(message)) => {
            format!("message {}", message.message_id)
        }
        Some(pb::stream_response::Payload::StatusUpdate(update)) => update
            .status
            .as_ref()
            .and_then(|status| pb::TaskState::try_from(status.state).ok())
            .map_or_else(
                || "UNKNOWN".to_owned(),
                |state| state.as_str_name().to_owned(),
            ),
        Some(pb::stream_response::Payload::ArtifactUpdate(_)) => "artifact".to_owned(),
        None => "empty".to_owned(),
    };
    line(io.out, &format!("seq {} {summary}", event.seq))
}

/// Drains a stream until its end marker, printing each event.
///
/// # Errors
/// Returns the mapped client error of a failed event read, or a failure when writing output fails.
pub async fn drain_events<B: Bus>(
    events: &mut EventStream<'_, B>,
    io: &mut Io<'_>,
) -> Result<(), CliError> {
    while let Some(event) = events.next().await? {
        print_event(io, &event)?;
        if event.stream_end {
            break;
        }
    }
    Ok(())
}

/// Builds the directory query from exactly one filter flag.
///
/// # Errors
/// Returns a usage error when no filter is set or the agent id is invalid.
pub fn discover_query(args: &DiscoverArgs) -> Result<Query, CliError> {
    if let Some(agent) = &args.agent {
        return Ok(Query::Agent(parse_agent(agent)?));
    }
    if let Some(skill) = &args.skill {
        return Ok(Query::Skill(skill.clone()));
    }
    if let Some(tag) = &args.tag {
        return Ok(Query::Tag(tag.clone()));
    }
    if let Some(extension) = &args.extension {
        return Ok(Query::Extension(extension.clone()));
    }
    if let Some(mode) = &args.input_mode {
        return Ok(Query::InputMode(mode.clone()));
    }
    if let Some(mode) = &args.output_mode {
        return Ok(Query::OutputMode(mode.clone()));
    }
    Err(CliError::Usage(
        "discover needs exactly one filter: --agent, --skill, --tag, --extension, --input-mode or --output-mode"
            .to_owned(),
    ))
}

/// `a2a discover`: lists agents matching one filter.
///
/// # Errors
/// Returns usage errors for bad filters and the mapped client error for bus failures.
pub async fn discover<B: Bus>(
    session: &Session<B>,
    args: &DiscoverArgs,
    io: &mut Io<'_>,
) -> Result<Exit, CliError> {
    let query = discover_query(args)?;
    let mut directory = Directory::new(&session.client);
    directory.sync().await?;
    let listings = directory.query(&query, args.include_offline)?;
    if io.json {
        let cards: Vec<&serde_json::Value> =
            listings.iter().map(|listing| listing.card.card()).collect();
        write_json(io.out, &cards)?;
        return Ok(Exit::Success);
    }
    for listing in &listings {
        let presence = match listing.presence {
            Some(_) if listing.is_ready() => "ready",
            Some(_) => "not-ready",
            None => "offline",
        };
        let verified = if listing.card.verified() {
            "verified"
        } else {
            "unverified"
        };
        line(
            io.out,
            &format!(
                "{}  v{}  {presence}  {verified}",
                listing.card.agent(),
                listing.card.version()
            ),
        )?;
    }
    Ok(Exit::Success)
}

/// Reads the message body; `-` reads standard input to its end.
fn read_body(raw: &str, io: &mut Io<'_>) -> Result<String, CliError> {
    let body = if raw == "-" {
        let mut buffer = String::new();
        io.input
            .read_to_string(&mut buffer)
            .map_err(|error| CliError::Failed(format!("reading standard input: {error}")))?;
        buffer
    } else {
        raw.to_owned()
    };
    if body.trim().is_empty() {
        return Err(CliError::Usage("message text is empty".to_owned()));
    }
    Ok(body)
}

/// Maps a terminal task state to the process outcome of `send --wait`.
fn terminal_outcome(task: &pb::Task) -> Result<(), CliError> {
    match pb::TaskState::try_from(task.status.as_ref().map_or(0, |status| status.state)) {
        Ok(pb::TaskState::Completed) => Ok(()),
        Ok(pb::TaskState::Rejected) => {
            Err(CliError::Rejected(format!("task {} was rejected", task.id)))
        }
        _ => Err(CliError::Failed(format!(
            "task {} ended in state {}",
            task.id,
            state_name(task)
        ))),
    }
}

/// `a2a send`: sends one message; `--wait` blocks for a terminal state, `--follow` streams.
///
/// # Errors
/// Returns usage errors, the mapped client error, or a timeout/failure from `--wait`.
pub async fn send<B: Bus>(
    session: &Session<B>,
    args: &SendArgs,
    io: &mut Io<'_>,
) -> Result<Exit, CliError> {
    let agent = parse_agent(&args.agent)?;
    let body = read_body(&args.text, io)?;
    let mut outbound = Outbound::new(vec![text(&body)]);
    if let Some(context) = &args.context {
        outbound.context_id = Some(parse_context(context)?);
    }
    let opts = session.opts();
    if args.wait {
        match session
            .client
            .send_and_wait(&agent, &outbound, &opts)
            .await?
        {
            Outcome::Message(message) => print_message(io, &message)?,
            Outcome::Interrupted(task) => print_task(io, &task)?,
            Outcome::Terminal(task) => {
                print_task(io, &task)?;
                terminal_outcome(&task)?;
            }
            Outcome::Pending(task) => {
                print_task(io, &task)?;
                return Err(CliError::Timeout(format!(
                    "task {} still running after the wait bound",
                    task.id
                )));
            }
        }
        return Ok(Exit::Success);
    }
    if args.follow {
        match session.client.stream(&agent, &outbound, &opts).await? {
            StreamStart::Message(message) => print_message(io, &message)?,
            StreamStart::Done(task) => print_task(io, &task)?,
            StreamStart::Follow { task, mut events } => {
                print_task(io, &task)?;
                drain_events(&mut events, io).await?;
            }
        }
        return Ok(Exit::Success);
    }
    match session.client.send(&agent, &outbound, &opts).await?.value {
        SendReply::Message(message) => print_message(io, &message)?,
        SendReply::Task(task) => print_task(io, &task)?,
    }
    Ok(Exit::Success)
}

/// Finds the agent that holds a task: the explicit `--agent`, or the first configured agent
/// that answers `GetTask` for it.
///
/// # Errors
/// Returns usage errors when there is nothing to search, NotFound when no agent holds the
/// task, or the last non-NotFound client error.
pub async fn locate_agent<B: Bus>(
    session: &Session<B>,
    task_id: &TaskId,
    explicit: Option<&str>,
) -> Result<AgentId, CliError> {
    if let Some(agent) = explicit {
        return parse_agent(agent);
    }
    if session.settings.agents.is_empty() {
        return Err(CliError::Usage(
            "no --agent given and no [[agent]] configured to search".to_owned(),
        ));
    }
    let opts = session.opts();
    let mut last_error = None;
    for agent in &session.settings.agents {
        match session.client.get_task(agent, task_id, &opts).await {
            Ok(_) => return Ok(agent.clone()),
            Err(error) if classify(&error) == Exit::NotFound => {}
            Err(error) => last_error = Some(error),
        }
    }
    match last_error {
        Some(error) => Err(error.into()),
        None => Err(CliError::NotFound(format!(
            "task {task_id} not found on any configured agent"
        ))),
    }
}

/// `a2a watch`: prints the task snapshot, then streams events until the stream ends.
///
/// # Errors
/// Returns usage, lookup, or client errors.
pub async fn watch<B: Bus>(
    session: &Session<B>,
    args: &WatchArgs,
    io: &mut Io<'_>,
) -> Result<Exit, CliError> {
    let task_id = parse_task(&args.task_id)?;
    let agent = locate_agent(session, &task_id, args.agent.as_deref()).await?;
    let (task, mut events) = session
        .client
        .subscribe(&agent, &task_id, args.after_seq, &session.opts())
        .await?;
    print_task(io, &task)?;
    drain_events(&mut events, io).await?;
    Ok(Exit::Success)
}

/// `a2a cancel`: requests cancellation and prints the resulting task.
///
/// # Errors
/// Returns usage, lookup, or client errors.
pub async fn cancel<B: Bus>(
    session: &Session<B>,
    args: &CancelArgs,
    io: &mut Io<'_>,
) -> Result<Exit, CliError> {
    let task_id = parse_task(&args.task_id)?;
    let agent = locate_agent(session, &task_id, args.agent.as_deref()).await?;
    let reply = session
        .client
        .cancel_task(&agent, &task_id, &session.opts())
        .await?;
    print_task(io, &reply.value)?;
    Ok(Exit::Success)
}
