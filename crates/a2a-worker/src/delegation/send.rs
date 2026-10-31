//! Peer tool calls (RFC-0015 section 4.6): `a2a_send`, `a2a_get`, `a2a_cancel`,
//! `a2a_discover` and `cancel-call`. Every refusal is answered on the request and
//! audited (0015-075). Refusals follow the order of 0015-054.

use a2a_client::budget as client_budget;
use a2a_client::{CallOptions, ClientError, Directory, Outbound, SendReply, outbound};
use a2a_core::budget::{self, BreakerStatus};
use a2a_core::task::{Effect, TaskError, TaskStore};
use a2a_mcp::ipc::{
    CancelResult, DiscoverParams, DiscoverResult, GetParams, IpcError, PeerEntry, SendParams,
    SkillRef, Wait, reason, validate_discover, validate_send,
};
use a2a_mcp::ipc_server::ServerRequest;
use a2a_types::failure::FailureCode;
use a2a_types::ids::{AgentId, TaskId};
use a2a_types::proto::a2a::{self as pb, TaskState};
use a2a_types::vocab::ext;
use bus_api::bus::Bus;
use serde_json::{Value, json};

use super::policy::{
    budget_exhausted, budget_refusal, call_deadline, check_allowlist, check_inflight,
    child_context, context_id, dimension_of, exhausted_dimension, parse_task_id,
};
use super::result::{frame, state_name};
use super::{
    CHILD_POLL_MS, Child, Delegation, Pending, RESEND_ATTEMPTS, RESEND_BACKOFF_MS, Waiter, current,
    respond_err, respond_outcome, respond_value,
};
use crate::backend::Backend;
use crate::error::WorkerError;
use crate::worker::Worker;

/// Peers listed by `a2a_discover` when the model gives no limit (0015-020).
const DEFAULT_DISCOVER_LIMIT: u32 = 20;
/// Longest peer name shown by `a2a_discover`, in characters.
const MAX_PEER_NAME_CHARS: usize = 128;
/// Longest peer description shown by `a2a_discover`, in characters.
const MAX_PEER_DESCRIPTION_CHARS: usize = 1024;

/// How the remote answered one send of a child message.
enum Attempt {
    /// The remote holds the child; its first view is attached.
    Accepted(Box<pb::Task>),
    /// The remote answered and holds no child: an error reply, or a reply that is not a task.
    Refused,
    /// No usable answer arrived (timeout, no responder, bus or reply failure): the remote may
    /// already hold the child under a task id this worker never saw (0015-117).
    Unanswered,
}

/// Whether a failed send leaves it unknown if the remote received the message. A reply that
/// carries a protocol error means the remote refused it; a failure before or without any
/// reply does not.
fn delivery_unknown(err: &ClientError) -> bool {
    matches!(
        err,
        ClientError::Local(_) | ClientError::Bus(_) | ClientError::InvalidReply(_)
    )
}

/// A send that passed every refusal and is ready to leave the worker.
struct Prepared {
    target: AgentId,
    context: String,
    hop: u32,
    deadline: u64,
    message: Outbound,
}

fn unavailable() -> IpcError {
    IpcError::new(reason::UNAVAILABLE)
}

fn malformed() -> IpcError {
    IpcError::new(reason::MALFORMED_MESSAGE)
}

impl<B: Bus, D: Backend, S: TaskStore> Worker<B, D, S> {
    /// `a2a_send`: creates a remote child, or replays the outcome of a repeated key.
    pub(crate) async fn send_call(
        &mut self,
        d: &mut Delegation<B>,
        request: ServerRequest,
        task: &str,
        params: &SendParams,
        now: u64,
        effects: &mut Vec<Effect>,
    ) -> Result<(), WorkerError> {
        if let Err(err) = validate_send(params) {
            self.refuse(Some(task), request, err);
            return Ok(());
        }
        let blocking = matches!(params.wait, Wait::Blocking);
        let replayed = d
            .ledger
            .get(&(task.to_owned(), params.idempotency_key.clone()))
            .cloned();
        if let Some(child_id) = replayed {
            replay(d, request, &child_id, blocking);
            return Ok(());
        }
        let (child_id, remote) = match self.start_child(d, task, params, now, effects).await? {
            Ok(created) => created,
            Err(err) => {
                self.refuse(Some(task), request, err);
                return Ok(());
            }
        };
        self.observe(d, &child_id, &remote, now, effects)?;
        replay(d, request, &child_id, blocking);
        Ok(())
    }

