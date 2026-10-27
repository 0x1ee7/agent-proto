//! Request, reply and event envelopes and payload codec (RFC-0002 sections 4.5 to 4.8).
//!
//! [`receive`] runs the inbound validation pipeline for requests. A failure either drops the
//! request silently (foreign subjects) or answers it once with a `google.rpc.Status`.
//! [`encode_request`] and [`encode_event`] build outbound messages. Payloads are binary protobuf;
//! proto-JSON is a debug option that a receiver must opt into ([0002-035], [0002-037]).

use std::fmt::{self, Write as _};
use std::time::SystemTime;

use a2a_types::extension::{Activation, ExtensionError, ExtensionUri};
use a2a_types::ids::{AgentId, MessageId, TaskId, TenantId};
use a2a_types::proto::a2a as pb;
use a2a_types::proto::rpc;
use a2a_types::vocab;
use bus_api::error::BusError;
use bus_api::headers::Headers;
use bus_api::message::{Reply, ReplyHandle, Request as BusRequest};
use bus_api::name::{InvalidName, Principal, Subject};
use bytes::Bytes;
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::errors::{FieldViolation, StatusExtras, WireError};
use crate::headers::{self as hdr, HeaderError};
use crate::subject::{self, Family, Op, SubjectError};

/// Largest inline payload the binding allows, regardless of the bus limit ([0002-110]).
pub const MAX_INLINE_BYTES: usize = 262_144;

/// URI scheme of blob references in `Part.url` ([0002-111]).
pub const BLOB_SCHEME: &str = "a2abus-blob://";

/// Fully qualified name of the `google.rpc.Status` error payload.
pub const STATUS_TYPE: &str = <rpc::Status as WireMessage>::TYPE_NAME;

/// The inline limit for a bus whose maximum message size is `bus_max` ([0002-110]).
#[must_use]
pub fn inline_limit(bus_max: usize) -> usize {
    bus_max.min(MAX_INLINE_BYTES)
}

/// Source of receipt time for freshness checks ([0002-071], [0002-072]).
pub trait Clock: Send + Sync {
    /// Current Unix time in milliseconds, or `None` when the clock cannot be read.
    fn now_ms(&self) -> Option<u64>;
}

/// The operating-system wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> Option<u64> {
        let elapsed = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()?;
        u64::try_from(elapsed.as_millis()).ok()
    }
}

/// Receiver policy for inbound requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Extension URIs every request must activate, or `EXTENSION_SUPPORT_REQUIRED` ([0002-081]).
    pub required_extensions: Vec<String>,
    /// Allowance for sender clock skew in the staleness check ([0002-072]).
    pub skew_ms: u64,
    /// Largest accepted payload in bytes; see [`inline_limit`] ([0002-110], [0002-113]).
    pub max_inline: usize,
    /// Whether proto-JSON payloads are accepted as a debug option ([0002-037]).
    pub accept_json: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            required_extensions: Vec::new(),
            skew_ms: hdr::DEFAULT_SKEW_MS,
            max_inline: MAX_INLINE_BYTES,
            accept_json: false,
        }
    }
}

mod sealed {
    pub trait Sealed {}
}

/// A protobuf message that travels as a bus payload. The implemented set is closed.
pub trait WireMessage: sealed::Sealed + Sized {
    /// Fully qualified protobuf name, the value of `A2AB-Payload-Type`.
    const TYPE_NAME: &'static str;

    /// Encodes as protobuf, or as proto-JSON when `json` is set. `None` when serialization fails.
    fn encode_wire(&self, json: bool) -> Option<Vec<u8>>;

    /// Decodes protobuf, or proto-JSON when `json` is set. `None` when the bytes do not parse.
    fn decode_wire(bytes: &[u8], json: bool) -> Option<Self>;
}

macro_rules! wire_messages {
    ($($ty:ty => $name:literal;)*) => {$(
        impl sealed::Sealed for $ty {}

        impl WireMessage for $ty {
            const TYPE_NAME: &'static str = $name;

            fn encode_wire(&self, json: bool) -> Option<Vec<u8>> {
                if json {
                    serde_json::to_vec(self).ok()
                } else {
                    Some(prost::Message::encode_to_vec(self))
                }
            }

            fn decode_wire(bytes: &[u8], json: bool) -> Option<Self> {
                if json {
                    serde_json::from_slice(bytes).ok()
                } else {
                    <Self as prost::Message>::decode(bytes).ok()
                }
            }
        }
    )*};
}

wire_messages! {
    pb::SendMessageRequest => "lf.a2a.v1.SendMessageRequest";
    pb::GetTaskRequest => "lf.a2a.v1.GetTaskRequest";
    pb::ListTasksRequest => "lf.a2a.v1.ListTasksRequest";
    pb::CancelTaskRequest => "lf.a2a.v1.CancelTaskRequest";
    pb::SubscribeToTaskRequest => "lf.a2a.v1.SubscribeToTaskRequest";
    pb::TaskPushNotificationConfig => "lf.a2a.v1.TaskPushNotificationConfig";
    pb::GetTaskPushNotificationConfigRequest => "lf.a2a.v1.GetTaskPushNotificationConfigRequest";
    pb::ListTaskPushNotificationConfigsRequest => "lf.a2a.v1.ListTaskPushNotificationConfigsRequest";
    pb::DeleteTaskPushNotificationConfigRequest => "lf.a2a.v1.DeleteTaskPushNotificationConfigRequest";
    pb::GetExtendedAgentCardRequest => "lf.a2a.v1.GetExtendedAgentCardRequest";
    pb::SendMessageResponse => "lf.a2a.v1.SendMessageResponse";
    pb::StreamResponse => "lf.a2a.v1.StreamResponse";
    pb::Task => "lf.a2a.v1.Task";
    pb::ListTasksResponse => "lf.a2a.v1.ListTasksResponse";
    pb::ListTaskPushNotificationConfigsResponse => "lf.a2a.v1.ListTaskPushNotificationConfigsResponse";
    pb::AgentCard => "lf.a2a.v1.AgentCard";
    pbjson_types::Empty => "google.protobuf.Empty";
    rpc::Status => "google.rpc.Status";
}

/// A request body, one variant per A2A request message ([0002-021]). `Send` and `Stream` share
/// [`RequestBody::SendMessage`].
#[derive(Clone, PartialEq)]
#[allow(
    clippy::large_enum_variant,
    reason = "bodies are built once per request and moved, not stored in collections; boxing would add an allocation per request"
)]
pub enum RequestBody {
    /// `SendMessage` and `SendStreamingMessage`.
    SendMessage(pb::SendMessageRequest),
    /// `GetTask`.
    GetTask(pb::GetTaskRequest),
    /// `ListTasks`.
    ListTasks(pb::ListTasksRequest),
    /// `CancelTask`.
    CancelTask(pb::CancelTaskRequest),
    /// `SubscribeToTask`.
    SubscribeToTask(pb::SubscribeToTaskRequest),
    /// `CreateTaskPushNotificationConfig`, carried as the config message.
    PushCreate(pb::TaskPushNotificationConfig),
    /// `GetTaskPushNotificationConfig`.
    PushGet(pb::GetTaskPushNotificationConfigRequest),
    /// `ListTaskPushNotificationConfigs`.
    PushList(pb::ListTaskPushNotificationConfigsRequest),
    /// `DeleteTaskPushNotificationConfig`.
    PushDelete(pb::DeleteTaskPushNotificationConfigRequest),
    /// `GetExtendedAgentCard`.
    GetExtendedAgentCard(pb::GetExtendedAgentCardRequest),
}

impl fmt::Debug for RequestBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RequestBody({})", self.type_name())
    }
}

impl RequestBody {
    /// Protobuf name of this body, the value `A2AB-Payload-Type` takes for it.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::SendMessage(_) => <pb::SendMessageRequest as WireMessage>::TYPE_NAME,
            Self::GetTask(_) => <pb::GetTaskRequest as WireMessage>::TYPE_NAME,
            Self::ListTasks(_) => <pb::ListTasksRequest as WireMessage>::TYPE_NAME,
            Self::CancelTask(_) => <pb::CancelTaskRequest as WireMessage>::TYPE_NAME,
            Self::SubscribeToTask(_) => <pb::SubscribeToTaskRequest as WireMessage>::TYPE_NAME,
            Self::PushCreate(_) => <pb::TaskPushNotificationConfig as WireMessage>::TYPE_NAME,
            Self::PushGet(_) => {
                <pb::GetTaskPushNotificationConfigRequest as WireMessage>::TYPE_NAME
            }
            Self::PushList(_) => {
                <pb::ListTaskPushNotificationConfigsRequest as WireMessage>::TYPE_NAME
            }
            Self::PushDelete(_) => {
                <pb::DeleteTaskPushNotificationConfigRequest as WireMessage>::TYPE_NAME
            }
            Self::GetExtendedAgentCard(_) => {
                <pb::GetExtendedAgentCardRequest as WireMessage>::TYPE_NAME
            }
        }
    }

    /// The tenant the body carries; empty when unset.
    #[must_use]
    pub fn tenant(&self) -> &str {
        match self {
            Self::SendMessage(r) => &r.tenant,
            Self::GetTask(r) => &r.tenant,
            Self::ListTasks(r) => &r.tenant,
            Self::CancelTask(r) => &r.tenant,
            Self::SubscribeToTask(r) => &r.tenant,
            Self::PushCreate(r) => &r.tenant,
            Self::PushGet(r) => &r.tenant,
            Self::PushList(r) => &r.tenant,
            Self::PushDelete(r) => &r.tenant,
            Self::GetExtendedAgentCard(r) => &r.tenant,
        }
    }

    /// True when this body is the request message of `op` ([0002-036]).
    fn matches_op(&self, op: Op) -> bool {
        self.type_name() == op.request_type()
    }

    fn encode(&self, json: bool) -> Option<Vec<u8>> {
        match self {
            Self::SendMessage(m) => m.encode_wire(json),
            Self::GetTask(m) => m.encode_wire(json),
            Self::ListTasks(m) => m.encode_wire(json),
            Self::CancelTask(m) => m.encode_wire(json),
            Self::SubscribeToTask(m) => m.encode_wire(json),
            Self::PushCreate(m) => m.encode_wire(json),
            Self::PushGet(m) => m.encode_wire(json),
            Self::PushList(m) => m.encode_wire(json),
            Self::PushDelete(m) => m.encode_wire(json),
            Self::GetExtendedAgentCard(m) => m.encode_wire(json),
        }
    }
}

