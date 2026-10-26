//! Requester runtime: unary operations, send, stream, subscribe and blocking waits.
//!
//! Every call builds one request envelope, sends it through the bus request path, verifies the
//! reply, and decodes it. Retries are explicit and reuse the message identifier (RFC-0005 0005-034).

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use a2a_core::envelope::{
    Clock, ReplyDecodeError, RequestBody, RequestParts, WireMessage, decode_reply, encode_request,
};
use a2a_core::errors::LocalError;
use a2a_core::headers::{self, MAX_TTL_MS, MIN_TTL_MS};
use a2a_core::subject::Op;
use a2a_types::ids::{AgentId, MessageId, TaskId, TenantId};
use a2a_types::proto::a2a as pb;
use a2a_types::proto::a2a::TaskState;
use bus_api::bus::Bus;
use bus_api::message::Reply;
use bus_api::name::Principal;

use crate::config::{CallOptions, Config};
use crate::error::ClientError;
use crate::outbound::Outbound;
use crate::stream::EventStream;

/// A reply value with the `A2AB-Last-Seq` header of the reply, when present.
#[derive(Debug, Clone)]
pub struct Unary<T> {
    /// Decoded reply payload.
    pub value: T,
    /// Task event sequence reported by the worker (RFC-0002 0002-051).
    pub last_seq: Option<u64>,
}

/// Reply to `SendMessage`.
#[derive(Debug, Clone)]
pub enum SendReply {
    /// The message created or advanced a task.
    Task(pb::Task),
    /// The worker answered with a direct message.
    Message(pb::Message),
}

/// Result of a blocking send (RFC-0002 0002-050).
#[derive(Debug, Clone)]
pub enum Outcome {
    /// The worker answered with a direct message.
    Message(pb::Message),
    /// The task reached a terminal state.
    Terminal(pb::Task),
    /// The task waits for input or authentication (RFC-0002 0002-064).
    Interrupted(pb::Task),
    /// The bound elapsed while the task was still in progress.
    Pending(pb::Task),
}

/// Result of a streaming send (RFC-0002 0002-053).
#[derive(Debug)]
pub enum StreamStart<'a, B: Bus> {
    /// The worker answered with a direct message; nothing follows.
    Message(pb::Message),
    /// The task is terminal or interrupted at reply time; no subscription was opened.
    Done(pb::Task),
    /// The task is in progress; events follow from the reply's `Last-Seq`.
    Follow {
        /// Task snapshot from the send reply.
        task: pb::Task,
        /// Ordered events after the snapshot.
        events: EventStream<'a, B>,
    },
}

/// Requester runtime bound to one bus, one tenant and one sending agent.
pub struct Client<B: Bus> {
    pub(crate) bus: B,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) config: Config,
    pub(crate) tenant: TenantId,
    pub(crate) sender: AgentId,
    pub(crate) principal: Principal,
}

impl<B: Bus> fmt::Debug for Client<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("tenant", &self.tenant)
            .field("sender", &self.sender)
            .finish_non_exhaustive()
    }
}

impl<B: Bus> Client<B> {
    /// Creates a client that sends as `sender`.
    ///
    /// # Errors
    /// Returns `Name` when the sender is not a valid principal name.
    pub fn new(
        bus: B,
        clock: Arc<dyn Clock>,
        config: Config,
        tenant: TenantId,
        sender: AgentId,
    ) -> Result<Self, ClientError> {
        let principal = Principal::named(sender.as_str()).map_err(ClientError::Name)?;
        Ok(Self {
            bus,
            clock,
            config,
            tenant,
            sender,
            principal,
        })
    }

    /// Tenant every request is scoped to.
    #[must_use]
    pub const fn tenant(&self) -> &TenantId {
        &self.tenant
    }

    /// Agent identifier the client sends as.
    #[must_use]
    pub const fn sender(&self) -> &AgentId {
        &self.sender
    }

