//! Decoding of task events and their per-task sequence numbers (RFC-0002 0002-043, L8).

use a2a_core::envelope::{self, Config};
use a2a_core::subject::{self, Family};
use a2a_types::ids::{MessageId, TaskId};
use a2a_types::proto::a2a as pb;
use a2a_types::vocab::ext::SEQUENCE;
use bus_api::headers::Headers;
use bus_api::name::{Principal, Subject};
use bytes::Bytes;
use pbjson_types::{Struct, value::Kind};

use crate::error::ClientError;

/// Largest integer a JSON number carries exactly.
const MAX_EXACT: f64 = 9_007_199_254_740_992.0;

/// A decoded task event.
#[derive(Debug, Clone)]
pub struct Event {
    /// Task the event belongs to.
    pub task: TaskId,
    /// Message identifier of the event (RFC-0002 0002-031).
    pub msg_id: MessageId,
    /// Per-task sequence number (metadata `urn:a2abus:ext:sequence:1`).
    pub seq: u64,
    /// `A2AB-Stream-End: true` on the first terminal or interrupted event (RFC-0002 0002-044).
    pub stream_end: bool,
    /// The event content.
    pub response: pb::StreamResponse,
}

/// Decodes an event delivered on `subject`. `principal` is the publisher the bus authenticated
/// (RFC-0002 0002-006, 0002-011); it is never derived from the subject.
pub(crate) fn decode(
    config: &Config,
    principal: Option<&Principal>,
    subject: &Subject,
    headers: &Headers,
    payload: &Bytes,
) -> Result<Event, ClientError> {
    let address =
        subject::parse(subject.as_str()).map_err(|_| ClientError::InvalidEvent("subject"))?;
    let Family::Event { task, .. } = address.family else {
        return Err(ClientError::InvalidEvent("not an event subject"));
    };
    let inbound = envelope::receive_event(config, principal, subject, headers, payload)
        .map_err(|w| ClientError::Wire(w, None))?;
    let seq = sequence_of_response(&inbound.event)
        .ok_or(ClientError::InvalidEvent("missing task sequence"))?;
    if inbound.task != task {
        return Err(ClientError::InvalidEvent("task mismatch"));
    }
    Ok(Event {
        task,
        msg_id: inbound.msg_id,
        seq,
        stream_end: inbound.stream_end,
        response: inbound.event,
    })
}

/// Reads the per-task sequence from an event's metadata.
fn sequence_of_response(response: &pb::StreamResponse) -> Option<u64> {
    let metadata = match &response.payload {
        Some(pb::stream_response::Payload::StatusUpdate(update)) => update.metadata.as_ref(),
        Some(pb::stream_response::Payload::ArtifactUpdate(update)) => update.metadata.as_ref(),
        _ => None,
    }?;
    sequence_of(metadata)
}

/// Reads `{"seq": n}` from the sequence extension of `metadata` (RFC-0002 0002-051).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(crate) fn sequence_of(metadata: &Struct) -> Option<u64> {
    let Kind::StructValue(ext) = metadata.fields.get(SEQUENCE)?.kind.as_ref()? else {
        return None;
    };
    let Kind::NumberValue(n) = ext.fields.get("seq")?.kind.as_ref()? else {
        return None;
    };
    let n = *n;
    if !n.is_finite() || n.fract() != 0.0 || !(0.0..=MAX_EXACT).contains(&n) {
        return None;
    }
    // The range and fraction checks above make this conversion exact.
    Some(n as u64)
}