fn decode_request(op: Op, bytes: &[u8], json: bool) -> Option<RequestBody> {
    Some(match op {
        Op::Send | Op::Stream => RequestBody::SendMessage(WireMessage::decode_wire(bytes, json)?),
        Op::Get => RequestBody::GetTask(WireMessage::decode_wire(bytes, json)?),
        Op::List => RequestBody::ListTasks(WireMessage::decode_wire(bytes, json)?),
        Op::Cancel => RequestBody::CancelTask(WireMessage::decode_wire(bytes, json)?),
        Op::Subscribe => RequestBody::SubscribeToTask(WireMessage::decode_wire(bytes, json)?),
        Op::PushCreate => RequestBody::PushCreate(WireMessage::decode_wire(bytes, json)?),
        Op::PushGet => RequestBody::PushGet(WireMessage::decode_wire(bytes, json)?),
        Op::PushList => RequestBody::PushList(WireMessage::decode_wire(bytes, json)?),
        Op::PushDelete => RequestBody::PushDelete(WireMessage::decode_wire(bytes, json)?),
        Op::CardExt => RequestBody::GetExtendedAgentCard(WireMessage::decode_wire(bytes, json)?),
    })
}

/// Fields of an accepted request that the worker needs. Built only by [`receive`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestHead {
    /// Message identifier of the request ([0002-031]).
    pub msg_id: MessageId,
    /// Envelope sender, `A2AB-Sender` ([0002-011]).
    pub sender: AgentId,
    /// Effective origin: `A2AB-Origin` from the gateway, otherwise the sender ([0002-014]).
    pub origin: String,
    /// Tenant of the subject ([0002-010]).
    pub tenant: TenantId,
    /// Recipient agent named in the subject.
    pub agent: AgentId,
    /// Operation named in the subject.
    pub op: Op,
    /// Receipt time plus `A2AB-Ttl-Ms`, in Unix milliseconds ([0002-071]).
    pub deadline_ms: u64,
    /// W3C `traceparent`, if present.
    pub traceparent: Option<String>,
    /// W3C `tracestate`, if present.
    pub tracestate: Option<String>,
    /// Extensions activated on the request (`A2A-Extensions`).
    pub activation: Activation,
    /// Service parameters with lowercased keys ([0002-082]).
    pub service_params: Vec<(String, String)>,
    /// Blob references in the body, already checked against the tenant ([0002-112]).
    pub blob_refs: Vec<BlobRef>,
    /// True when the payload is proto-JSON; replies use the same encoding.
    pub json: bool,
    /// Replay position of a `subscribe`: events through this seq are already processed
    /// ([0002-061]). `None` when `A2AB-After-Seq` is absent.
    pub after_seq: Option<u64>,
}

/// An accepted request: its head, its body, and the one right to reply ([0002-040]).
pub struct Inbound {
    /// Validated request fields.
    pub head: RequestHead,
    /// Decoded request body.
    pub body: RequestBody,
    reply: ReplyHandle,
}

impl fmt::Debug for Inbound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Inbound")
            .field("head", &self.head)
            .field("body", &self.body)
            .finish_non_exhaustive()
    }
}

/// Optional parts of a successful reply.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplyOptions {
    /// Highest task sequence at reply time, `A2AB-Last-Seq` ([0002-051]).
    pub last_seq: Option<u64>,
    /// Extension URIs the worker applied; echoed in `A2A-Extensions`.
    pub applied_extensions: Vec<String>,
}