    /// Fetches a task snapshot (`GetTask`).
    ///
    /// # Errors
    /// Returns the mapped bus, wire or local error of the call.
    pub async fn get_task(
        &self,
        agent: &AgentId,
        id: &TaskId,
        opts: &CallOptions,
    ) -> Result<Unary<pb::Task>, ClientError> {
        let body = RequestBody::GetTask(pb::GetTaskRequest {
            tenant: self.tenant_string(),
            id: id.as_str().to_owned(),
            ..Default::default()
        });
        self.call(agent, Op::Get, body, opts, MessageId::generate())
            .await
    }

    /// Lists tasks (`ListTasks`). The request tenant is always the client tenant.
    ///
    /// # Errors
    /// Returns the mapped bus, wire or local error of the call.
    pub async fn list_tasks(
        &self,
        agent: &AgentId,
        mut request: pb::ListTasksRequest,
        opts: &CallOptions,
    ) -> Result<Unary<pb::ListTasksResponse>, ClientError> {
        request.tenant = self.tenant_string();
        self.call(
            agent,
            Op::List,
            RequestBody::ListTasks(request),
            opts,
            MessageId::generate(),
        )
        .await
    }

    /// Requests cancellation of a task (`CancelTask`).
    ///
    /// # Errors
    /// Returns the mapped bus, wire or local error of the call.
    pub async fn cancel_task(
        &self,
        agent: &AgentId,
        id: &TaskId,
        opts: &CallOptions,
    ) -> Result<Unary<pb::Task>, ClientError> {
        let body = RequestBody::CancelTask(pb::CancelTaskRequest {
            tenant: self.tenant_string(),
            id: id.as_str().to_owned(),
            ..Default::default()
        });
        self.call(agent, Op::Cancel, body, opts, MessageId::generate())
            .await
    }

    /// Fetches the authenticated extended agent card (`GetExtendedAgentCard`).
    ///
    /// # Errors
    /// Returns the mapped bus, wire or local error of the call.
    pub async fn extended_card(
        &self,
        agent: &AgentId,
        opts: &CallOptions,
    ) -> Result<Unary<pb::AgentCard>, ClientError> {
        let body = RequestBody::GetExtendedAgentCard(pb::GetExtendedAgentCardRequest {
            tenant: self.tenant_string(),
        });
        self.call(agent, Op::CardExt, body, opts, MessageId::generate())
            .await
    }

    /// Creates a push notification configuration (`CreateTaskPushNotificationConfig`).
    ///
    /// # Errors
    /// Returns the mapped bus, wire or local error of the call.
    pub async fn push_create(
        &self,
        agent: &AgentId,
        mut config: pb::TaskPushNotificationConfig,
        opts: &CallOptions,
    ) -> Result<Unary<pb::TaskPushNotificationConfig>, ClientError> {
        config.tenant = self.tenant_string();
        self.call(
            agent,
            Op::PushCreate,
            RequestBody::PushCreate(config),
            opts,
            MessageId::generate(),
        )
        .await
    }

    /// Fetches a push notification configuration.
    ///
    /// # Errors
    /// Returns the mapped bus, wire or local error of the call.
    pub async fn push_get(
        &self,
        agent: &AgentId,
        mut request: pb::GetTaskPushNotificationConfigRequest,
        opts: &CallOptions,
    ) -> Result<Unary<pb::TaskPushNotificationConfig>, ClientError> {
        request.tenant = self.tenant_string();
        self.call(
            agent,
            Op::PushGet,
            RequestBody::PushGet(request),
            opts,
            MessageId::generate(),
        )
        .await
    }

    /// Lists push notification configurations of a task.
    ///
    /// # Errors
    /// Returns the mapped bus, wire or local error of the call.
    pub async fn push_list(
        &self,
        agent: &AgentId,
        mut request: pb::ListTaskPushNotificationConfigsRequest,
        opts: &CallOptions,
    ) -> Result<Unary<pb::ListTaskPushNotificationConfigsResponse>, ClientError> {
        request.tenant = self.tenant_string();
        self.call(
            agent,
            Op::PushList,
            RequestBody::PushList(request),
            opts,
            MessageId::generate(),
        )
        .await
    }