    /// Checks refusals (a) to (f), delegates the allowance and sends the child.
    /// Returns the child's local id and the remote task's first view.
    async fn start_child(
        &mut self,
        d: &mut Delegation<B>,
        task: &str,
        params: &SendParams,
        now: u64,
        effects: &mut Vec<Effect>,
    ) -> Result<Result<(String, pb::Task), IpcError>, WorkerError> {
        let prepared = match self.prepare(d, task, params, now, effects) {
            Ok(prepared) => prepared,
            Err(err) => return Ok(Err(err)),
        };
        // The wire message and its transport identifier are journaled with the child, so a
        // resend after a crash repeats the same request (0015-062).
        let Some(wire) = prepared
            .message
            .to_message()
            .ok()
            .and_then(|message| serde_json::to_value(message).ok())
        else {
            return Ok(Err(unavailable()));
        };
        let (child_id, allowance) = match self.tasks.delegate(task) {
            Ok(created) => created,
            Err(TaskError::Delegation(refused)) => {
                return Ok(Err(budget_exhausted(dimension_of(&refused))));
            }
            Err(_) => return Ok(Err(unavailable())),
        };
        if let Some(binding) = self.peers.bindings.get_mut(task) {
            binding.sends.admit(now);
        }
        // 0015-062: the child is recorded durably before its message is sent (0015-063 entry).
        let entry = serde_json::json!({
            "phase": "prepared",
            "parent": task,
            "key": params.idempotency_key,
            "child": child_id,
            "target": prepared.target.as_str(),
            "allowance": allowance,
            "context": prepared.context,
            "hop": prepared.hop,
            "deadline": prepared.deadline,
            "message": wire,
            "transport": prepared.message.transport_id().as_str(),
            "return_immediately": prepared.message.return_immediately,
        });
        if self
            .tasks
            .append_extension(super::JOURNAL_OWNER, entry)
            .is_err()
        {
            self.settle_unsent(task, &child_id, now, effects)?;
            return Ok(Err(unavailable()));
        }
        let pending = Pending {
            child: child_id.clone(),
            parent: task.to_owned(),
            key: params.idempotency_key.clone(),
            target: prepared.target,
            context: prepared.context,
            chain_length: prepared.hop,
            deadline: prepared.deadline,
            message: prepared.message,
            attempts: 0,
            retry_at: 0,
        };
        let Some(remote) = self.send_pending(d, &pending, now, effects).await? else {
            return Ok(Err(unavailable()));
        };
        self.audit(
            Some(task),
            format!(
                "DELEGATION_CHILD_CREATED child={child_id} target={} chain_length={}",
                pending.target.as_str(),
                pending.chain_length
            ),
        );
        Ok(Ok((child_id, remote)))
    }

    /// Sends a journaled child message and records the outcome (0015-062). On success the send
    /// is journaled as `sent`, the child is tracked and the ledger is updated; returns the remote
    /// task's first view. A failed send abandons the child and returns `None`.
    async fn send_pending(
        &mut self,
        d: &mut Delegation<B>,
        pending: &Pending,
        now: u64,
        effects: &mut Vec<Effect>,
    ) -> Result<Option<pb::Task>, WorkerError> {
        match self.attempt_send(d, pending).await? {
            Attempt::Accepted(remote) => self.record_sent(d, pending, *remote, now, effects).await,
            Attempt::Refused | Attempt::Unanswered => {
                self.abandon(pending, now, effects)?;
                Ok(None)
            }
        }
    }

