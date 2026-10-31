//! Inbound RPC handling. Each accepted request is mapped onto the task store, then
//! answered exactly once. Replies are sent first; events that a subscription replays
//! are published after the reply so the snapshot precedes them.

use std::collections::BTreeSet;

use a2a_core::budget::{self, Allowance};
use a2a_core::envelope::{
    Inbound, ReplyError, ReplyOptions, RequestBody, RequestHead, WireMessage,
};
use a2a_core::errors::{StatusExtras, WireError};
use a2a_core::subject::Op;
use a2a_core::task::{
    Admission, Effect, EventBody, Incoming, ListQuery, Submission, TaskError, TaskStore, TaskView,
};
use a2a_security::error::{BusError, Reason};
use a2a_types::extension::{Activation, ExtensionError, read_activated};
use a2a_types::failure::FailureCode;
use a2a_types::ids::{AgentId, ContextId, TaskId};
use a2a_types::proto::a2a as pb;
use a2a_types::proto::a2abus;
use a2a_types::vocab::ext::{APPROVAL, BUDGET};
use bus_api::bus::Bus;
use pbjson_types::{Struct, Timestamp};

use crate::backend::{Backend, InputPart};
use crate::error::WorkerError;
use crate::signing::{self, Received};
use crate::wire;
use crate::worker::{PendingTurn, Worker, artifact_key};
use a2a_core::envelope::Outgoing;

/// The typed body of a successful reply.
pub(crate) enum Response {
    Send(pb::SendMessageResponse),
    Task(pb::Task),
    Tasks(pb::ListTasksResponse),
    Stream(pb::StreamResponse),
}

/// A successful outcome: the reply, reply metadata, and events to publish after it.
pub(crate) struct Handled {
    response: Response,
    last_seq: Option<u64>,
    applied: Vec<String>,
    replay: Vec<Outgoing>,
    /// A blocking send whose reply waits for the task (0002-050).
    hold: Option<Wait>,
}

/// The task a blocking send waits on, and the instant the reply is due regardless.
pub(crate) struct Wait {
    pub(crate) origin: String,
    pub(crate) task_id: String,
    pub(crate) until: u64,
}

/// A held blocking send awaiting its reply.
pub(crate) struct Held {
    pub(crate) inbound: Inbound,
    pub(crate) wait: Wait,
}

/// A refused request. `Wire` and `Security` answer the caller; `Fatal` means the worker cannot continue.
pub(crate) enum RpcError {
    Wire {
        error: WireError,
        detail: Option<&'static str>,
    },
    /// A refused decision carrying an RFC-0007 security reason, answered with that reason.
    Security(BusError),
    Fatal(WorkerError),
}

impl RpcError {
    fn wire(error: WireError) -> Self {
        Self::Wire {
            error,
            detail: None,
        }
    }

    fn invalid(detail: &'static str) -> Self {
        Self::Wire {
            error: WireError::InvalidParams,
            detail: Some(detail),
        }
    }

    fn extension_invalid(detail: &'static str) -> Self {
        Self::Wire {
            error: WireError::ExtensionDataInvalid,
            detail: Some(detail),
        }
    }
}

impl From<TaskError> for RpcError {
    fn from(err: TaskError) -> Self {
        match err {
            TaskError::NotFound => Self::wire(WireError::TaskNotFound),
            TaskError::ContextMismatch => Self::invalid("CONTEXT_MISMATCH"),
            TaskError::NotActive => Self::invalid("TASK_NOT_ACTIVE"),
            TaskError::Approval(refusal) => Self::invalid(refusal.detail()),
            TaskError::Invalid(detail) => Self::invalid(detail),
            TaskError::BadPageToken => Self::invalid("BAD_PAGE_TOKEN"),
            TaskError::Budget(_) => Self::extension_invalid("BUDGET_INVALID"),
            TaskError::Terminal | TaskError::NotCancelable => {
                Self::wire(WireError::TaskNotCancelable)
            }
            TaskError::NotSubscribable => Self::wire(WireError::UnsupportedOperation),
            TaskError::Draining => Self::wire(WireError::Draining),
            other => Self::Fatal(WorkerError::from(other)),
        }
    }
}

pub(crate) fn plain(response: Response) -> Handled {
    Handled {
        response,
        last_seq: None,
        applied: Vec::new(),
        replay: Vec::new(),
        hold: None,
    }
}

fn send_response(task: pb::Task) -> pb::SendMessageResponse {
    pb::SendMessageResponse {
        payload: Some(pb::send_message_response::Payload::Task(task)),
    }
}