    /// Deletes a push notification configuration.
    ///
    /// # Errors
    /// Returns the mapped bus, wire or local error of the call.
    pub async fn push_delete(
        &self,
        agent: &AgentId,
        mut request: pb::DeleteTaskPushNotificationConfigRequest,
        opts: &CallOptions,
    ) -> Result<Unary<pbjson_types::Empty>, ClientError> {
        request.tenant = self.tenant_string();
        self.call(
            agent,
            Op::PushDelete,
            RequestBody::PushDelete(request),
            opts,
            MessageId::generate(),
        )
        .await
    }

    /// Sends a message once (`SendMessage`), honoring the outbound `return_immediately` flag.
    ///
    /// # Errors
    /// Returns the mapped bus, wire or local error of the call.
    pub async fn send(
        &self,
        agent: &AgentId,
        outbound: &Outbound,
        opts: &CallOptions,
    ) -> Result<Unary<SendReply>, ClientError> {
        self.send_with(agent, outbound, opts, outbound.return_immediately)
            .await
    }

    /// Sends a message, retrying up to `attempts` times on local deadline expiry.
    ///
    /// Every attempt carries the outbound message identifier, so the worker deduplicates them
    /// (RFC-0005 0005-034).
    ///
    /// # Errors
    /// Returns the last error when every attempt fails, or any error other than deadline expiry.
    pub async fn send_retrying(
        &self,
        agent: &AgentId,
        outbound: &Outbound,
        opts: &CallOptions,
        attempts: u32,
    ) -> Result<Unary<SendReply>, ClientError> {
        let attempts = attempts.max(1);
        let mut attempt = 1;
        loop {
            match self.send(agent, outbound, opts).await {
                Err(err)
                    if attempt < attempts
                        && matches!(err.local(), Some(LocalError::DeadlineExceeded)) =>
                {
                    attempt += 1;
                }
                other => return other,
            }
        }
    }