    /// Makes one send of a journaled child message and classifies the answer.
    async fn attempt_send(
        &mut self,
        d: &mut Delegation<B>,
        pending: &Pending,
    ) -> Result<Attempt, WorkerError> {
        let sent = self
            .await_peer(
                d.client
                    .send(&pending.target, &pending.message, &CallOptions::default()),
            )
            .await?;
        Ok(match sent.map(|unary| unary.value) {
            Ok(SendReply::Task(remote)) => Attempt::Accepted(Box::new(remote)),
            Ok(SendReply::Message(_)) => Attempt::Refused,
            Err(err) if delivery_unknown(&err) => Attempt::Unanswered,
            Err(_) => Attempt::Refused,
        })
    }

    /// Journals an accepted send as `sent` and tracks the child. A remote id that does not
    /// parse abandons the child.
    async fn record_sent(
        &mut self,
        d: &mut Delegation<B>,
        pending: &Pending,
        remote: pb::Task,
        now: u64,
        effects: &mut Vec<Effect>,
    ) -> Result<Option<pb::Task>, WorkerError> {
        let Ok(remote_id) = TaskId::parse(&remote.id) else {
            self.abandon(pending, now, effects)?;
            return Ok(None);
        };
        #[cfg(test)]
        {
            // Crash point (0015-062): the remote accepted the child, but `sent` is never written.
            if std::mem::take(&mut d.park_after_accept) {
                std::future::pending::<()>().await;
            }
        }
        self.tasks.append_extension(
            super::JOURNAL_OWNER,
            json!({"phase": "sent", "child": pending.child, "remote": remote_id.as_str()}),
        )?;
        d.ledger.insert(
            (pending.parent.clone(), pending.key.clone()),
            pending.child.clone(),
        );
        d.children.insert(
            pending.child.clone(),
            Child {
                parent: pending.parent.clone(),
                target: pending.target.clone(),
                remote: remote_id,
                context_id: pending.context.clone(),
                chain_length: pending.chain_length,
                state: TaskState::Submitted,
                cancel_requested: false,
                settled: None,
                waiters: Vec::new(),
                next_poll: now.saturating_add(CHILD_POLL_MS),
                deadline: pending.deadline,
            },
        );
        Ok(Some(remote))
    }

    /// Closes a child that will not be sent: `abandoned` stops a restart from resending it, then
    /// its allowance is released (0015-062). Only a send the remote refused, or one repeated
    /// without an answer past its limit (0015-117), ends here.
    fn abandon(
        &mut self,
        pending: &Pending,
        now: u64,
        effects: &mut Vec<Effect>,
    ) -> Result<(), WorkerError> {
        self.tasks.append_extension(
            super::JOURNAL_OWNER,
            json!({"phase": "abandoned", "child": pending.child}),
        )?;
        self.settle_unsent(&pending.parent, &pending.child, now, effects)
    }