/// The reply to a send-like request carrying `view`: `SendMessage` answers with a
/// `SendMessageResponse`, `SendStreamingMessage` with the first `StreamResponse` element and
/// the task sequence the requester resumes from (RFC-0002 0002-052).
fn task_reply(op: Op, view: &TaskView) -> (Response, Option<u64>) {
    let task = wire::task_proto(view);
    if op == Op::Stream {
        let first = pb::StreamResponse {
            payload: Some(pb::stream_response::Payload::Task(task)),
        };
        (Response::Stream(first), Some(view.seq))
    } else {
        (Response::Send(send_response(task)), None)
    }
}

/// Sends the one reply of `inbound`. A body whose type is not the operation's reply type is
/// answered with a failure instead, so the requester never waits for a reply that cannot come.
fn send_reply<M: WireMessage>(
    inbound: Inbound,
    body: &M,
    options: &ReplyOptions,
) -> Result<(), ReplyError> {
    if M::TYPE_NAME != inbound.head.op.reply_type() {
        // The refusal is best effort: the type mismatch is the error reported.
        let _ = inbound.fail(WireError::Overloaded, &StatusExtras::default());
        return Err(ReplyError::PayloadType);
    }
    inbound.reply(body, options)
}

/// Reads extension data under `uri`, gated by activation. Malformed data is refused.
fn marked<'a>(
    metadata: Option<&'a Struct>,
    uri: &str,
    activation: &Activation,
) -> Result<Option<&'a Struct>, RpcError> {
    match metadata {
        None => Ok(None),
        Some(meta) => read_activated(meta, uri, activation).map_err(extension_error),
    }
}

fn extension_error(err: ExtensionError) -> RpcError {
    RpcError::Wire {
        error: WireError::ExtensionDataInvalid,
        detail: err.reason(),
    }
}

fn budget_of(data: &Struct) -> Option<a2abus::Budget> {
    crate::wire::from_struct(data)
}

fn decision_of(value: &pbjson_types::Value) -> Option<a2abus::ApprovalDecision> {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| serde_json::from_value(value).ok())
}

/// Milliseconds since the epoch for a non-negative timestamp; `None` otherwise.
fn timestamp_ms(ts: &Timestamp) -> Option<u64> {
    let secs = u64::try_from(ts.seconds).ok()?;
    let nanos = u64::try_from(ts.nanos).ok()?;
    secs.checked_mul(1000)?.checked_add(nanos / 1_000_000)
}

impl<B: Bus, D: Backend, S: TaskStore> Worker<B, D, S> {
    /// Answers one accepted request: a blocking send with a task still in flight is held,
    /// everything else is answered at once.
    pub(crate) async fn handle(
        &mut self,
        inbound: Inbound,
        now: u64,
        signed: Option<Received>,
    ) -> Result<(), WorkerError> {
        match self.route(&inbound.head, &inbound.body, now, signed.as_ref()) {
            Ok(Handled {
                hold: Some(wait), ..
            }) => {
                self.held.push(Held { inbound, wait });
                Ok(())
            }
            result => self.answer(inbound, result).await,
        }
    }

    /// Writes the reply for a routed request: the success body, or a wire error on refusal,
    /// then any replay events.
    pub(crate) async fn answer(
        &mut self,
        inbound: Inbound,
        result: Result<Handled, RpcError>,
    ) -> Result<(), WorkerError> {
        let op = inbound.head.op;
        // A reply that cannot be written ends that one request, never the worker: the error is
        // recorded with its text and the serving loop continues.
        match result {
            Ok(handled) => {
                let options = ReplyOptions {
                    last_seq: handled.last_seq,
                    applied_extensions: handled.applied,
                };
                let sent = match &handled.response {
                    Response::Send(body) => send_reply(inbound, body, &options),
                    Response::Task(body) => send_reply(inbound, body, &options),
                    Response::Tasks(body) => send_reply(inbound, body, &options),
                    Response::Stream(body) => send_reply(inbound, body, &options),
                };
                if let Err(err) = sent {
                    self.audit(None, format!("REPLY_FAILED: op {}: {err}", op.token()));
                    return Ok(());
                }
                for outgoing in handled.replay {
                    if let Err(err) = self.publish(outgoing).await {
                        self.audit(None, format!("REPLAY_PUBLISH_FAILED: {err}"));
                    }
                }
                Ok(())
            }
            Err(RpcError::Wire { error, detail }) => {
                let mut extras = StatusExtras::default();
                if let Some(detail) = detail {
                    extras
                        .metadata
                        .insert("detail".to_owned(), detail.to_owned());
                }
                if let Err(err) = inbound.fail(error, &extras) {
                    self.audit(None, format!("REPLY_FAILED: op {}: {err}", op.token()));
                }
                Ok(())
            }
            Err(RpcError::Security(err)) => {
                if let Err(err) = inbound.fail_status(&err.to_status()) {
                    self.audit(None, format!("REPLY_FAILED: op {}: {err}", op.token()));
                }
                Ok(())
            }
            Err(RpcError::Fatal(err)) => {
                // No internal-error wire code exists (RFC-0002); OVERLOADED tells the caller to retry.
                if let Err(refusal) = inbound.fail(WireError::Overloaded, &StatusExtras::default())
                {
                    self.audit(None, format!("REPLY_FAILED: op {}: {refusal}", op.token()));
                }
                Err(err)
            }
        }
    }