    /// Sends a message and follows its task events if the task is still in progress
    /// (RFC-0002 0002-053).
    ///
    /// # Errors
    /// Returns the mapped bus, wire or local error of the send or a missing `Last-Seq`.
    pub async fn stream(
        &self,
        agent: &AgentId,
        outbound: &Outbound,
        opts: &CallOptions,
    ) -> Result<StreamStart<'_, B>, ClientError> {
        let reply = self.send_streaming(agent, outbound, opts).await?;
        match reply.value {
            SendReply::Message(message) => Ok(StreamStart::Message(message)),
            SendReply::Task(task) => {
                let state = state_of(&task)?;
                if state.is_terminal() || state.is_interrupted() {
                    return Ok(StreamStart::Done(task));
                }
                let task_id = task_id_of(&task)?;
                let last = reply
                    .last_seq
                    .ok_or(ClientError::InvalidReply("missing Last-Seq on send reply"))?;
                let events = EventStream::new(self, agent.clone(), task_id, last);
                Ok(StreamStart::Follow { task, events })
            }
        }
    }

    /// Subscribes to a task (`SubscribeToTask`). Events after `after_seq` (or after the snapshot
    /// when `None`) are delivered by the returned stream (RFC-0002 0002-060, 0002-061).
    ///
    /// # Errors
    /// Returns the mapped bus, wire or local error of the call or a missing `Last-Seq`.
    pub async fn subscribe(
        &self,
        agent: &AgentId,
        id: &TaskId,
        after_seq: Option<u64>,
        opts: &CallOptions,
    ) -> Result<(pb::Task, EventStream<'_, B>), ClientError> {
        let mut call_opts = opts.clone();
        call_opts.after_seq = after_seq;
        let body = RequestBody::SubscribeToTask(pb::SubscribeToTaskRequest {
            tenant: self.tenant_string(),
            id: id.as_str().to_owned(),
        });
        let reply: Unary<pb::StreamResponse> = self
            .call(
                agent,
                Op::Subscribe,
                body,
                &call_opts,
                MessageId::generate(),
            )
            .await?;
        let task = match reply.value.payload {
            Some(pb::stream_response::Payload::Task(task)) => task,
            _ => {
                return Err(ClientError::InvalidReply(
                    "subscribe reply carries no task snapshot",
                ));
            }
        };
        let last = reply.last_seq.ok_or(ClientError::InvalidReply(
            "missing Last-Seq on subscribe reply",
        ))?;
        let events = EventStream::new(self, agent.clone(), id.clone(), after_seq.unwrap_or(last));
        Ok((task, events))
    }

    /// Sends a message and waits until the task is terminal, interrupted, or the bound elapses
    /// (RFC-0002 0002-050, RFC-0005 0005-040).
    ///
    /// The bound is the smaller of the caller timeout (or the configured task wall limit) and the
    /// budget wall limit of the outbound message.
    ///
    /// # Errors
    /// Returns `Local(DeadlineExceeded)` when the worker does not answer within the bound plus a
    /// one-second transit margin, and the mapped bus or wire error otherwise.
    pub async fn send_and_wait(
        &self,
        agent: &AgentId,
        outbound: &Outbound,
        opts: &CallOptions,
    ) -> Result<Outcome, ClientError> {
        let mut bound = opts.timeout.unwrap_or(self.config.max_task_wall);
        if let Some(budget) = &outbound.budget {
            bound = bound.min(Duration::from_millis(budget.max_wall_ms));
        }
        let ttl_ms = u64::try_from(bound.as_millis())
            .unwrap_or(u64::MAX)
            .clamp(MIN_TTL_MS, MAX_TTL_MS);
        let mut call_opts = opts.clone();
        call_opts.timeout = Some(bound + Duration::from_millis(a2a_core::budget::WALL_MARGIN_MS));
        call_opts.ttl_ms.get_or_insert(ttl_ms);
        let reply = self.send_with(agent, outbound, &call_opts, false).await?;
        match reply.value {
            SendReply::Message(message) => Ok(Outcome::Message(message)),
            SendReply::Task(task) => {
                let state = state_of(&task)?;
                Ok(if state.is_terminal() {
                    Outcome::Terminal(task)
                } else if state.is_interrupted() {
                    Outcome::Interrupted(task)
                } else {
                    Outcome::Pending(task)
                })
            }
        }
    }

    fn send_body(
        &self,
        outbound: &Outbound,
        return_immediately: bool,
    ) -> Result<RequestBody, ClientError> {
        let message = outbound.to_message()?;
        Ok(RequestBody::SendMessage(pb::SendMessageRequest {
            tenant: self.tenant_string(),
            message: Some(message),
            configuration: Some(pb::SendMessageConfiguration {
                return_immediately,
                ..Default::default()
            }),
            ..Default::default()
        }))
    }

    async fn send_with(
        &self,
        agent: &AgentId,
        outbound: &Outbound,
        opts: &CallOptions,
        return_immediately: bool,
    ) -> Result<Unary<SendReply>, ClientError> {
        let body = self.send_body(outbound, return_immediately)?;
        let reply: Unary<pb::SendMessageResponse> = self
            .call(agent, Op::Send, body, opts, outbound.message_id.clone())
            .await?;
        let value = match reply.value.payload {
            Some(pb::send_message_response::Payload::Task(task)) => SendReply::Task(task),
            Some(pb::send_message_response::Payload::Message(message)) => {
                SendReply::Message(message)
            }
            None => return Err(ClientError::InvalidReply("empty send reply")),
        };
        Ok(Unary {
            value,
            last_seq: reply.last_seq,
        })
    }

    /// Sends with `SendStreamingMessage` (`stream`): the reply carries the initial element only
    /// (RFC-0002 0002-020).
    async fn send_streaming(
        &self,
        agent: &AgentId,
        outbound: &Outbound,
        opts: &CallOptions,
    ) -> Result<Unary<SendReply>, ClientError> {
        let body = self.send_body(outbound, true)?;
        let reply: Unary<pb::StreamResponse> = self
            .call(agent, Op::Stream, body, opts, outbound.message_id.clone())
            .await?;
        let value = match reply.value.payload {
            Some(pb::stream_response::Payload::Task(task)) => SendReply::Task(task),
            Some(pb::stream_response::Payload::Message(message)) => SendReply::Message(message),
            _ => {
                return Err(ClientError::InvalidReply(
                    "stream reply carries neither task nor message",
                ));
            }
        };
        Ok(Unary {
            value,
            last_seq: reply.last_seq,
        })
    }

    /// Runs one request/reply exchange for `body` under the message identifier `msg_id`.
    async fn call<M: WireMessage>(
        &self,
        agent: &AgentId,
        op: Op,
        body: RequestBody,
        opts: &CallOptions,
        msg_id: MessageId,
    ) -> Result<Unary<M>, ClientError> {
        if let Some(traceparent) = &opts.traceparent {
            headers::validate_traceparent(traceparent).map_err(ClientError::Header)?;
        }
        if let Some(tracestate) = &opts.tracestate {
            headers::validate_tracestate(tracestate).map_err(ClientError::Header)?;
        }
        let timeout = opts.timeout.unwrap_or(self.config.request_timeout);
        let ttl_ms = opts.ttl_ms.unwrap_or(self.config.default_ttl_ms);
        let ts_ms = self.clock.now_ms().ok_or(ClientError::Clock)?;
        // Gateway origin is honored only when the gateway sends (RFC-0002 0002-112).
        let origin = if self.sender.as_str() == AgentId::gateway().as_str() {
            opts.origin.clone()
        } else {
            None
        };
        let parts = RequestParts {
            tenant: self.tenant.clone(),
            agent: agent.clone(),
            msg_id: msg_id.clone(),
            sender: self.sender.clone(),
            origin,
            ts_ms,
            ttl_ms,
            traceparent: opts.traceparent.clone(),
            tracestate: opts.tracestate.clone(),
            extensions: self.config.extensions.clone(),
            service_params: opts.service_params.clone(),
            json: self.config.json,
            op,
            body,
        };
        let mut outgoing =
            encode_request(parts, self.config.wire.max_inline).map_err(ClientError::Encode)?;
        if let Some(after) = opts.after_seq {
            outgoing
                .headers
                .insert(headers::AFTER_SEQ, &after.to_string())
                .map_err(ClientError::Name)?;
        }
        if let Some(hook) = &self.config.signing {
            hook.before_send(&mut outgoing)?;
        }
        let reply = self
            .bus
            .request(
                &self.principal,
                &outgoing.subject,
                &outgoing.headers,
                outgoing.payload,
                timeout,
            )
            .await
            .map_err(crate::error::from_bus)?;
        if let Some(verify) = &self.config.verify {
            verify.verify(&reply.headers, &reply.payload)?;
        }
        unary(&reply, &msg_id)
    }

    fn tenant_string(&self) -> String {
        self.tenant.as_str().to_owned()
    }
}