/// Failure to send a reply.
#[derive(Debug, Error)]
pub enum ReplyError {
    /// The message type does not match the operation's reply type ([0002-040]).
    #[error("reply payload type does not match the operation")]
    PayloadType,
    /// The payload could not be serialized.
    #[error("reply payload could not be serialized")]
    Serialize,
    /// An applied extension was not activated on the request.
    #[error("extension rejected: {0}")]
    Extension(#[from] ExtensionError),
    /// The bus refused the reply.
    #[error("bus rejected the reply: {0}")]
    Bus(#[from] BusError),
}

impl Inbound {
    /// Sends the one success reply of this request ([0002-040]). The payload type is the
    /// operation's reply type, and the reply uses the request's encoding.
    ///
    /// # Errors
    /// Returns [`ReplyError`] when the message type is wrong, serialization fails, an applied
    /// extension is not activated, or the bus refuses the reply.
    pub fn reply<M: WireMessage>(
        self,
        message: &M,
        options: &ReplyOptions,
    ) -> Result<(), ReplyError> {
        if M::TYPE_NAME != self.head.op.reply_type() {
            return Err(ReplyError::PayloadType);
        }
        let payload = message
            .encode_wire(self.head.json)
            .ok_or(ReplyError::Serialize)?;
        let mut headers = base_reply_headers(
            M::TYPE_NAME,
            self.head.json,
            Some(self.head.msg_id.as_str()),
            self.head.traceparent.as_deref(),
            self.head.tracestate.as_deref(),
        )?;
        if let Some(seq) = options.last_seq {
            put(&mut headers, hdr::LAST_SEQ, &seq.to_string())?;
        }
        let applied = options.applied_extensions.iter().map(String::as_str);
        let echoed = self.head.activation.echo(applied)?;
        if !echoed.is_empty() {
            put(&mut headers, hdr::A2A_EXTENSIONS, &echoed)?;
        }
        self.reply
            .reply(headers, Bytes::from(payload))
            .map_err(ReplyError::Bus)
    }

    /// Answers the request with a failure status ([0002-090] to [0002-096]).
    ///
    /// # Errors
    /// Returns [`BusError`] when the bus refuses the reply or the status cannot be serialized.
    pub fn fail(self, error: WireError, extras: &StatusExtras) -> Result<(), BusError> {
        self.fail_status(&error.status_with(extras))
    }

    /// Answers the request with a prepared failure status ([0002-090] to [0002-096]). Security
    /// refusals carry their RFC-0007 reason through this path.
    ///
    /// # Errors
    /// Returns [`BusError`] when the bus refuses the reply or the status cannot be serialized.
    pub fn fail_status(self, status: &rpc::Status) -> Result<(), BusError> {
        let payload = status
            .encode_wire(self.head.json)
            .ok_or(BusError::Internal)?;
        let headers = base_reply_headers(
            STATUS_TYPE,
            self.head.json,
            Some(self.head.msg_id.as_str()),
            self.head.traceparent.as_deref(),
            self.head.tracestate.as_deref(),
        )?;
        self.reply.reply(headers, Bytes::from(payload))
    }
}

/// A request that could not be accepted.
#[derive(Debug)]
pub enum Rejection {
    /// The request is not ours to answer (foreign or non-request subject). No reply is sent.
    Dropped,
    /// The request is answered once with a status.
    Answer(Answer),
}

impl Rejection {
    /// The wire error to answer with, or `None` when the request is dropped.
    #[must_use]
    pub fn error(&self) -> Option<WireError> {
        match self {
            Self::Dropped => None,
            Self::Answer(answer) => Some(answer.error),
        }
    }
}

/// The single failure reply to a rejected request. Replies are protobuf `google.rpc.Status`.
#[derive(Debug)]
pub struct Answer {
    reply: ReplyHandle,
    causation: Option<MessageId>,
    error: WireError,
    extras: StatusExtras,
}

impl Answer {
    /// The wire error this answer carries.
    #[must_use]
    pub fn error(&self) -> WireError {
        self.error
    }

    /// Sends the failure reply.
    ///
    /// # Errors
    /// Returns [`BusError`] when the bus refuses the reply.
    pub fn send(self) -> Result<(), BusError> {
        let payload = self
            .error
            .status_with(&self.extras)
            .encode_wire(false)
            .ok_or(BusError::Internal)?;
        let headers = base_reply_headers(
            STATUS_TYPE,
            false,
            self.causation.as_ref().map(MessageId::as_str),
            None,
            None,
        )?;
        self.reply.reply(headers, Bytes::from(payload))
    }
}

/// Outcome of a stop inside the validation pipeline.
enum Stop {
    Dropped,
    Error(WireError, StatusExtras),
}

impl From<WireError> for Stop {
    fn from(error: WireError) -> Self {
        Self::Error(error, StatusExtras::default())
    }
}

impl From<HeaderError> for Stop {
    fn from(error: HeaderError) -> Self {
        field_stop(error.field(), error.description())
    }
}

fn field_stop(field: &str, description: &str) -> Stop {
    Stop::Error(
        WireError::MalformedMessage,
        StatusExtras {
            field_violations: vec![FieldViolation {
                field: field.to_owned(),
                description: description.to_owned(),
            }],
            ..StatusExtras::default()
        },
    )
}

fn subject_stop(error: SubjectError) -> Stop {
    match error {
        SubjectError::Foreign => Stop::Dropped,
        SubjectError::Malformed => WireError::MalformedMessage.into(),
        SubjectError::UnknownOperation => WireError::UnknownOperation.into(),
    }
}

impl From<HeaderError> for WireError {
    fn from(_: HeaderError) -> Self {
        WireError::MalformedMessage
    }
}

/// Payload encoding allowed by the content type: protobuf, or JSON when `accept_json` is set.
/// Returns `true` for JSON.
fn encoding(headers: &Headers, accept_json: bool) -> Result<bool, WireError> {
    match hdr::optional(headers, hdr::CONTENT_TYPE) {
        Some(hdr::PROTOBUF_CONTENT_TYPE) => Ok(false),
        Some(hdr::JSON_CONTENT_TYPE) if accept_json => Ok(true),
        _ => Err(WireError::UnsupportedMediaType),
    }
}

fn content_type(json: bool) -> &'static str {
    if json {
        hdr::JSON_CONTENT_TYPE
    } else {
        hdr::PROTOBUF_CONTENT_TYPE
    }
}

fn is_stale(now: u64, ts: u64, ttl_ms: u64, skew_ms: u64) -> bool {
    // A future `ts` saturates to zero age, so it is not stale ([0002-072]).
    now.saturating_sub(ts) > ttl_ms.saturating_add(skew_ms)
}

/// Whether the transport principal is the sender ([0002-011], [0011-092]). The binding promises a
/// principal on every request; a missing or anonymous principal never matches and is refused.
fn principal_matches(principal: Option<&Principal>, sender: &AgentId) -> bool {
    principal.and_then(Principal::name) == Some(sender.as_str())
}

/// Runs the inbound pipeline of RFC-0002 section 4.4 to 4.10 on the subject, headers and payload.
fn validate(
    config: &Config,
    clock: &dyn Clock,
    principal: Option<&Principal>,
    subject: &Subject,
    headers: &Headers,
    payload: &Bytes,
) -> Result<(RequestHead, RequestBody), Stop> {
    let address = subject::parse(subject.as_str()).map_err(subject_stop)?;
    let Family::Rpc { agent, op } = address.family else {
        return Err(Stop::Dropped);
    };

    hdr::reject_authorization(headers)?;
    hdr::validate_all(headers)?;
    hdr::check_version(hdr::optional(headers, hdr::A2A_VERSION))?;

    let msg_id = hdr::parse_msg_id(hdr::MSG_ID, hdr::required(headers, hdr::MSG_ID)?)?;
    let sender = hdr::parse_agent_id(hdr::SENDER, hdr::required(headers, hdr::SENDER)?)?;
    if !principal_matches(principal, &sender) {
        return Err(WireError::PermissionDenied.into());
    }
    let ts = hdr::parse_ts(hdr::required(headers, hdr::TS)?)?;
    let ttl_ms = hdr::parse_ttl_ms(hdr::required(headers, hdr::TTL_MS)?)?;
    let payload_type = hdr::required(headers, hdr::PAYLOAD_TYPE)?;

    let now = clock.now_ms().ok_or(WireError::RequestExpired)?;
    if is_stale(now, ts, ttl_ms, config.skew_ms) {
        return Err(WireError::RequestExpired.into());
    }
    let deadline_ms = now.saturating_add(ttl_ms);

    let activation = match hdr::optional(headers, hdr::A2A_EXTENSIONS) {
        Some(value) => Activation::parse(value)
            .map_err(|_| field_stop(hdr::A2A_EXTENSIONS, "invalid extension list"))?,
        None => Activation::default(),
    };
    for uri in &config.required_extensions {
        if !activation.contains(uri) {
            return Err(WireError::ExtensionSupportRequired.into());
        }
    }

    let service_params = hdr::service_params(headers)?;

    let after_seq = hdr::optional(headers, hdr::AFTER_SEQ)
        .map(|value| hdr::parse_u64(hdr::AFTER_SEQ, value))
        .transpose()?;

    if payload.len() > config.max_inline {
        return Err(WireError::PayloadTooLarge.into());
    }
    let json = encoding(headers, config.accept_json)?;

    if payload_type != op.request_type() {
        return Err(WireError::MalformedMessage.into());
    }
    let body = decode_request(op, payload, json).ok_or(WireError::MalformedMessage)?;

    let body_tenant = body.tenant();
    if !body_tenant.is_empty() && body_tenant != address.tenant.as_str() {
        return Err(WireError::TenantMismatch.into());
    }

    let mut blob_refs = Vec::new();
    if let RequestBody::SendMessage(request) = &body {
        let parts = request
            .message
            .iter()
            .flat_map(|message| message.parts.iter());
        for part in parts {
            let Some(pb::part::Content::Url(url)) = &part.content else {
                continue;
            };
            if url.starts_with(BLOB_SCHEME) {
                blob_refs.push(BlobRef::parse(url, &address.tenant)?);
            }
        }
    }

    let origin = match hdr::optional(headers, hdr::ORIGIN) {
        Some(value) if sender == AgentId::gateway() => value.to_owned(),
        _ => sender.as_str().to_owned(),
    };

    let head = RequestHead {
        msg_id,
        sender,
        origin,
        tenant: address.tenant,
        agent,
        op,
        deadline_ms,
        traceparent: hdr::optional(headers, hdr::TRACEPARENT).map(str::to_owned),
        tracestate: hdr::optional(headers, hdr::TRACESTATE).map(str::to_owned),
        activation,
        service_params,
        blob_refs,
        json,
        after_seq,
    };
    Ok((head, body))
}

/// Validates an inbound request and takes ownership of its reply right.
///
/// # Errors
/// Returns [`Rejection::Dropped`] for foreign or non-request subjects, which get no reply.
/// Returns [`Rejection::Answer`] for every other failure; the caller sends it with
/// [`Answer::send`]. A sender that is not the transport principal gets `PERMISSION_DENIED`
/// ([0002-011]).
pub fn receive(
    config: &Config,
    clock: &dyn Clock,
    request: BusRequest,
) -> Result<Inbound, Rejection> {
    let BusRequest {
        subject,
        principal,
        headers,
        payload,
        reply,
    } = request;
    match validate(
        config,
        clock,
        principal.as_ref(),
        &subject,
        &headers,
        &payload,
    ) {
        Ok((head, body)) => Ok(Inbound { head, body, reply }),
        Err(Stop::Dropped) => Err(Rejection::Dropped),
        Err(Stop::Error(error, extras)) => {
            let causation = hdr::optional(&headers, hdr::MSG_ID)
                .and_then(|value| hdr::parse_msg_id(hdr::MSG_ID, value).ok());
            Err(Rejection::Answer(Answer {
                reply,
                causation,
                error,
                extras,
            }))
        }
    }
}

/// Reads a reply and decodes it as `M`. A `google.rpc.Status` reply is returned as
/// [`ReplyDecodeError::Failed`] so the caller can classify it.
///
/// # Errors
/// Returns [`ReplyDecodeError::Failed`] for status replies and [`ReplyDecodeError::Invalid`]
/// when the headers or payload do not match `M`.
pub fn decode_reply<M: WireMessage>(reply: &Reply) -> Result<M, ReplyDecodeError> {
    let json = encoding(&reply.headers, true).map_err(|_| ReplyDecodeError::Invalid)?;
    let Some(payload_type) = reply.headers.get(hdr::PAYLOAD_TYPE) else {
        return Err(ReplyDecodeError::Invalid);
    };
    if payload_type == STATUS_TYPE {
        let status =
            rpc::Status::decode_wire(&reply.payload, json).ok_or(ReplyDecodeError::Invalid)?;
        return Err(ReplyDecodeError::Failed(status));
    }
    if payload_type != M::TYPE_NAME {
        return Err(ReplyDecodeError::Invalid);
    }
    M::decode_wire(&reply.payload, json).ok_or(ReplyDecodeError::Invalid)
}

/// Why a reply did not decode to the expected message.
#[derive(Clone, PartialEq)]
pub enum ReplyDecodeError {
    /// The reply is a `google.rpc.Status` error. Classify it with [`crate::errors::classify`].
    Failed(rpc::Status),
    /// The reply headers or payload do not match the expected message.
    Invalid,
}

impl fmt::Debug for ReplyDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Failed(status) => write!(f, "Failed(code {})", status.code),
            Self::Invalid => f.write_str("Invalid"),
        }
    }
}