    /// Answers held blocking sends whose task is terminal or input-required, or whose wait
    /// is due (0002-050, 0005-046). A task that can no longer be read is answered at once.
    pub(crate) async fn release_held(&mut self, now: u64) -> Result<(), WorkerError> {
        for entry in std::mem::take(&mut self.held) {
            let view = self.tasks.task(&entry.wait.origin, &entry.wait.task_id);
            let due = match &view {
                Ok(v) => {
                    v.state.is_terminal() || v.state.is_interrupted() || now >= entry.wait.until
                }
                Err(_) => true,
            };
            if !due {
                self.held.push(entry);
                continue;
            }
            let op = entry.inbound.head.op;
            let result = view.map_err(RpcError::from).map(|v| {
                let (response, last_seq) = task_reply(op, &v);
                Handled {
                    response,
                    last_seq,
                    applied: Vec::new(),
                    replay: Vec::new(),
                    hold: None,
                }
            });
            self.answer(entry.inbound, result).await?;
        }
        Ok(())
    }

    fn route(
        &mut self,
        head: &RequestHead,
        body: &RequestBody,
        now: u64,
        signed: Option<&Received>,
    ) -> Result<Handled, RpcError> {
        match body {
            RequestBody::SendMessage(req) => self.send_message(head, req, now, signed),
            RequestBody::GetTask(req) => self.get_task(head, req),
            RequestBody::ListTasks(req) => self.list_tasks(head, req),
            RequestBody::CancelTask(req) => self.cancel_task(head, req, now),
            RequestBody::SubscribeToTask(req) => self.subscribe(head, req, now),
            RequestBody::PushCreate(_)
            | RequestBody::PushGet(_)
            | RequestBody::PushList(_)
            | RequestBody::PushDelete(_) => {
                Err(RpcError::wire(WireError::PushNotificationNotSupported))
            }
            RequestBody::GetExtendedAgentCard(_) => {
                Err(RpcError::wire(WireError::ExtendedAgentCardNotConfigured))
            }
        }
    }