    /// Resends the messages journaled as `prepared` before a restart (0015-062). Each resend
    /// carries the recorded message and transport identifier, so the child worker answers with
    /// the child it already holds or creates it once. Runs before cancels flush, so a resumed
    /// child of a parent that failed at restart is canceled.
    ///
    /// A resend the remote answers with a refusal abandons the child. A resend that gets no
    /// answer leaves the child `prepared` and is repeated with doubling backoff: the remote may
    /// already hold the message but its task id is unknown, so the child can be neither closed
    /// nor canceled yet (0015-117). After `RESEND_ATTEMPTS` unanswered sends, or past the
    /// child's deadline, it is abandoned and audited as unconfirmed.
    pub(crate) async fn resume_sends(
        &mut self,
        d: &mut Delegation<B>,
        now: u64,
        effects: &mut Vec<Effect>,
    ) -> Result<(), WorkerError> {
        // An entry leaves `d.pending` only once its outcome is decided, so a pass interrupted
        // at the await still leaves the child to be repeated.
        let mut index = 0;
        while index < d.pending.len() {
            let mut pending = d.pending[index].clone();
            if pending.retry_at > now {
                index += 1;
                continue;
            }
            match self.attempt_send(d, &pending).await? {
                Attempt::Accepted(remote) => {
                    let recorded = self.record_sent(d, &pending, *remote, now, effects).await?;
                    d.pending.remove(index);
                    if recorded.is_some() {
                        self.audit(
                            Some(pending.parent.as_str()),
                            format!(
                                "DELEGATION_CHILD_RESUMED child={} target={}",
                                pending.child,
                                pending.target.as_str()
                            ),
                        );
                    }
                }
                Attempt::Refused => {
                    self.abandon(&pending, now, effects)?;
                    d.pending.remove(index);
                }
                Attempt::Unanswered => {
                    pending.attempts += 1;
                    let after = self.clock.now_ms().ok_or(WorkerError::Clock)?;
                    if pending.attempts >= RESEND_ATTEMPTS || after >= pending.deadline {
                        self.audit(
                            Some(pending.parent.as_str()),
                            format!(
                                "DELEGATION_CHILD_UNCONFIRMED child={} target={} attempts={}",
                                pending.child,
                                pending.target.as_str(),
                                pending.attempts
                            ),
                        );
                        self.abandon(&pending, after, effects)?;
                        d.pending.remove(index);
                    } else {
                        let backoff = RESEND_BACKOFF_MS << (pending.attempts - 1).min(16);
                        pending.retry_at = after.saturating_add(backoff);
                        d.pending[index] = pending;
                        index += 1;
                    }
                }
            }
        }
        Ok(())
    }

    /// Refusals (a) to (f) of 0015-054 and the outbound message for a child.
    /// Refusals (c) and (d) that loop also fail the delegating task (0015-057).
    fn prepare(
        &mut self,
        d: &Delegation<B>,
        task: &str,
        params: &SendParams,
        now: u64,
        effects: &mut Vec<Effect>,
    ) -> Result<Prepared, IpcError> {
        let owner = self.tasks.task_owner(task).ok_or_else(unavailable)?;
        let target = AgentId::parse(&params.agent).map_err(|_| malformed())?;
        let context = child_context(&owner.context_id, params.context_id.as_deref())?;
        check_allowlist(&d.config.peers, &d.own, &target)?;
        if matches!(
            self.tasks.breaker_status(target.as_str(), now),
            BreakerStatus::Open { .. }
        ) {
            return Err(IpcError::new(reason::RATE_LIMITED).with_meta("breaker", "open"));
        }
        let (mut budget, usage) = self.tasks.budget_view(task).ok_or_else(unavailable)?;
        if let Some(chain) = self.peers.chains.get(task) {
            budget.chain = chain.clone();
        }
        // 0015-054 (c) / 0006-059: a target already in this task's chain is re-entrant
        // delegation whatever the text; the pair rule (0006-045) is checked by the client.
        let reentrant = budget::is_reentrant(&budget.chain, target.as_str());
        let fingerprint = client_budget::text_fingerprint(&target, &params.text);
        let child = if reentrant {
            Err(ClientError::LoopDetected)
        } else {
            client_budget::child(&budget, &usage, &target, &fingerprint)
        };
        let child_budget = match child {
            Ok(child_budget) => child_budget,
            Err(ClientError::LoopDetected) => {
                let failed = self
                    .tasks
                    .fail_parent(
                        task,
                        FailureCode::LoopDetected,
                        Some(
                            crate::worker::cause(
                                "LOOP_DETECTED",
                                "peer call refused: delegation loop",
                            )
                            .to_string(),
                        ),
                        now,
                    )
                    .map_err(|_| unavailable())?;
                effects.extend(failed);
                return Err(IpcError::new(reason::LOOP_DETECTED));
            }
            Err(err) => return Err(budget_refusal(&err)),
        };
        if let Some(dimension) = exhausted_dimension(
            child_budget.max_tokens,
            child_budget.max_cost_units,
            child_budget.max_wall_ms,
        ) {
            return Err(budget_exhausted(dimension));
        }
        let binding = self.peers.bindings.get(task).ok_or_else(unavailable)?;
        if !binding.sends.allows(now, d.config.max_sends_per_min) {
            return Err(IpcError::new(reason::RATE_LIMITED));
        }
        let open = d
            .children
            .values()
            .filter(|c| c.parent == task && c.settled.is_none())
            .count();
        check_inflight(
            u32::try_from(open).unwrap_or(u32::MAX),
            d.config.max_inflight,
        )?;

        let hop = u32::try_from(child_budget.chain.len().saturating_sub(1)).unwrap_or(u32::MAX);
        let deadline = call_deadline(now, params.timeout_ms, child_budget.max_wall_ms);
        let mut message = Outbound::new(vec![outbound::text(&params.text)]);
        message.context_id = Some(context_id(&context)?);
        message.budget = Some(child_budget);
        message.return_immediately = true;
        message.metadata = Some(
            serde_json::from_value(json!({
                ext::PROVENANCE: {
                    "originAgent": self.config.agent.as_str(),
                    "delegatingTaskId": task,
                    "hop": hop,
                }
            }))
            .map_err(|_| unavailable())?,
        );
        Ok(Prepared {
            target,
            context,
            hop,
            deadline,
            message,
        })
    }

