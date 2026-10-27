//! The message a requester sends: new work, a continuation, or an approval decision.

use a2a_types::ids::{ContextId, MessageId, TaskId};
use a2a_types::proto::a2a as pb;
use a2a_types::proto::a2abus;
use pbjson_types::Struct;

use crate::budget;
use crate::error::ClientError;

/// A user message to send through `SendMessage` or `SendStreamingMessage`.
#[derive(Debug, Clone)]
pub struct Outbound {
    /// Opaque A2A message identifier, forwarded unchanged in `Message.messageId` (RFC-0008
    /// 0008-082). Retries resend it unchanged (RFC-0005 0005-034).
    pub message_id: MessageId,
    /// Transport identifier sent as `A2AB-Msg-Id`: a fresh UUIDv7 per outbound message
    /// (RFC-0002 0002-031). Retries reuse it so the worker can dedupe them.
    pub(crate) transport_id: MessageId,
    /// Task to continue; `None` creates a new task (RFC-0005 0005-047).
    pub task_id: Option<TaskId>,
    /// Context of the task; required when `task_id` is set (RFC-0005 0005-002).
    pub context_id: Option<ContextId>,
    /// Message parts.
    pub parts: Vec<pb::Part>,
    /// Extra message metadata.
    pub metadata: Option<Struct>,
    /// Budget granted to the receiving task (RFC-0006 0006-030).
    pub budget: Option<a2abus::Budget>,
    /// Return once the task is committed instead of waiting (RFC-0005 0005-046).
    pub return_immediately: bool,
}

impl Outbound {
    /// Creates a message that starts a new task.
    #[must_use]
    pub fn new(parts: Vec<pb::Part>) -> Self {
        Self {
            message_id: MessageId::generate(),
            transport_id: MessageId::generate(),
            task_id: None,
            context_id: None,
            parts,
            metadata: None,
            budget: None,
            return_immediately: false,
        }
    }

    /// Creates a message that continues an existing task (RFC-0002 0002-064, RFC-0005 0005-008).
    #[must_use]
    pub fn continuation(task: TaskId, context: ContextId, parts: Vec<pb::Part>) -> Self {
        Self {
            task_id: Some(task),
            context_id: Some(context),
            ..Self::new(parts)
        }
    }

    /// Replaces the message identifier, for example to resend an identical message.
    #[must_use]
    pub fn with_message_id(mut self, id: MessageId) -> Self {
        self.message_id = id;
        self
    }

    /// Attaches a budget for the receiving task (RFC-0006 0006-030).
    #[must_use]
    pub fn with_budget(mut self, budget: a2abus::Budget) -> Self {
        self.budget = Some(budget);
        self
    }

    /// Transport identifier sent as `A2AB-Msg-Id` (RFC-0002 0002-031). A resend of this message
    /// must reuse it so the worker dedupes it (RFC-0015 0015-062).
    #[must_use]
    pub fn transport_id(&self) -> &MessageId {
        &self.transport_id
    }

    /// Rebuilds a message from the protobuf form returned by [`Outbound::to_message`], the
    /// transport identifier it was first sent under, and its `return_immediately` flag
    /// (RFC-0015 0015-062). The flag lives in the request configuration rather than the message,
    /// so the caller journals it with the message. Returns `None` when an identifier is
    /// malformed. The budget stays in `message.metadata`, so `budget` is `None`.
    #[must_use]
    pub fn from_message(
        message: pb::Message,
        transport_id: MessageId,
        return_immediately: bool,
    ) -> Option<Self> {
        Some(Self {
            message_id: MessageId::parse(&message.message_id).ok()?,
            transport_id,
            task_id: match message.task_id.as_str() {
                "" => None,
                id => Some(TaskId::parse(id).ok()?),
            },
            context_id: match message.context_id.as_str() {
                "" => None,
                id => Some(ContextId::parse(id).ok()?),
            },
            parts: message.parts,
            metadata: message.metadata,
            budget: None,
            return_immediately,
        })
    }

    /// Builds the protobuf message, attaching the budget to its metadata.
    pub fn to_message(&self) -> Result<pb::Message, ClientError> {
        let mut metadata = self.metadata.clone();
        if let Some(budget) = &self.budget {
            budget::attach(&mut metadata, budget)?;
        }
        Ok(pb::Message {
            message_id: self.message_id.as_str().to_owned(),
            context_id: self
                .context_id
                .as_ref()
                .map(|c| c.as_str().to_owned())
                .unwrap_or_default(),
            task_id: self
                .task_id
                .as_ref()
                .map(|t| t.as_str().to_owned())
                .unwrap_or_default(),
            role: pb::Role::User as i32,
            parts: self.parts.clone(),
            metadata,
            ..Default::default()
        })
    }
}

/// Builds a text part.
#[must_use]
pub fn text(text: &str) -> pb::Part {
    pb::Part {
        content: Some(pb::part::Content::Text(text.to_owned())),
        ..Default::default()
    }
}
