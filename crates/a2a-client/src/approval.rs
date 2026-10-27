//! Approvals (RFC-0006): reading an open approval request and sending the operator's decision.
//!
//! The request travels in the status message of an `INPUT_REQUIRED` task under the approval
//! extension key. A decision is a `SendMessage` continuation of that task whose single part is
//! marked with the same key and carries an `ApprovalDecision` (0006-012). The decision names the
//! request digest (0006-051), so the worker can refuse a decision made against another request.

use a2a_core::approval::ALLOW_ONCE_ID;
use a2a_security::decision::request_digest;
use a2a_types::extension::{ExtensionUri, write_data};
use a2a_types::ids::{ContextId, TaskId};
use a2a_types::proto::a2a as pb;
use a2a_types::proto::a2abus;
use a2a_types::vocab::ext::APPROVAL;
use bus_api::bus::Bus;
use pbjson_types::{Struct, Timestamp};
use serde_json::{Value, json};

use crate::client::{Client, SendReply, Unary, Wire};
use crate::config::{CallOptions, SigningHook};
use crate::error::{ApprovalError, ClientError};
use crate::outbound::Outbound;

/// An open approval request, with the digest a decision must name ([0006-051]).
#[derive(Debug, Clone, PartialEq)]
pub struct PendingApproval {
    request: a2abus::ApprovalRequest,
    digest: String,
    task_id: TaskId,
    context_id: ContextId,
}

impl PendingApproval {
    /// Reads the open approval request of `task`.
    ///
    /// Returns `Ok(None)` when the task is not `INPUT_REQUIRED` or carries no approval request.
    ///
    /// # Errors
    /// Returns [`ApprovalError::Malformed`] when the request does not decode, or when its task id
    /// or the task's context id is not a valid identifier.
    pub fn from_task(task: &pb::Task) -> Result<Option<Self>, ClientError> {
        let Some(status) = &task.status else {
            return Ok(None);
        };
        if status.state != pb::TaskState::InputRequired as i32 {
            return Ok(None);
        }
        let Some(data) = status
            .message
            .as_ref()
            .and_then(|message| message.metadata.as_ref())
            .and_then(|meta| meta.fields.get(APPROVAL))
        else {
            return Ok(None);
        };
        let Some(pbjson_types::value::Kind::StructValue(request)) = &data.kind else {
            return Err(ClientError::Approval(ApprovalError::Malformed));
        };
        let request: a2abus::ApprovalRequest = from_struct(request)?;
        let task_id = TaskId::parse(&request.task_id)
            .map_err(|_| ClientError::Approval(ApprovalError::Malformed))?;
        let context_id = ContextId::parse(&task.context_id)
            .map_err(|_| ClientError::Approval(ApprovalError::Malformed))?;
        let digest = request_digest(&request);
        Ok(Some(Self {
            request,
            digest,
            task_id,
            context_id,
        }))
    }

    /// The approval request as received.
    #[must_use]
    pub const fn request(&self) -> &a2abus::ApprovalRequest {
        &self.request
    }

    /// Lowercase hex SHA-256 of the request's protobuf encoding ([0006-051]).
    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// Task that is waiting for the decision.
    #[must_use]
    pub const fn task_id(&self) -> &TaskId {
        &self.task_id
    }

    /// Context of the waiting task.
    #[must_use]
    pub const fn context_id(&self) -> &ContextId {
        &self.context_id
    }

    /// Checks that `option_id` is offered and the request has not expired at `now_ms`.
    ///
    /// # Errors
    /// Returns [`ApprovalError::OptionNotOffered`] or [`ApprovalError::Expired`].
    pub fn check(&self, option_id: &str, now_ms: u64) -> Result<(), ClientError> {
        if !self
            .request
            .options
            .iter()
            .any(|option| option.option_id == option_id)
        {
            return Err(ClientError::Approval(ApprovalError::OptionNotOffered));
        }
        let expires = self
            .request
            .expires_at
            .as_ref()
            .and_then(timestamp_ms)
            .ok_or(ClientError::Approval(ApprovalError::Malformed))?;
        if now_ms >= expires {
            return Err(ClientError::Approval(ApprovalError::Expired));
        }
        Ok(())
    }
}

/// The option that allows the action once, if the request offers it ([0006-016]).
#[must_use]
pub fn allow_once(pending: &PendingApproval) -> Option<&str> {
    pending
        .request
        .options
        .iter()
        .find(|option| option.option_id == ALLOW_ONCE_ID)
        .map(|option| option.option_id.as_str())
}