    fn send_message(
        &mut self,
        head: &RequestHead,
        req: &pb::SendMessageRequest,
        now: u64,
        signed: Option<&Received>,
    ) -> Result<Handled, RpcError> {
        let message = req
            .message
            .as_ref()
            .ok_or(RpcError::invalid("MESSAGE_REQUIRED"))?;
        for part in &message.parts {
            if marked(part.metadata.as_ref(), APPROVAL, &head.activation)?.is_some() {
                return self.decide_request(head, message, part, now, signed);
            }
        }
        if !message.task_id.is_empty() {
            return Err(RpcError::invalid("FOLLOW_UP_UNSUPPORTED"));
        }
        if message.parts.is_empty() {
            return Err(RpcError::invalid("PARTS_REQUIRED"));
        }
        let mut input = Vec::with_capacity(message.parts.len());
        let mut texts: Vec<&str> = Vec::with_capacity(message.parts.len());
        for part in &message.parts {
            match &part.content {
                Some(pb::part::Content::Text(text)) => {
                    texts.push(text.as_str());
                    input.push(InputPart::Text(text.clone()));
                }
                _ => return Err(RpcError::wire(WireError::ContentTypeNotSupported)),
            }
        }
        let context_id = if message.context_id.is_empty() {
            ContextId::generate().as_str().to_owned()
        } else {
            ContextId::parse(&message.context_id)
                .map_err(|_| RpcError::invalid("CONTEXT_ID_INVALID"))?
                .as_str()
                .to_owned()
        };
        let mut applied = Vec::new();
        let (budget, budget_proto) =
            match marked(message.metadata.as_ref(), BUDGET, &head.activation)? {
                None => (None, None),
                Some(data) => {
                    let proto =
                        budget_of(data).ok_or(RpcError::extension_invalid("BUDGET_INVALID"))?;
                    let allowance = Allowance::from_proto(&proto);
                    allowance
                        .validate()
                        .map_err(|_| RpcError::extension_invalid("BUDGET_INVALID"))?;
                    applied.push(BUDGET.to_owned());
                    (Some(allowance), Some(proto))
                }
            };
        let agent = self.config.agent.as_str();
        let fingerprint = budget::fingerprint(agent, texts.iter().copied());
        let chain = budget_proto.as_ref().map_or_else(
            || budget::root_chain(agent, &fingerprint),
            |proto| proto.chain.clone(),
        );
        // 0006-059 / 0015-057: re-entrancy is refused by core at admission, without queueing. A
        // full queue must not take precedence over it, so only the ancestors are consulted here.
        let reentrant = budget::chain_before_self(&chain, agent)
            .is_some_and(|ancestors| budget::is_reentrant(ancestors, agent));
        // 0005-028: a full waiting queue for this (origin, context_id) refuses the submission.
        let waiting = self.waiting_in_context(&head.origin, &context_id);
        let pressure = if !reentrant && waiting >= self.config.max_waiting_per_context {
            Some(FailureCode::QueueFull)
        } else {
            None
        };
        // A message carrying a budget and chain is a delegation (0006-043): core validates the
        // chain and applies the loop rules (0006-045, 0006-059) when it is admitted.
        let incoming = budget.map(|grant| Incoming {
            task_id: TaskId::generate().to_string(),
            parent_task_id: None,
            grant,
            chain: chain.clone(),
        });
        let submission = Submission {
            msg_id: head.msg_id.as_str().to_owned(),
            origin: head.origin.clone(),
            message_id: (!message.message_id.is_empty()).then(|| message.message_id.clone()),
            context_id: Some(context_id.clone()),
            sent_ms: now,
            ttl_ms: head.deadline_ms.saturating_sub(now),
            fingerprint,
            budget,
            incoming,
            pressure,
        };
        let (admission, effects) = self.tasks.submit(submission, now)?;
        let task_id = match admission {
            Admission::Created { task_id } => {
                self.peers.chains.insert(task_id.clone(), chain);
                let staged = effects
                    .iter()
                    .any(|effect| matches!(effect, Effect::StartBackend { task_id: id } if *id == task_id));
                if staged {
                    let deadline_ms = self.tasks.task(&head.origin, &task_id)?.deadline_ms;
                    self.pending.insert(
                        task_id.clone(),
                        PendingTurn {
                            context_id,
                            input,
                            deadline_ms,
                            origin: head.origin.clone(),
                        },
                    );
                }
                self.run(effects, now).map_err(RpcError::Fatal)?;
                task_id
            }
            Admission::Existing { task_id } => task_id,
            Admission::Stale => return Err(RpcError::wire(WireError::RequestExpired)),
        };
        let view = self.tasks.task(&head.origin, &task_id)?;
        // 0002-050, 0005-046: a blocking send replies once the task is terminal or
        // input-required, or at the earlier of the caller's and the task's deadline.
        let immediate = req
            .configuration
            .as_ref()
            .is_some_and(|config| config.return_immediately);
        let settled = view.state.is_terminal() || view.state.is_interrupted();
        // A streaming send replies with the first stream element at once (0002-052).
        let hold = (!immediate && !settled && head.op != Op::Stream).then(|| Wait {
            origin: head.origin.clone(),
            task_id: task_id.clone(),
            until: head.deadline_ms.min(view.deadline_ms),
        });
        let (response, last_seq) = task_reply(head.op, &view);
        Ok(Handled {
            response,
            last_seq,
            applied,
            replay: Vec::new(),
            hold,
        })
    }