    /// `a2a_get`: the current view of a child of this task.
    pub(crate) fn get_call(
        &mut self,
        d: &mut Delegation<B>,
        request: ServerRequest,
        task: &str,
        params: &GetParams,
    ) {
        let child_id = match parse_task_id(&params.task_id) {
            Ok(id) => id.as_str().to_owned(),
            Err(err) => return self.refuse(Some(task), request, err),
        };
        let Some(child) = d.children.get(&child_id).filter(|c| c.parent == task) else {
            return self.refuse(Some(task), request, IpcError::new(reason::TASK_NOT_FOUND));
        };
        let mut answer = current(&child_id, child);
        if !params.include_text {
            if let Ok(result) = &mut answer {
                result.text.clear();
            }
        }
        respond_outcome(request, &answer);
    }

    /// `a2a_cancel`: requests cancellation of one of this task's children (0015-069).
    pub(crate) fn cancel_child(
        &mut self,
        d: &mut Delegation<B>,
        request: ServerRequest,
        task: &str,
        raw_task_id: &str,
    ) {
        let child_id = match parse_task_id(raw_task_id) {
            Ok(id) => id.as_str().to_owned(),
            Err(err) => return self.refuse(Some(task), request, err),
        };
        let Some(child) = d.children.get_mut(&child_id).filter(|c| c.parent == task) else {
            return self.refuse(Some(task), request, IpcError::new(reason::TASK_NOT_FOUND));
        };
        if child.settled.is_none() && !child.cancel_requested {
            child.cancel_requested = true;
            self.peers.cancels.push(child_id.clone());
        }
        let state = state_name(child.state).to_owned();
        respond_value(
            request,
            &CancelResult {
                task_id: child_id,
                state,
            },
        );
    }

    /// `cancel-call`: abandons a waiting `a2a_send` and cancels its child (0015-046).
    /// An unknown call id is acknowledged too, so the shim may retry safely.
    pub(crate) fn cancel_call(
        &mut self,
        d: &mut Delegation<B>,
        request: ServerRequest,
        task: &str,
        call_id: u64,
    ) {
        let waiting = d
            .children
            .iter_mut()
            .find(|(_, c)| c.parent == task && c.waiters.iter().any(|w| w.call_id == call_id));
        if let Some((child_id, child)) = waiting {
            child.waiters.retain(|w| w.call_id != call_id);
            if child.settled.is_none() && !child.cancel_requested {
                child.cancel_requested = true;
                self.peers.cancels.push(child_id.clone());
            }
        }
        respond_value(request, &json!({ "ok": true }));
    }