impl<B: Bus> Client<B> {
    /// Sends the operator's decision for `pending` to `agent` (RFC-0006 0006-012).
    ///
    /// The decision is a continuation of the waiting task in its context. It names the request
    /// id, task id, option, nonce and digest. `decidedBy` is left empty: the worker fills in the
    /// verified identity of the operator ([0007-098]).
    /// The option must be offered and the request must not have expired ([0006-013]).
    ///
    /// # Errors
    /// Returns an approval error for a refused option or expired request, a clock error, or any
    /// error from the send itself.
    pub async fn decide(
        &self,
        agent: &a2a_types::ids::AgentId,
        pending: &PendingApproval,
        option_id: &str,
        opts: &CallOptions,
    ) -> Result<Unary<SendReply>, ClientError> {
        self.decide_with(
            agent,
            pending,
            option_id,
            self.config.signing.as_deref(),
            opts,
        )
        .await
    }

    /// Sends the operator's decision with the envelope signed by `operator` (RFC-0007 0007-095).
    ///
    /// The message is the same as [`Self::decide`]. `operator` is a hook built on the operator
    /// key, such as [`crate::signing::envelope_hook`]. The verifier then checks the signature
    /// against the operator role and the relay sender of this client (`tui` or `cli`).
    ///
    /// # Errors
    /// As [`Self::decide`], plus any error the `operator` hook returns while signing.
    pub async fn decide_signed(
        &self,
        agent: &a2a_types::ids::AgentId,
        pending: &PendingApproval,
        option_id: &str,
        operator: &dyn SigningHook,
        opts: &CallOptions,
    ) -> Result<Unary<SendReply>, ClientError> {
        self.decide_with(agent, pending, option_id, Some(operator), opts)
            .await
    }

    async fn decide_with(
        &self,
        agent: &a2a_types::ids::AgentId,
        pending: &PendingApproval,
        option_id: &str,
        signing: Option<&dyn SigningHook>,
        opts: &CallOptions,
    ) -> Result<Unary<SendReply>, ClientError> {
        let now = self.clock.now_ms().ok_or(ClientError::Clock)?;
        pending.check(option_id, now)?;
        let outbound = decision_outbound(pending, option_id, now)?;
        // A decision is a follow-up on a task, so the worker needs the approval extension
        // activated on the request ([0006-012]).
        let mut extensions = self.config.extensions.clone();
        if !extensions.iter().any(|uri| uri == APPROVAL) {
            extensions.push(APPROVAL.to_owned());
        }
        self.send_with(
            agent,
            &outbound,
            opts,
            outbound.return_immediately,
            Wire {
                signing,
                extensions,
            },
        )
        .await
    }
}

fn decision_outbound(
    pending: &PendingApproval,
    option_id: &str,
    now_ms: u64,
) -> Result<Outbound, ClientError> {
    let decided_at = Timestamp {
        seconds: i64::try_from(now_ms / 1000).map_err(|_| ClientError::Clock)?,
        nanos: i32::try_from((now_ms % 1000) * 1_000_000).map_err(|_| ClientError::Clock)?,
    };
    let decision = json!({
        "requestId": pending.request.request_id,
        "taskId": pending.request.task_id,
        "optionId": option_id,
        "nonce": pending.request.nonce,
        "decidedAt": serde_json::to_value(decided_at).map_err(|_| ClientError::Clock)?,
        "requestSha256": pending.digest,
    });
    let data: pbjson_types::Value = serde_json::from_value(decision)
        .map_err(|_| ClientError::Approval(ApprovalError::Malformed))?;
    let mut metadata = Struct::default();
    let uri = ExtensionUri::parse(APPROVAL)
        .map_err(|_| ClientError::Approval(ApprovalError::Malformed))?;
    write_data(&mut metadata, &uri, Struct::default());
    let part = pb::Part {
        content: Some(pb::part::Content::Data(data)),
        metadata: Some(metadata),
        ..Default::default()
    };
    Ok(Outbound::continuation(
        pending.task_id.clone(),
        pending.context_id.clone(),
        vec![part],
    ))
}

fn from_struct<T: serde::de::DeserializeOwned>(value: &Struct) -> Result<T, ClientError> {
    let json: Value =
        serde_json::to_value(value).map_err(|_| ClientError::Approval(ApprovalError::Malformed))?;
    serde_json::from_value(json).map_err(|_| ClientError::Approval(ApprovalError::Malformed))
}

fn timestamp_ms(ts: &Timestamp) -> Option<u64> {
    let secs = u64::try_from(ts.seconds).ok()?;
    let nanos = u64::try_from(ts.nanos).ok()?;
    secs.checked_mul(1000)?.checked_add(nanos / 1_000_000)
}
