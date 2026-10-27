//! Raw bus requests for the protocol-level negatives (RFC-0002 [0002-023], [0002-034],
//! [0002-080], [0002-081]). The rig's client only sends well-formed requests, so these build the
//! envelope directly and may change one field before it reaches the bus.

use std::time::Duration;

use a2a_core::envelope::{self, Clock, ReplyDecodeError, RequestBody, RequestParts};
use a2a_core::errors::{WireError, classify};
use a2a_core::headers as hdr;
use a2a_core::subject::Op;
use a2a_types::ids::MessageId;
use a2a_types::proto::a2a as pb;
use bus_api::bus::Bus as _;
use bus_api::message::Reply;
use bus_api::name::Subject;

use super::rig::{Bus, TENANT, VirtualClock, WORKER, agent, principal, tenant};

/// Relative expiry of the requests built here (RFC-0002 [0002-034]).
const TTL_MS: u64 = 60_000;

/// Departures from a well-formed request, applied before publish.
#[derive(Default)]
pub(crate) struct Tamper {
    /// Overrides `A2AB-Ts`, in Unix milliseconds.
    pub(crate) ts_ms: Option<u64>,
    /// Extension URIs to activate (`A2A-Extensions`).
    pub(crate) extensions: Vec<String>,
    /// Replaces the `A2A-Version` header.
    pub(crate) version: Option<&'static str>,
    /// Publishes to `rpc.worker.<token>` instead of the operation's own subject.
    pub(crate) op_token: Option<&'static str>,
}

/// A `SendMessage` body carrying one user text part, in `context` when given.
pub(crate) fn send_body(text: &str, context: Option<&str>) -> RequestBody {
    RequestBody::SendMessage(pb::SendMessageRequest {
        tenant: TENANT.to_owned(),
        configuration: Some(pb::SendMessageConfiguration {
            return_immediately: true,
            ..Default::default()
        }),
        message: Some(pb::Message {
            message_id: MessageId::generate().as_str().to_owned(),
            context_id: context.unwrap_or_default().to_owned(),
            role: pb::Role::User as i32,
            parts: vec![pb::Part {
                content: Some(pb::part::Content::Text(text.to_owned())),
                ..Default::default()
            }],
            ..Default::default()
        }),
        ..Default::default()
    })
}

/// Publishes one request from `sender` to the worker and waits for its reply.
pub(crate) async fn raw(
    bus: &Bus,
    clock: &VirtualClock,
    sender: &str,
    op: Op,
    body: RequestBody,
    tamper: Tamper,
) -> Result<Reply, String> {
    let now = Clock::now_ms(clock).ok_or("clock has no time")?;
    let parts = RequestParts {
        tenant: tenant()?,
        agent: agent(WORKER)?,
        msg_id: MessageId::generate(),
        sender: agent(sender)?,
        origin: None,
        ts_ms: tamper.ts_ms.unwrap_or(now),
        ttl_ms: TTL_MS,
        traceparent: None,
        tracestate: None,
        extensions: tamper.extensions,
        service_params: Vec::new(),
        json: false,
        op,
        body,
    };
    let mut out = envelope::encode_request(parts, envelope::MAX_INLINE_BYTES)
        .map_err(|e| format!("encode request: {e:?}"))?;
    if let Some(version) = tamper.version {
        out.headers
            .insert(hdr::A2A_VERSION, version)
            .map_err(|e| format!("version header: {e:?}"))?;
    }
    let subject = match tamper.op_token {
        Some(token) => Subject::parse(&format!("a2a.v1.{TENANT}.rpc.{WORKER}.{token}"))
            .map_err(|e| format!("subject: {e:?}"))?,
        None => out.subject,
    };
    bus.request(
        &principal(sender)?,
        &subject,
        &out.headers,
        out.payload,
        Duration::from_secs(60),
    )
    .await
    .map_err(|e| format!("bus request: {e:?}"))
}

/// The refusal a reply carries, classified against the RFC-0002 error model.
pub(crate) fn refusal(reply: &Reply) -> Result<WireError, String> {
    match envelope::decode_reply::<pb::SendMessageResponse>(reply) {
        Err(ReplyDecodeError::Failed(status)) => {
            classify(&status).ok_or_else(|| format!("unclassified refusal: {status:?}"))
        }
        Err(ReplyDecodeError::Invalid) => Err("reply headers or payload invalid".to_owned()),
        Ok(_) => Err("reply succeeded where a refusal was expected".to_owned()),
    }
}

/// The task a successful `SendMessage` reply carries.
pub(crate) fn accepted(reply: &Reply) -> Result<pb::Task, String> {
    match envelope::decode_reply::<pb::SendMessageResponse>(reply) {
        Ok(response) => match response.payload {
            Some(pb::send_message_response::Payload::Task(task)) => Ok(task),
            other => Err(format!("expected a Task reply, got {other:?}")),
        },
        Err(ReplyDecodeError::Failed(status)) => Err(format!("refused: {status:?}")),
        Err(ReplyDecodeError::Invalid) => Err("reply headers or payload invalid".to_owned()),
    }
}