/// Failure to build an outbound message.
#[derive(Debug, Error)]
pub enum EncodeError {
    /// The request body is not the request message of the operation.
    #[error("request body does not match the operation")]
    BodyMismatch,
    /// The body's tenant differs from the subject tenant ([0002-010]).
    #[error("body tenant does not match the subject tenant")]
    TenantMismatch,
    /// The payload exceeds the inline limit and no blob reference was used ([0002-113]).
    #[error("payload exceeds the inline limit of {limit} bytes")]
    PayloadTooLarge {
        /// The inline limit that was exceeded.
        limit: usize,
    },
    /// The payload could not be serialized.
    #[error("payload could not be serialized")]
    Serialize,
    /// The timestamp is outside the representable range.
    #[error("timestamp is outside the representable range")]
    Timestamp,
    /// The TTL is outside `[1, 3600000]` ms ([0002-034]).
    #[error("ttl is outside the allowed range")]
    Ttl,
    /// A service parameter maps to a standard header and would clobber it ([0002-082]).
    #[error("service parameter {0} maps to a standard header")]
    ReservedParam(String),
    /// An extension URI is invalid.
    #[error("extension URI is invalid: {0}")]
    Extension(#[from] ExtensionError),
    /// A header name or value is rejected.
    #[error("header rejected: {0}")]
    Header(#[from] HeaderError),
    /// A subject or header name is invalid.
    #[error("name is not valid: {0}")]
    Name(#[from] InvalidName),
}

/// Fields of an outbound request. The caller supplies identifiers, time and the body.
#[derive(Debug)]
pub struct RequestParts {
    /// Tenant of the subject; the body's tenant must equal it.
    pub tenant: TenantId,
    /// Recipient agent.
    pub agent: AgentId,
    /// Message identifier; UUIDv7 ([0002-031]).
    pub msg_id: MessageId,
    /// Sender agent, `A2AB-Sender`.
    pub sender: AgentId,
    /// Origin peer id, honored by receivers only from the gateway ([0002-014]).
    pub origin: Option<String>,
    /// Send time in Unix milliseconds, `A2AB-Ts`.
    pub ts_ms: u64,
    /// Relative expiry in milliseconds, `A2AB-Ttl-Ms` ([0002-034]).
    pub ttl_ms: u64,
    /// W3C `traceparent`, if tracing is on.
    pub traceparent: Option<String>,
    /// W3C `tracestate`, if tracing is on.
    pub tracestate: Option<String>,
    /// Extension URIs to activate, `A2A-Extensions`.
    pub extensions: Vec<String>,
    /// Service parameters, carried as `A2AB-SP-<key>` ([0002-082]).
    pub service_params: Vec<(String, String)>,
    /// Use proto-JSON instead of protobuf (debug option, [0002-037]).
    pub json: bool,
    /// Operation to invoke.
    pub op: Op,
    /// Request body; must be the request message of `op`.
    pub body: RequestBody,
}

/// A built message: subject, headers and payload, ready for `Bus::request` or `Bus::publish`.
#[derive(Debug)]
pub struct Outgoing {
    /// Subject to publish to.
    pub subject: Subject,
    /// Binding headers.
    pub headers: Headers,
    /// Encoded payload.
    pub payload: Bytes,
}

fn set(headers: &mut Headers, name: &str, value: &str) -> Result<(), EncodeError> {
    hdr::validate_value(name, value)?;
    headers.insert(name, value)?;
    Ok(())
}

fn put(headers: &mut Headers, name: &str, value: &str) -> Result<(), BusError> {
    headers.insert(name, value).map_err(|_| BusError::Internal)
}

/// Builds a request, checking limits and the operation ([0002-036], [0002-034], [0002-113]).
///
/// # Errors
/// Returns [`EncodeError`] when the body does not match the operation or its tenant, the payload
/// exceeds `max_inline`, the TTL is out of range, a header or extension is invalid, or a service
/// parameter is reserved.
pub fn encode_request(parts: RequestParts, max_inline: usize) -> Result<Outgoing, EncodeError> {
    if !parts.body.matches_op(parts.op) {
        return Err(EncodeError::BodyMismatch);
    }
    if parts.body.tenant() != parts.tenant.as_str() {
        return Err(EncodeError::TenantMismatch);
    }
    let payload = parts
        .body
        .encode(parts.json)
        .ok_or(EncodeError::Serialize)?;
    if payload.len() > max_inline {
        return Err(EncodeError::PayloadTooLarge { limit: max_inline });
    }
    let ts_text = hdr::format_ts(parts.ts_ms).ok_or(EncodeError::Timestamp)?;
    if !(hdr::MIN_TTL_MS..=hdr::MAX_TTL_MS).contains(&parts.ttl_ms) {
        return Err(EncodeError::Ttl);
    }

    let mut headers = Headers::new();
    set(&mut headers, hdr::MSG_ID, parts.msg_id.as_str())?;
    set(&mut headers, hdr::SENDER, parts.sender.as_str())?;
    if let Some(origin) = &parts.origin {
        set(&mut headers, hdr::ORIGIN, origin)?;
    }
    set(&mut headers, hdr::TS, &ts_text)?;
    set(&mut headers, hdr::TTL_MS, &parts.ttl_ms.to_string())?;
    set(&mut headers, hdr::PAYLOAD_TYPE, parts.body.type_name())?;
    set(&mut headers, hdr::CONTENT_TYPE, content_type(parts.json))?;
    set(&mut headers, hdr::A2A_VERSION, vocab::A2A_VERSION)?;
    if !parts.extensions.is_empty() {
        for uri in &parts.extensions {
            ExtensionUri::parse(uri)?;
        }
        set(
            &mut headers,
            hdr::A2A_EXTENSIONS,
            &parts.extensions.join(", "),
        )?;
    }
    if let Some(traceparent) = &parts.traceparent {
        hdr::validate_traceparent(traceparent)?;
        set(&mut headers, hdr::TRACEPARENT, traceparent)?;
    }
    if let Some(tracestate) = &parts.tracestate {
        hdr::validate_tracestate(tracestate)?;
        set(&mut headers, hdr::TRACESTATE, tracestate)?;
    }
    for (key, value) in &parts.service_params {
        let name = hdr::service_param_header(key)?;
        if name == hdr::A2A_VERSION || name == hdr::A2A_EXTENSIONS {
            return Err(EncodeError::ReservedParam(key.clone()));
        }
        set(&mut headers, &name, value)?;
    }

    Ok(Outgoing {
        subject: subject::rpc(&parts.tenant, &parts.agent, parts.op)?,
        headers,
        payload: Bytes::from(payload),
    })
}

/// Fields of an outbound event on `ev.<agent>.<task_id>` ([0002-041]).
pub struct EventParts {
    /// Tenant of the subject.
    pub tenant: TenantId,
    /// Agent that produces the event.
    pub agent: AgentId,
    /// Task the event belongs to.
    pub task: TaskId,
    /// Message identifier; UUIDv7.
    pub msg_id: MessageId,
    /// W3C `traceparent`, if tracing is on.
    pub traceparent: Option<String>,
    /// W3C `tracestate`, if tracing is on.
    pub tracestate: Option<String>,
    /// True only on the stream-end event ([0002-044]).
    pub stream_end: bool,
    /// Use proto-JSON instead of protobuf.
    pub json: bool,
    /// The `StreamResponse` carried by the event.
    pub event: pb::StreamResponse,
}

impl fmt::Debug for EventParts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventParts")
            .field("tenant", &self.tenant)
            .field("agent", &self.agent)
            .field("task", &self.task)
            .field("msg_id", &self.msg_id)
            .field("stream_end", &self.stream_end)
            .finish_non_exhaustive()
    }
}

/// Builds an event.
///
/// # Errors
/// Returns [`EncodeError`] when the payload cannot be serialized, exceeds `max_inline`, or a
/// header or name is invalid.
pub fn encode_event(parts: EventParts, max_inline: usize) -> Result<Outgoing, EncodeError> {
    let payload = parts
        .event
        .encode_wire(parts.json)
        .ok_or(EncodeError::Serialize)?;
    if payload.len() > max_inline {
        return Err(EncodeError::PayloadTooLarge { limit: max_inline });
    }
    let mut headers = Headers::new();
    set(&mut headers, hdr::MSG_ID, parts.msg_id.as_str())?;
    set(
        &mut headers,
        hdr::PAYLOAD_TYPE,
        <pb::StreamResponse as WireMessage>::TYPE_NAME,
    )?;
    set(&mut headers, hdr::CONTENT_TYPE, content_type(parts.json))?;
    if parts.stream_end {
        set(&mut headers, hdr::STREAM_END, "true")?;
    }
    if let Some(traceparent) = &parts.traceparent {
        hdr::validate_traceparent(traceparent)?;
        set(&mut headers, hdr::TRACEPARENT, traceparent)?;
    }
    if let Some(tracestate) = &parts.tracestate {
        hdr::validate_tracestate(tracestate)?;
        set(&mut headers, hdr::TRACESTATE, tracestate)?;
    }
    Ok(Outgoing {
        subject: subject::event(&parts.tenant, &parts.agent, &parts.task)?,
        headers,
        payload: Bytes::from(payload),
    })
}

/// A received event on `ev.<agent>.<task_id>`.
pub struct InboundEvent {
    /// Tenant of the subject.
    pub tenant: TenantId,
    /// Producing agent.
    pub agent: AgentId,
    /// Task of the event.
    pub task: TaskId,
    /// Message identifier of the event.
    pub msg_id: MessageId,
    /// True when this is the stream-end event ([0002-044]).
    pub stream_end: bool,
    /// The `StreamResponse` carried by the event.
    pub event: pb::StreamResponse,
}

impl fmt::Debug for InboundEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InboundEvent")
            .field("tenant", &self.tenant)
            .field("agent", &self.agent)
            .field("task", &self.task)
            .field("msg_id", &self.msg_id)
            .field("stream_end", &self.stream_end)
            .finish_non_exhaustive()
    }
}