    /// `a2a_discover`: allowlisted peers only, filtered by skill and tag (0015-110 to 0015-113).
    pub(crate) async fn discover(
        &mut self,
        d: &mut Delegation<B>,
        request: ServerRequest,
        task: &str,
        params: &DiscoverParams,
    ) -> Result<(), WorkerError> {
        if let Err(err) = validate_discover(params) {
            self.refuse(Some(task), request, err);
            return Ok(());
        }
        let mut directory = Directory::new(&d.client);
        let synced = self.await_peer(directory.sync()).await?;
        if synced.is_err() {
            self.refuse(Some(task), request, unavailable());
            return Ok(());
        }
        let listings = match directory.all(false) {
            Ok(listings) => listings,
            Err(_) => {
                self.refuse(Some(task), request, unavailable());
                return Ok(());
            }
        };
        let limit = params.limit.unwrap_or(DEFAULT_DISCOVER_LIMIT) as usize;
        let mut peers = Vec::new();
        for listing in listings {
            if peers.len() >= limit {
                break;
            }
            let card = listing.card;
            let agent = card.agent();
            if !d.config.peers.contains(agent) {
                continue;
            }
            let value = card.card();
            let (skills, tags) = card_skills(value);
            if let Some(skill) = &params.skill {
                if !skills.iter().any(|s| s.id == *skill || s.name == *skill) {
                    continue;
                }
            }
            if let Some(tag) = &params.tag {
                if !tags.iter().any(|t| t == tag) {
                    continue;
                }
            }
            let name: String = value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .chars()
                .filter(|c| !c.is_control())
                .take(MAX_PEER_NAME_CHARS)
                .collect();
            let description: String = value
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .chars()
                .filter(|c| !c.is_control())
                .take(MAX_PEER_DESCRIPTION_CHARS)
                .collect();
            let description = match frame(&description, agent.as_str()) {
                Ok(framed) => framed,
                Err(err) => {
                    respond_err(request, err);
                    return Ok(());
                }
            };
            peers.push(PeerEntry {
                agent: agent.as_str().to_owned(),
                name,
                description,
                skills,
                tags,
            });
        }
        let text = format!("{} peer(s) available", peers.len());
        respond_value(request, &DiscoverResult { text, peers });
        Ok(())
    }
}

/// Answers a call for an existing child: waits when blocking and pending,
/// otherwise returns the current view.
fn replay<B: Bus>(d: &mut Delegation<B>, request: ServerRequest, child_id: &str, blocking: bool) {
    let call_id = request.frame.id();
    let Some(child) = d.children.get_mut(child_id) else {
        return respond_err(request, unavailable());
    };
    if blocking && child.settled.is_none() {
        let deadline = child.deadline;
        child.waiters.push(Waiter {
            request,
            call_id,
            deadline,
        });
    } else {
        respond_outcome(request, &current(child_id, child));
    }
}

/// Skills and tags of an agent card (RFC-0015 0015-110). Missing fields are empty.
fn card_skills(card: &Value) -> (Vec<SkillRef>, Vec<String>) {
    let mut skills = Vec::new();
    let mut tags = Vec::new();
    for skill in card
        .get("skills")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let id = skill.get("id").and_then(Value::as_str).unwrap_or_default();
        let name = skill
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        skills.push(SkillRef {
            id: id.to_owned(),
            name: name.to_owned(),
        });
        for tag in skill
            .get("tags")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if !tags.iter().any(|t| t == tag) {
                tags.push(tag.to_owned());
            }
        }
    }
    (skills, tags)
}