/// Decodes a reply after checking that it answers `request`.
fn unary<M: WireMessage>(reply: &Reply, request: &MessageId) -> Result<Unary<M>, ClientError> {
    let causation = reply
        .headers
        .get(headers::CAUSATION_ID)
        .ok_or(ClientError::InvalidReply("missing causation id"))?;
    if causation != request.as_str() {
        return Err(ClientError::InvalidReply(
            "causation id does not match request",
        ));
    }
    let last_seq = headers::optional(&reply.headers, headers::LAST_SEQ)
        .map(|value| headers::parse_u64(headers::LAST_SEQ, value))
        .transpose()
        .map_err(ClientError::Header)?;
    let value = match decode_reply::<M>(reply) {
        Ok(value) => value,
        Err(ReplyDecodeError::Failed(status)) => return Err(crate::error::from_status(status)),
        Err(ReplyDecodeError::Invalid) => {
            return Err(ClientError::InvalidReply("unexpected reply payload"));
        }
    };
    Ok(Unary { value, last_seq })
}

fn state_of(task: &pb::Task) -> Result<TaskState, ClientError> {
    let raw = task.status.as_ref().map_or(0, |status| status.state);
    TaskState::from_wire(raw).map_err(|_| ClientError::InvalidReply("invalid task state"))
}

fn task_id_of(task: &pb::Task) -> Result<TaskId, ClientError> {
    TaskId::parse(&task.id).map_err(|_| ClientError::InvalidReply("invalid task id"))
}