    /// Approval decision: a SendMessage that carries the approval extension (RFC-0006).
    fn decide_request(
        &mut self,
        head: &RequestHead,
        message: &pb::Message,
        part: &pb::Part,
        now: u64,
        signed: Option<&Received>,
    ) -> Result<Handled, RpcError> {
        let Some(pb::part::Content::Data(value)) = &part.content else {
            return Err(RpcError::extension_invalid("APPROVAL_DECISION_INVALID"));
        };
        let decision =
            decision_of(value).ok_or(RpcError::extension_invalid("APPROVAL_DECISION_INVALID"))?;
        // The signature is checked before any task state is read, so a refusal changes nothing.
        let operator = self.decision_operator(head, signed, now)?;
        if decision.task_id != message.task_id {
            return Err(RpcError::invalid("APPROVAL_NOT_APPLICABLE"));
        }
        // Authority is the operator identity (0006-014), not the task's origin: the owning origin
        // is resolved from the task id so an operator's own origin cannot hide the task.
        let owner = self
            .tasks
            .task_owner(&decision.task_id)
            .ok_or(TaskError::NotFound)?;
        let view = self.tasks.task(&owner.origin, &decision.task_id)?;
        if !message.context_id.is_empty() && message.context_id != view.context_id {
            return Err(RpcError::invalid("CONTEXT_MISMATCH"));
        }
        let (_settled, effects) = self.tasks.decide(
            &owner.origin,
            &decision,
            Some(operator.as_str()),
            head.tenant.as_str(),
            now,
        )?;
        self.run(effects, now).map_err(RpcError::Fatal)?;
        let view = self.tasks.task(&owner.origin, &decision.task_id)?;
        let (response, last_seq) = task_reply(head.op, &view);
        Ok(Handled {
            response,
            last_seq,
            applied: vec![APPROVAL.to_owned()],
            replay: Vec::new(),
            hold: None,
        })
    }

    /// The operator identity a decision is attributed to ([0006-014]). With signing off the bus
    /// sender stands. With signing on, the signature must verify under an operator-role key and
    /// the relay must be `tui` or `cli` ([0010-079]); anything else is refused.
    fn decision_operator(
        &self,
        head: &RequestHead,
        signed: Option<&Received>,
        now: u64,
    ) -> Result<String, RpcError> {
        let Some(trust) = self.config.signing.as_deref() else {
            return Ok(head.sender.as_str().to_owned());
        };
        let received =
            signed.ok_or_else(|| RpcError::Security(BusError::new(Reason::SignatureMissing)))?;
        signing::verify_operator_decision(
            trust,
            &self.replay,
            now,
            &head.tenant,
            &head.sender,
            received,
        )
        .map_err(RpcError::Security)
    }

    /// True for the operator service identities `cli` and `tui` (RFC-0005 [0005-004]). The bus
    /// authenticates the sender, and neither id can name an agent (RFC-0012 [0012-002]), so an
    /// agent or a gateway peer cannot present it.
    fn is_operator(head: &RequestHead) -> bool {
        head.sender == AgentId::cli() || head.sender == AgentId::tui()
    }

    /// The origin under which a task read is made: the owner's for an operator, so the operator
    /// sees the task wherever it came from (0005-004); the caller's own for everyone else.
    fn read_origin(&self, head: &RequestHead, task_id: &str) -> String {
        if Self::is_operator(head)
            && let Some(owner) = self.tasks.task_owner(task_id)
        {
            return owner.origin;
        }
        head.origin.clone()
    }

    fn get_task(&self, head: &RequestHead, req: &pb::GetTaskRequest) -> Result<Handled, RpcError> {
        let origin = self.read_origin(head, &req.id);
        let view = self.tasks.task(&origin, &req.id)?;
        Ok(plain(Response::Task(
            self.task_with_approval(&origin, &view)?,
        )))
    }

    /// The task snapshot with its open approval request, when the task awaits one (0006-007).
    fn task_with_approval(&self, origin: &str, view: &TaskView) -> Result<pb::Task, RpcError> {
        let mut task = wire::task_proto(view);
        if let Some(body) = self.pending_approval(origin, view)? {
            wire::attach_approval(&mut task, body);
        }
        Ok(task)
    }

    /// The open approval request of an INPUT_REQUIRED task, read from its status events. The
    /// latest status event decides: a decision or expiry leaves INPUT_REQUIRED, and a peer's
    /// input request (0005-008) carries no approval id.
    fn pending_approval(&self, origin: &str, view: &TaskView) -> Result<Option<Struct>, RpcError> {
        if view.state != pb::TaskState::InputRequired {
            return Ok(None);
        }
        let events = self.tasks.subscribe(origin, &view.task_id, 0)?;
        let latest_status = events.iter().rev().find_map(|event| match &event.body {
            EventBody::Status { approval, .. } => Some(approval.as_deref()),
            _ => None,
        });
        let Some(Some(request_id)) = latest_status else {
            return Ok(None);
        };
        let Some(request) = self.tasks.approval_request(request_id) else {
            return Ok(None);
        };
        wire::approval_metadata(&request)
            .map(Some)
            .map_err(|_| RpcError::invalid("APPROVAL_NOT_ENCODABLE"))
    }