/// Decodes an event received on an event subject.
///
/// # Errors
/// Returns [`WireError::MalformedMessage`] for a foreign subject, bad headers, a wrong payload
/// type, or an undecodable payload. Returns [`WireError::UnsupportedMediaType`] for an unknown
/// content type.
/// Returns [`WireError::PermissionDenied`] when the principal is not the owning agent of the
/// subject ([0002-006], [0002-011]).
pub fn receive_event(
    config: &Config,
    principal: Option<&Principal>,
    subject: &Subject,
    headers: &Headers,
    payload: &Bytes,
) -> Result<InboundEvent, WireError> {
    let address = subject::parse(subject.as_str()).map_err(|_| WireError::MalformedMessage)?;
    let Family::Event { agent, task } = address.family else {
        return Err(WireError::MalformedMessage);
    };
    if !principal_matches(principal, &agent) {
        return Err(WireError::PermissionDenied);
    }
    hdr::validate_all(headers)?;
    let msg_id = hdr::parse_msg_id(hdr::MSG_ID, hdr::required(headers, hdr::MSG_ID)?)?;
    if hdr::required(headers, hdr::PAYLOAD_TYPE)? != <pb::StreamResponse as WireMessage>::TYPE_NAME
    {
        return Err(WireError::MalformedMessage);
    }
    let json = encoding(headers, config.accept_json)?;
    let stream_end = hdr::parse_stream_end(headers)?;
    let event = <pb::StreamResponse as WireMessage>::decode_wire(payload, json)
        .ok_or(WireError::MalformedMessage)?;
    Ok(InboundEvent {
        tenant: address.tenant,
        agent,
        task,
        msg_id,
        stream_end,
        event,
    })
}

fn base_reply_headers(
    payload_type: &str,
    json: bool,
    causation: Option<&str>,
    traceparent: Option<&str>,
    tracestate: Option<&str>,
) -> Result<Headers, BusError> {
    let mut headers = Headers::new();
    put(&mut headers, hdr::PAYLOAD_TYPE, payload_type)?;
    put(&mut headers, hdr::CONTENT_TYPE, content_type(json))?;
    if let Some(causation) = causation {
        put(&mut headers, hdr::CAUSATION_ID, causation)?;
    }
    if let Some(traceparent) = traceparent {
        put(&mut headers, hdr::TRACEPARENT, traceparent)?;
    }
    if let Some(tracestate) = tracestate {
        put(&mut headers, hdr::TRACESTATE, tracestate)?;
    }
    Ok(headers)
}

/// A blob reference `a2abus-blob://<tenant>/<sha256-hex>` ([0002-111]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobRef {
    tenant: TenantId,
    sha256: [u8; 32],
}

impl BlobRef {
    /// The reference for `bytes` stored under `tenant`.
    #[must_use]
    pub fn for_bytes(tenant: &TenantId, bytes: &[u8]) -> Self {
        Self {
            tenant: tenant.clone(),
            sha256: digest(bytes),
        }
    }

    /// Parses a blob URL that a request body carries.
    ///
    /// # Errors
    /// Returns [`WireError::MalformedMessage`] for a malformed URL, and
    /// [`WireError::PermissionDenied`] when the reference names another tenant ([0002-112]).
    pub fn parse(url: &str, expected: &TenantId) -> Result<Self, WireError> {
        let rest = url
            .strip_prefix(BLOB_SCHEME)
            .ok_or(WireError::MalformedMessage)?;
        let (tenant_text, hex) = rest.split_once('/').ok_or(WireError::MalformedMessage)?;
        let tenant = TenantId::parse(tenant_text).map_err(|_| WireError::MalformedMessage)?;
        let sha256 = parse_hex_digest(hex).ok_or(WireError::MalformedMessage)?;
        if &tenant != expected {
            return Err(WireError::PermissionDenied);
        }
        Ok(Self { tenant, sha256 })
    }

    /// The tenant the referenced content belongs to.
    #[must_use]
    pub fn tenant(&self) -> &TenantId {
        &self.tenant
    }

    /// The SHA-256 digest of the referenced content.
    #[must_use]
    pub fn sha256(&self) -> &[u8; 32] {
        &self.sha256
    }

    /// The URL form, with a lowercase hex digest.
    #[must_use]
    pub fn url(&self) -> String {
        let mut url = String::with_capacity(BLOB_SCHEME.len() + self.tenant.as_str().len() + 66);
        url.push_str(BLOB_SCHEME);
        url.push_str(self.tenant.as_str());
        url.push('/');
        for byte in self.sha256 {
            let _ = write!(url, "{byte:02x}");
        }
        url
    }

    /// Checks fetched bytes against the digest. A mismatch means the content is discarded
    /// ([0002-112]).
    ///
    /// # Errors
    /// Returns [`WireError::MalformedMessage`] when the digest does not match.
    pub fn verify(&self, bytes: &[u8]) -> Result<(), WireError> {
        if digest(bytes) == self.sha256 {
            Ok(())
        } else {
            Err(WireError::MalformedMessage)
        }
    }
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn parse_hex_digest(text: &str) -> Option<[u8; 32]> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0_u8; 32];
    for (slot, pair) in out.iter_mut().zip(bytes.chunks_exact(2)) {
        let [hi, lo] = pair else {
            return None;
        };
        *slot = (hex_nibble(*hi)? << 4) | hex_nibble(*lo)?;
    }
    Some(out)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;
    use crate::errors::{classify, retry_after};
    use bus_api::message::ReplySink;

    type TestResult = Result<(), String>;

    trait Must<T> {
        fn must(self) -> Result<T, String>;
    }

    impl<T, E: fmt::Debug> Must<T> for Result<T, E> {
        fn must(self) -> Result<T, String> {
            self.map_err(|error| format!("{error:?}"))
        }
    }

    const NOW: u64 = 1_750_000_000_000;
    const TTL: u64 = 60_000;
    const BUDGET: &str = "urn:a2abus:ext:budget:1";

    struct FixedClock(Option<u64>);

    impl Clock for FixedClock {
        fn now_ms(&self) -> Option<u64> {
            self.0
        }
    }

    struct Capture(mpsc::Sender<Reply>);

    impl ReplySink for Capture {
        fn send(self: Box<Self>, reply: Reply) -> Result<(), BusError> {
            self.0.send(reply).map_err(|_| BusError::Internal)
        }
    }

    fn capture() -> (ReplyHandle, mpsc::Receiver<Reply>) {
        let (tx, rx) = mpsc::channel();
        (ReplyHandle::new(Box::new(Capture(tx))), rx)
    }

    fn text_part(text: &str) -> pb::Part {
        pb::Part {
            content: Some(pb::part::Content::Text(text.to_owned())),
            ..Default::default()
        }
    }

    fn send_body(tenant: &str) -> RequestBody {
        RequestBody::SendMessage(pb::SendMessageRequest {
            tenant: tenant.to_owned(),
            message: Some(pb::Message {
                message_id: "m-1".to_owned(),
                parts: vec![text_part("hello")],
                ..Default::default()
            }),
            ..Default::default()
        })
    }

    fn request_parts(op: Op, body: RequestBody) -> Result<RequestParts, String> {
        Ok(RequestParts {
            tenant: TenantId::parse("acme").must()?,
            agent: AgentId::parse("worker").must()?,
            msg_id: MessageId::generate(),
            sender: AgentId::parse("planner").must()?,
            origin: None,
            ts_ms: NOW - 1_000,
            ttl_ms: TTL,
            traceparent: None,
            tracestate: None,
            extensions: Vec::new(),
            service_params: Vec::new(),
            json: false,
            op,
            body,
        })
    }

    fn encode(parts: RequestParts) -> Result<Outgoing, String> {
        encode_request(parts, MAX_INLINE_BYTES).must()
    }

    fn to_request(out: Outgoing) -> (BusRequest, mpsc::Receiver<Reply>) {
        let (reply, rx) = capture();
        let principal = out
            .headers
            .get(hdr::SENDER)
            .and_then(|name| Principal::named(name).ok());
        let request = BusRequest {
            subject: out.subject,
            principal,
            headers: out.headers,
            payload: out.payload,
            reply,
        };
        (request, rx)
    }

    fn without(headers: &Headers, name: &str) -> Headers {
        let mut kept = Headers::new();
        for (key, value) in headers.iter() {
            if key != name {
                let _ = kept.insert(key, value);
            }
        }
        kept
    }

    fn accept(config: &Config, clock: u64, request: BusRequest) -> Result<Inbound, String> {
        match receive(config, &FixedClock(Some(clock)), request) {
            Ok(inbound) => Ok(inbound),
            Err(rejection) => Err(format!("rejected: {:?}", rejection.error())),
        }
    }

    fn rejected(
        config: &Config,
        clock: Option<u64>,
        request: BusRequest,
    ) -> Result<WireError, String> {
        match receive(config, &FixedClock(clock), request) {
            Ok(_) => Err("request was accepted".to_owned()),
            Err(rejection) => rejection
                .error()
                .ok_or_else(|| "request was dropped".to_owned()),
        }
    }

    fn sent(rx: &mpsc::Receiver<Reply>) -> Result<Reply, String> {
        rx.try_recv().must()
    }

    #[test]
    fn req_0002_001_foreign_subject_is_dropped_without_reply() -> TestResult {
        let out = encode(request_parts(Op::Send, send_body("acme"))?)?;
        let (mut request, rx) = to_request(out);
        request.subject = Subject::parse("other.v1.acme.rpc.worker.send").must()?;
        let result = receive(&Config::default(), &FixedClock(Some(NOW)), request);
        assert!(matches!(result, Err(Rejection::Dropped)));
        assert!(rx.try_recv().is_err());
        Ok(())
    }

    #[test]
    fn req_0002_023_unknown_operation_is_answered_with_unsupported_operation() -> TestResult {
        let out = encode(request_parts(Op::Send, send_body("acme"))?)?;
        let msg_id = out
            .headers
            .get(hdr::MSG_ID)
            .map(str::to_owned)
            .ok_or("no msg id")?;
        let (mut request, rx) = to_request(out);
        request.subject = Subject::parse("a2a.v1.acme.rpc.worker.frobnicate").must()?;
        let Err(Rejection::Answer(answer)) =
            receive(&Config::default(), &FixedClock(Some(NOW)), request)
        else {
            return Err("expected an answer".to_owned());
        };
        assert_eq!(answer.error(), WireError::UnknownOperation);
        answer.send().must()?;
        let reply = sent(&rx)?;
        assert_eq!(reply.headers.get(hdr::CAUSATION_ID), Some(msg_id.as_str()));
        match decode_reply::<pb::Task>(&reply) {
            Err(ReplyDecodeError::Failed(status)) => {
                assert_eq!(classify(&status), Some(WireError::UnknownOperation));
            }
            _ => return Err("expected a status reply".to_owned()),
        }
        Ok(())
    }

    #[test]
    fn req_0002_020_body_must_match_operation_when_encoding() -> TestResult {
        let parts = request_parts(Op::Get, send_body("acme"))?;
        assert!(matches!(
            encode_request(parts, MAX_INLINE_BYTES),
            Err(EncodeError::BodyMismatch)
        ));
        Ok(())
    }

    #[test]
    fn req_0002_035_protobuf_round_trip_preserves_head_and_body() -> TestResult {
        let body = send_body("acme");
        let out = encode(request_parts(Op::Send, body.clone())?)?;
        assert_eq!(
            out.headers.get(hdr::CONTENT_TYPE),
            Some(hdr::PROTOBUF_CONTENT_TYPE)
        );
        let msg_id = out
            .headers
            .get(hdr::MSG_ID)
            .map(str::to_owned)
            .ok_or("no msg id")?;
        let (request, _rx) = to_request(out);
        let inbound = accept(&Config::default(), NOW, request)?;
        assert_eq!(inbound.head.msg_id.as_str(), msg_id);
        assert_eq!(inbound.head.tenant.as_str(), "acme");
        assert_eq!(inbound.head.agent.as_str(), "worker");
        assert_eq!(inbound.head.op, Op::Send);
        assert_eq!(inbound.head.sender.as_str(), "planner");
        assert_eq!(inbound.body, body);
        Ok(())
    }

    #[test]
    fn req_0002_036_payload_type_mismatch_and_unparsable_payload_are_malformed() -> TestResult {
        let out = encode(request_parts(Op::Send, send_body("acme"))?)?;
        let mut wrong_type = Headers::new();
        for (key, value) in out.headers.iter() {
            let value = if key == hdr::PAYLOAD_TYPE {
                "lf.a2a.v1.GetTaskRequest"
            } else {
                value
            };
            wrong_type.insert(key, value).must()?;
        }
        let (request, _rx) = to_request(Outgoing {
            subject: out.subject,
            headers: wrong_type,
            payload: out.payload,
        });
        assert_eq!(
            rejected(&Config::default(), Some(NOW), request)?,
            WireError::MalformedMessage
        );

        let out = encode(request_parts(Op::Send, send_body("acme"))?)?;
        let (request, _rx) = to_request(Outgoing {
            subject: out.subject,
            headers: out.headers,
            payload: Bytes::from_static(&[0xff, 0xff, 0xff]),
        });
        assert_eq!(
            rejected(&Config::default(), Some(NOW), request)?,
            WireError::MalformedMessage
        );
        Ok(())
    }

    #[test]
    fn req_0002_037_json_is_refused_unless_accepted() -> TestResult {
        let mut parts = request_parts(Op::Send, send_body("acme"))?;
        parts.json = true;
        let out = encode(parts)?;
        let (request, _rx) = to_request(Outgoing {
            subject: out.subject.clone(),
            headers: out.headers.clone(),
            payload: out.payload.clone(),
        });
        assert_eq!(
            rejected(&Config::default(), Some(NOW), request)?,
            WireError::UnsupportedMediaType
        );
        let config = Config {
            accept_json: true,
            ..Config::default()
        };
        let (request, _rx) = to_request(out);
        let inbound = accept(&config, NOW, request)?;
        assert!(inbound.head.json);
        assert_eq!(inbound.body, send_body("acme"));
        Ok(())
    }

    #[test]
    fn req_0002_010_body_tenant_must_match_subject_tenant() -> TestResult {
        let out = encode(request_parts(Op::Send, send_body("acme"))?)?;
        let (mut request, _rx) = to_request(out);
        request.subject = Subject::parse("a2a.v1.other.rpc.worker.send").must()?;
        assert_eq!(
            rejected(&Config::default(), Some(NOW), request)?,
            WireError::TenantMismatch
        );
        Ok(())
    }

    #[test]
    fn req_0002_010_empty_body_tenant_is_accepted() -> TestResult {
        let out = encode(request_parts(Op::Send, send_body("acme"))?)?;
        let payload = send_body("").encode(false).ok_or("encode")?;
        let (request, _rx) = to_request(Outgoing {
            subject: out.subject,
            headers: out.headers,
            payload: Bytes::from(payload),
        });
        let inbound = accept(&Config::default(), NOW, request)?;
        assert_eq!(inbound.body.tenant(), "");
        Ok(())
    }

    #[test]
    fn req_0002_013_authorization_header_is_malformed() -> TestResult {
        let mut out = encode(request_parts(Op::Send, send_body("acme"))?)?;
        out.headers
            .insert(hdr::AUTHORIZATION, "Bearer token")
            .must()?;
        let (request, _rx) = to_request(out);
        assert_eq!(
            rejected(&Config::default(), Some(NOW), request)?,
            WireError::MalformedMessage
        );
        Ok(())
    }

    #[test]
    fn req_0002_014_origin_is_honored_only_from_gateway() -> TestResult {
        let mut parts = request_parts(Op::Send, send_body("acme"))?;
        parts.origin = Some("203.0.113.7".to_owned());
        let (request, _rx) = to_request(encode(parts)?);
        let inbound = accept(&Config::default(), NOW, request)?;
        assert_eq!(inbound.head.origin, "planner");

        let mut parts = request_parts(Op::Send, send_body("acme"))?;
        parts.sender = AgentId::gateway();
        parts.origin = Some("203.0.113.7".to_owned());
        let (request, _rx) = to_request(encode(parts)?);
        let inbound = accept(&Config::default(), NOW, request)?;
        assert_eq!(inbound.head.origin, "203.0.113.7");
        assert_eq!(inbound.head.sender, AgentId::gateway());
        Ok(())
    }

    fn planner() -> Result<Principal, String> {
        Principal::named("planner").must()
    }

    fn owner() -> Result<Principal, String> {
        Principal::named("worker").must()
    }

    fn as_principal(request: BusRequest, principal: Option<Principal>) -> BusRequest {
        BusRequest {
            principal,
            ..request
        }
    }

    #[test]
    fn req_0002_011_spoofed_sender_header_is_refused() -> TestResult {
        let (request, _rx) = to_request(encode(request_parts(Op::Send, send_body("acme"))?)?);
        let spoofed = as_principal(request, Some(Principal::named("mallory").must()?));
        assert_eq!(
            rejected(&Config::default(), Some(NOW), spoofed)?,
            WireError::PermissionDenied
        );
        Ok(())
    }

    #[test]
    fn req_0002_011_missing_or_anonymous_principal_is_refused() -> TestResult {
        let (request, _rx) = to_request(encode(request_parts(Op::Send, send_body("acme"))?)?);
        let missing = as_principal(request, None);
        assert_eq!(
            rejected(&Config::default(), Some(NOW), missing)?,
            WireError::PermissionDenied
        );

        let (request, _rx) = to_request(encode(request_parts(Op::Send, send_body("acme"))?)?);
        let anonymous = as_principal(request, Some(Principal::anonymous()));
        assert_eq!(
            rejected(&Config::default(), Some(NOW), anonymous)?,
            WireError::PermissionDenied
        );
        Ok(())
    }

    #[test]
    fn req_0002_061_after_seq_is_surfaced_and_validated() -> TestResult {
        let subscribe = || {
            RequestBody::SubscribeToTask(pb::SubscribeToTaskRequest {
                tenant: "acme".to_owned(),
                ..Default::default()
            })
        };
        let (request, _rx) = to_request(encode(request_parts(Op::Subscribe, subscribe())?)?);
        let inbound = accept(&Config::default(), NOW, request)?;
        assert_eq!(inbound.head.after_seq, None);

        let mut out = encode(request_parts(Op::Subscribe, subscribe())?)?;
        out.headers.insert(hdr::AFTER_SEQ, "0").must()?;
        let (request, _rx) = to_request(out);
        let inbound = accept(&Config::default(), NOW, request)?;
        assert_eq!(inbound.head.after_seq, Some(0));

        let mut out = encode(request_parts(Op::Subscribe, subscribe())?)?;
        out.headers
            .insert(hdr::AFTER_SEQ, "18446744073709551615")
            .must()?;
        let (request, _rx) = to_request(out);
        let inbound = accept(&Config::default(), NOW, request)?;
        assert_eq!(inbound.head.after_seq, Some(u64::MAX));

        for value in ["-1", " 2", "0x2", "18446744073709551616"] {
            let mut out = encode(request_parts(Op::Subscribe, subscribe())?)?;
            out.headers.insert(hdr::AFTER_SEQ, value).must()?;
            let (request, _rx) = to_request(out);
            assert_eq!(
                rejected(&Config::default(), Some(NOW), request)?,
                WireError::MalformedMessage,
                "A2AB-After-Seq {value:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn req_0002_011_matching_principal_is_accepted() -> TestResult {
        let (request, _rx) = to_request(encode(request_parts(Op::Send, send_body("acme"))?)?);
        let request = as_principal(request, Some(planner()?));
        let inbound = accept(&Config::default(), NOW, request)?;
        assert_eq!(inbound.head.sender.as_str(), "planner");
        Ok(())
    }

    #[test]
    fn req_0002_011_gateway_origin_needs_verified_gateway_principal() -> TestResult {
        // A worker writing `gateway` into A2AB-Sender is refused, so its origin is never used.
        let mut parts = request_parts(Op::Send, send_body("acme"))?;
        parts.sender = AgentId::gateway();
        parts.origin = Some("203.0.113.7".to_owned());
        let (request, _rx) = to_request(encode(parts)?);
        let spoofed = as_principal(request, Some(planner()?));
        assert_eq!(
            rejected(&Config::default(), Some(NOW), spoofed)?,
            WireError::PermissionDenied
        );

        // Only the gateway's own principal may set the origin ([0002-014]).
        let mut parts = request_parts(Op::Send, send_body("acme"))?;
        parts.sender = AgentId::gateway();
        parts.origin = Some("203.0.113.7".to_owned());
        let (request, _rx) = to_request(encode(parts)?);
        let genuine = as_principal(request, Some(Principal::named("gateway").must()?));
        let inbound = accept(&Config::default(), NOW, genuine)?;
        assert_eq!(inbound.head.origin, "203.0.113.7");
        Ok(())
    }

    #[test]
    fn req_0002_011_event_principal_must_be_owning_agent() -> TestResult {
        let out = encode_event(
            EventParts {
                tenant: TenantId::parse("acme").must()?,
                agent: AgentId::parse("worker").must()?,
                task: TaskId::generate(),
                msg_id: MessageId::generate(),
                traceparent: None,
                tracestate: None,
                stream_end: false,
                json: false,
                event: pb::StreamResponse::default(),
            },
            MAX_INLINE_BYTES,
        )
        .must()?;
        let config = Config::default();
        assert!(
            receive_event(
                &config,
                Some(&owner()?),
                &out.subject,
                &out.headers,
                &out.payload
            )
            .is_ok()
        );
        let intruder = Principal::named("intruder").must()?;
        assert_eq!(
            receive_event(
                &config,
                Some(&intruder),
                &out.subject,
                &out.headers,
                &out.payload
            )
            .err(),
            Some(WireError::PermissionDenied)
        );
        assert_eq!(
            receive_event(&config, None, &out.subject, &out.headers, &out.payload).err(),
            Some(WireError::PermissionDenied)
        );
        Ok(())
    }

    #[test]
    fn req_0002_033_out_of_range_ttl_is_malformed() -> TestResult {
        let mut out = encode(request_parts(Op::Send, send_body("acme"))?)?;
        out.headers.insert(hdr::TTL_MS, "0").must()?;
        let (request, _rx) = to_request(out);
        assert_eq!(
            rejected(&Config::default(), Some(NOW), request)?,
            WireError::MalformedMessage
        );
        Ok(())
    }

    #[test]
    fn req_0002_034_encoding_rejects_ttl_out_of_range() -> TestResult {
        let mut parts = request_parts(Op::Send, send_body("acme"))?;
        parts.ttl_ms = hdr::MAX_TTL_MS + 1;
        assert!(matches!(
            encode_request(parts, MAX_INLINE_BYTES),
            Err(EncodeError::Ttl)
        ));
        Ok(())
    }

    #[test]
    fn req_0002_071_deadline_is_receipt_time_plus_ttl() -> TestResult {
        let (request, _rx) = to_request(encode(request_parts(Op::Send, send_body("acme"))?)?);
        let inbound = accept(&Config::default(), NOW, request)?;
        assert_eq!(inbound.head.deadline_ms, NOW + TTL);
        Ok(())
    }

    #[test]
    fn req_0002_072_stale_boundary_is_strict_and_future_ts_is_not_stale() -> TestResult {
        let config = Config::default();
        let allowance = TTL + hdr::DEFAULT_SKEW_MS;

        let mut parts = request_parts(Op::Send, send_body("acme"))?;
        parts.ts_ms = NOW - allowance;
        let (request, _rx) = to_request(encode(parts)?);
        assert!(accept(&config, NOW, request).is_ok());

        let mut parts = request_parts(Op::Send, send_body("acme"))?;
        parts.ts_ms = NOW - allowance - 1;
        let (request, _rx) = to_request(encode(parts)?);
        assert_eq!(
            rejected(&config, Some(NOW), request)?,
            WireError::RequestExpired
        );

        let mut parts = request_parts(Op::Send, send_body("acme"))?;
        parts.ts_ms = NOW + 120_000;
        let (request, _rx) = to_request(encode(parts)?);
        assert!(accept(&config, NOW, request).is_ok());
        Ok(())
    }

    #[test]
    fn req_0002_072_unreadable_clock_fails_closed() -> TestResult {
        let (request, _rx) = to_request(encode(request_parts(Op::Send, send_body("acme"))?)?);
        assert_eq!(
            rejected(&Config::default(), None, request)?,
            WireError::RequestExpired
        );
        Ok(())
    }

    #[test]
    fn req_0002_080_missing_or_wrong_version_is_rejected() -> TestResult {
        let out = encode(request_parts(Op::Send, send_body("acme"))?)?;
        let headers = without(&out.headers, hdr::A2A_VERSION);
        let (request, _rx) = to_request(Outgoing {
            subject: out.subject.clone(),
            headers,
            payload: out.payload.clone(),
        });
        assert_eq!(
            rejected(&Config::default(), Some(NOW), request)?,
            WireError::VersionNotSupported
        );

        let mut headers = out.headers;
        headers.insert(hdr::A2A_VERSION, "0.3").must()?;
        let (request, _rx) = to_request(Outgoing {
            subject: out.subject,
            headers,
            payload: out.payload,
        });
        assert_eq!(
            rejected(&Config::default(), Some(NOW), request)?,
            WireError::VersionNotSupported
        );
        Ok(())
    }

    #[test]
    fn req_0002_081_required_extension_must_be_activated() -> TestResult {
        let config = Config {
            required_extensions: vec![BUDGET.to_owned()],
            ..Config::default()
        };
        let (request, _rx) = to_request(encode(request_parts(Op::Send, send_body("acme"))?)?);
        assert_eq!(
            rejected(&config, Some(NOW), request)?,
            WireError::ExtensionSupportRequired
        );

        let mut parts = request_parts(Op::Send, send_body("acme"))?;
        parts.extensions = vec![BUDGET.to_owned()];
        let (request, _rx) = to_request(encode(parts)?);
        let inbound = accept(&config, NOW, request)?;
        assert!(inbound.head.activation.contains(BUDGET));
        Ok(())
    }

    #[test]
    fn req_0002_082_service_params_round_trip_with_lowercased_keys() -> TestResult {
        let mut parts = request_parts(Op::Send, send_body("acme"))?;
        parts.service_params = vec![("Tenant-Hint".to_owned(), "eu-west".to_owned())];
        let (request, _rx) = to_request(encode(parts)?);
        let inbound = accept(&Config::default(), NOW, request)?;
        assert_eq!(
            inbound.head.service_params,
            vec![("tenant-hint".to_owned(), "eu-west".to_owned())]
        );
        Ok(())
    }

    #[test]
    fn req_0002_082_reserved_service_parameter_is_refused() -> TestResult {
        let mut parts = request_parts(Op::Send, send_body("acme"))?;
        parts.service_params = vec![("Version".to_owned(), "0.3".to_owned())];
        assert!(matches!(
            encode_request(parts, MAX_INLINE_BYTES),
            Err(EncodeError::ReservedParam(_))
        ));
        Ok(())
    }

    #[test]
    fn req_0002_040_reply_is_single_and_carries_causation_and_type() -> TestResult {
        let out = encode(request_parts(Op::Send, send_body("acme"))?)?;
        let msg_id = out
            .headers
            .get(hdr::MSG_ID)
            .map(str::to_owned)
            .ok_or("no msg id")?;
        let (request, rx) = to_request(out);
        let inbound = accept(&Config::default(), NOW, request)?;
        let response = pb::SendMessageResponse::default();
        inbound.reply(&response, &ReplyOptions::default()).must()?;
        let reply = sent(&rx)?;
        assert_eq!(
            reply.headers.get(hdr::PAYLOAD_TYPE),
            Some("lf.a2a.v1.SendMessageResponse")
        );
        assert_eq!(reply.headers.get(hdr::CAUSATION_ID), Some(msg_id.as_str()));
        assert!(rx.try_recv().is_err());
        match decode_reply::<pb::SendMessageResponse>(&reply) {
            Ok(decoded) => assert!(decoded == response),
            Err(error) => return Err(format!("{error:?}")),
        }
        Ok(())
    }

    #[test]
    fn req_0002_040_reply_of_wrong_type_is_refused() -> TestResult {
        let (request, _rx) = to_request(encode(request_parts(Op::Send, send_body("acme"))?)?);
        let inbound = accept(&Config::default(), NOW, request)?;
        let wrong = pb::Task::default();
        assert!(matches!(
            inbound.reply(&wrong, &ReplyOptions::default()),
            Err(ReplyError::PayloadType)
        ));
        Ok(())
    }

    #[test]
    fn req_0002_051_last_seq_and_applied_extensions_on_reply() -> TestResult {
        let mut parts = request_parts(Op::Send, send_body("acme"))?;
        parts.extensions = vec![BUDGET.to_owned()];
        let (request, rx) = to_request(encode(parts)?);
        let inbound = accept(&Config::default(), NOW, request)?;
        let options = ReplyOptions {
            last_seq: Some(2),
            applied_extensions: vec![BUDGET.to_owned()],
        };
        inbound
            .reply(&pb::SendMessageResponse::default(), &options)
            .must()?;
        let reply = sent(&rx)?;
        assert_eq!(reply.headers.get(hdr::LAST_SEQ), Some("2"));
        assert_eq!(reply.headers.get(hdr::A2A_EXTENSIONS), Some(BUDGET));
        Ok(())
    }

    #[test]
    fn req_0002_040_failure_reply_carries_status_and_retry_info() -> TestResult {
        let (request, rx) = to_request(encode(request_parts(Op::Send, send_body("acme"))?)?);
        let inbound = accept(&Config::default(), NOW, request)?;
        let extras = StatusExtras {
            retry_after: Some(std::time::Duration::from_secs(5)),
            ..StatusExtras::default()
        };
        inbound.fail(WireError::Overloaded, &extras).must()?;
        let reply = sent(&rx)?;
        match decode_reply::<pb::Task>(&reply) {
            Err(ReplyDecodeError::Failed(status)) => {
                assert_eq!(classify(&status), Some(WireError::Overloaded));
                assert_eq!(
                    retry_after(&status),
                    Some(std::time::Duration::from_secs(5))
                );
            }
            _ => return Err("expected a status reply".to_owned()),
        }
        Ok(())
    }

    #[test]
    fn req_0002_110_inline_limit_is_the_smaller_of_bus_and_spec() {
        assert_eq!(inline_limit(1_024), 1_024);
        assert_eq!(inline_limit(usize::MAX), MAX_INLINE_BYTES);
        assert_eq!(MAX_INLINE_BYTES, 262_144);
    }

    #[test]
    fn req_0002_113_oversize_payload_without_reference_is_refused() -> TestResult {
        let parts = request_parts(Op::Send, send_body("acme"))?;
        assert!(matches!(
            encode_request(parts, 8),
            Err(EncodeError::PayloadTooLarge { limit: 8 })
        ));
        let out = encode(request_parts(Op::Send, send_body("acme"))?)?;
        let (request, _rx) = to_request(out);
        let config = Config {
            max_inline: 8,
            ..Config::default()
        };
        assert_eq!(
            rejected(&config, Some(NOW), request)?,
            WireError::PayloadTooLarge
        );
        Ok(())
    }

    #[test]
    fn req_0002_111_blob_reference_round_trips_in_a_request() -> TestResult {
        let tenant = TenantId::parse("acme").must()?;
        let blob = BlobRef::for_bytes(&tenant, b"abc");
        let url = blob.url();
        assert!(url.starts_with("a2abus-blob://acme/"));
        assert_eq!(url.len(), "a2abus-blob://acme/".len() + 64);
        assert_eq!(BlobRef::parse(&url, &tenant).must()?, blob);

        let body = RequestBody::SendMessage(pb::SendMessageRequest {
            tenant: "acme".to_owned(),
            message: Some(pb::Message {
                parts: vec![pb::Part {
                    content: Some(pb::part::Content::Url(url)),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        });
        let (request, _rx) = to_request(encode(request_parts(Op::Send, body)?)?);
        let inbound = accept(&Config::default(), NOW, request)?;
        assert_eq!(inbound.head.blob_refs, vec![blob]);
        Ok(())
    }

    #[test]
    fn req_0002_112_blob_digest_mismatch_and_cross_tenant_reference_are_rejected() -> TestResult {
        let tenant = TenantId::parse("acme").must()?;
        let blob = BlobRef::for_bytes(&tenant, b"abc");
        assert_eq!(blob.verify(b"abc"), Ok(()));
        assert_eq!(blob.verify(b"abd"), Err(WireError::MalformedMessage));

        let other = TenantId::parse("other").must()?;
        let foreign = BlobRef::for_bytes(&other, b"abc").url();
        assert_eq!(
            BlobRef::parse(&foreign, &tenant),
            Err(WireError::PermissionDenied)
        );

        let upper = format!("a2abus-blob://acme/{}", "A".repeat(64));
        assert_eq!(
            BlobRef::parse(&upper, &tenant),
            Err(WireError::MalformedMessage)
        );
        let short = "a2abus-blob://acme/abcd";
        assert_eq!(
            BlobRef::parse(short, &tenant),
            Err(WireError::MalformedMessage)
        );

        let body = RequestBody::SendMessage(pb::SendMessageRequest {
            tenant: "acme".to_owned(),
            message: Some(pb::Message {
                parts: vec![pb::Part {
                    content: Some(pb::part::Content::Url(foreign)),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        });
        let (request, _rx) = to_request(encode(request_parts(Op::Send, body)?)?);
        assert_eq!(
            rejected(&Config::default(), Some(NOW), request)?,
            WireError::PermissionDenied
        );
        Ok(())
    }

    #[test]
    fn req_0002_041_event_round_trip_and_stream_end_flag() -> TestResult {
        let tenant = TenantId::parse("acme").must()?;
        let agent = AgentId::parse("worker").must()?;
        let task = TaskId::generate();
        let event = pb::StreamResponse::default();
        let out = encode_event(
            EventParts {
                tenant: tenant.clone(),
                agent: agent.clone(),
                task: task.clone(),
                msg_id: MessageId::generate(),
                traceparent: None,
                tracestate: None,
                stream_end: true,
                json: false,
                event: event.clone(),
            },
            MAX_INLINE_BYTES,
        )
        .must()?;
        assert_eq!(out.headers.get(hdr::STREAM_END), Some("true"));
        let decoded = receive_event(
            &Config::default(),
            Some(&owner()?),
            &out.subject,
            &out.headers,
            &out.payload,
        )
        .must()?;
        assert!(decoded.stream_end);
        assert_eq!(decoded.task, task);
        assert_eq!(decoded.tenant, tenant);
        assert_eq!(decoded.agent, agent);
        assert!(decoded.event == event);

        let headers = without(&out.headers, hdr::STREAM_END);
        let decoded = receive_event(
            &Config::default(),
            Some(&owner()?),
            &out.subject,
            &headers,
            &out.payload,
        )
        .must()?;
        assert!(!decoded.stream_end);
        Ok(())
    }

    #[test]
    fn req_0002_041_event_on_foreign_subject_or_wrong_type_is_malformed() -> TestResult {
        let out = encode_event(
            EventParts {
                tenant: TenantId::parse("acme").must()?,
                agent: AgentId::parse("worker").must()?,
                task: TaskId::generate(),
                msg_id: MessageId::generate(),
                traceparent: None,
                tracestate: None,
                stream_end: false,
                json: false,
                event: pb::StreamResponse::default(),
            },
            MAX_INLINE_BYTES,
        )
        .must()?;
        let rpc = Subject::parse("a2a.v1.acme.rpc.worker.send").must()?;
        assert_eq!(
            receive_event(
                &Config::default(),
                Some(&owner()?),
                &rpc,
                &out.headers,
                &out.payload
            )
            .err(),
            Some(WireError::MalformedMessage)
        );
        let headers = {
            let mut headers = without(&out.headers, hdr::PAYLOAD_TYPE);
            headers.insert(hdr::PAYLOAD_TYPE, "lf.a2a.v1.Task").must()?;
            headers
        };
        assert_eq!(
            receive_event(
                &Config::default(),
                Some(&owner()?),
                &out.subject,
                &headers,
                &out.payload
            )
            .err(),
            Some(WireError::MalformedMessage)
        );
        Ok(())
    }

    #[test]
    fn req_0002_020_request_body_types_match_their_operation() {
        let pairs = [
            (RequestBody::SendMessage(Default::default()), Op::Send),
            (RequestBody::SendMessage(Default::default()), Op::Stream),
            (RequestBody::GetTask(Default::default()), Op::Get),
            (RequestBody::ListTasks(Default::default()), Op::List),
            (RequestBody::CancelTask(Default::default()), Op::Cancel),
            (
                RequestBody::SubscribeToTask(Default::default()),
                Op::Subscribe,
            ),
            (RequestBody::PushCreate(Default::default()), Op::PushCreate),
            (RequestBody::PushGet(Default::default()), Op::PushGet),
            (RequestBody::PushList(Default::default()), Op::PushList),
            (RequestBody::PushDelete(Default::default()), Op::PushDelete),
            (
                RequestBody::GetExtendedAgentCard(Default::default()),
                Op::CardExt,
            ),
        ];
        for (body, op) in pairs {
            assert!(body.matches_op(op), "{body:?} does not match {op:?}");
        }
    }
}