    fn list_tasks(
        &self,
        head: &RequestHead,
        req: &pb::ListTasksRequest,
    ) -> Result<Handled, RpcError> {
        if !req.context_id.is_empty() || req.status != 0 {
            return Err(RpcError::invalid("LIST_FILTER_UNSUPPORTED"));
        }
        let page_size = req
            .page_size
            .map(|n| usize::try_from(n).map_err(|_| RpcError::invalid("PAGE_SIZE_INVALID")))
            .transpose()?;
        let status_after_ms = match &req.status_timestamp_after {
            None => None,
            Some(ts) => {
                Some(timestamp_ms(ts).ok_or(RpcError::invalid("STATUS_TIMESTAMP_INVALID"))?)
            }
        };
        let query = ListQuery {
            page_size,
            page_token: (!req.page_token.is_empty()).then(|| req.page_token.clone()),
            status_after_ms,
        };
        let operator = Self::is_operator(head);
        let page = if operator {
            self.tasks.list_all(&query)?
        } else {
            self.tasks.list(&head.origin, &query)?
        };
        let tasks = page
            .tasks
            .iter()
            .map(|view| {
                let origin = if operator {
                    self.read_origin(head, &view.task_id)
                } else {
                    head.origin.clone()
                };
                self.task_with_approval(&origin, view)
            })
            .collect::<Result<Vec<pb::Task>, RpcError>>()?;
        Ok(plain(Response::Tasks(pb::ListTasksResponse {
            page_size: i32::try_from(tasks.len()).unwrap_or(i32::MAX),
            tasks,
            next_page_token: page.next_page_token.unwrap_or_default(),
            total_size: i32::try_from(page.total_size).unwrap_or(i32::MAX),
        })))
    }

    fn cancel_task(
        &mut self,
        head: &RequestHead,
        req: &pb::CancelTaskRequest,
        now: u64,
    ) -> Result<Handled, RpcError> {
        // 0005-004: an operator identity cancels a task of any origin; others only their own.
        let origin = self.read_origin(head, &req.id);
        let effects = if self.deferred_ends.contains_key(&req.id) {
            // 0015-067: the turn already ended and only children remain; nothing is left to stop.
            let effects = self.tasks.cancel_deferred(&origin, &req.id, now)?;
            self.deferred_ends.remove(&req.id);
            effects
        } else {
            self.tasks.cancel(&origin, &req.id, now)?
        };
        self.run(effects, now).map_err(RpcError::Fatal)?;
        let view = self.tasks.task(&origin, &req.id)?;
        Ok(plain(Response::Task(wire::task_proto(&view))))
    }

    /// Snapshot plus replay from `A2AB-After-Seq` (RFC-0002, L18). The snapshot is the reply;
    /// the events follow it on the ev subject.
    fn subscribe(
        &mut self,
        head: &RequestHead,
        req: &pb::SubscribeToTaskRequest,
        now: u64,
    ) -> Result<Handled, RpcError> {
        let _ = now;
        let origin = self.read_origin(head, &req.id);
        let events = self
            .tasks
            .subscribe(&origin, &req.id, head.after_seq.unwrap_or(0))?;
        let view = self.tasks.task(&origin, &req.id)?;
        let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
        let mut replay = Vec::with_capacity(events.len());
        for event in &events {
            let first = match artifact_key(event) {
                Some(key) => !self.artifacts.contains(&key) && seen.insert(key),
                None => false,
            };
            let response = match self.render(event, &view.context_id, first) {
                Ok(response) => response,
                Err(reason) => {
                    self.audit(Some(&event.task_id), reason);
                    continue;
                }
            };
            match self.encode(&event.task_id, &event.msg_id, event.stream_ends(), response) {
                Ok(outgoing) => replay.push(outgoing),
                Err(err) => self.audit(Some(&event.task_id), format!("EVENT_NOT_ENCODABLE: {err}")),
            }
        }
        let snapshot = pb::StreamResponse {
            payload: Some(pb::stream_response::Payload::Task(wire::task_proto(&view))),
        };
        Ok(Handled {
            response: Response::Stream(snapshot),
            last_seq: Some(view.seq),
            applied: Vec::new(),
            replay,
            hold: None,
        })
    }
}
