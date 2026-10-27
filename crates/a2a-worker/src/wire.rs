//! Mapping between task-engine values, backend outcomes and wire messages.

use std::collections::HashMap;

use a2a_core::task::{Event, EventBody, Failure, StopReason, TaskView};
use a2a_types::failure::FailureCode;
use a2a_types::ids::MessageId;
use a2a_types::proto::a2a as pb;
use a2a_types::proto::a2abus::{ApprovalRequest, BudgetUsage};
use a2a_types::vocab::FAILURE_KEY;
use pbjson_types::{Struct, Timestamp, Value, value::Kind};

use crate::backend::{Outcome, ReasonToken};

/// Converts Unix milliseconds to a protobuf timestamp.
#[must_use]
pub fn timestamp(ms: u64) -> Timestamp {
    Timestamp {
        seconds: i64::try_from(ms / 1000).unwrap_or(i64::MAX),
        nanos: i32::try_from((ms % 1000) * 1_000_000).unwrap_or(0),
    }
}

fn string_value(text: &str) -> Value {
    Value {
        kind: Some(Kind::StringValue(text.to_owned())),
    }
}

/// Parses a metadata `Struct` into a proto-shaped value. `Struct` numbers are `f64`; whole
/// numbers become integers so the u64 fields of a proto message parse from the JSON form.
pub(crate) fn from_struct<T: serde::de::DeserializeOwned>(data: &Struct) -> Option<T> {
    let mut json = serde_json::to_value(data).ok()?;
    integral_numbers(&mut json);
    serde_json::from_value(json).ok()
}

fn integral_numbers(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Number(number) if number.is_f64() => {
            if let Some(whole) = number.as_f64().filter(|f| f.fract() == 0.0 && *f >= 0.0) {
                *value = serde_json::Value::from(whole as u64);
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(integral_numbers),
        serde_json::Value::Object(fields) => fields.values_mut().for_each(integral_numbers),
        _ => {}
    }
}

/// Builds the failure metadata object `{"code": .., "detail": ..}` under `a2abus.failure`
/// (RFC-0005 failure table).
#[must_use]
pub fn failure_metadata(code: &str, detail: Option<&str>) -> Struct {
    let mut inner = HashMap::new();
    inner.insert("code".to_owned(), string_value(code));
    if let Some(detail) = detail {
        // RFC-0005 [0005-049]: `detail` is an object, so the text sits under `message`.
        let fields = HashMap::from([("message".to_owned(), string_value(detail))]);
        inner.insert(
            "detail".to_owned(),
            Value {
                kind: Some(Kind::StructValue(Struct { fields })),
            },
        );
    }
    let mut top = HashMap::new();
    top.insert(
        FAILURE_KEY.to_owned(),
        Value {
            kind: Some(Kind::StructValue(Struct { fields: inner })),
        },
    );
    Struct { fields: top }
}

/// The approval request as the value of the approval metadata entry (RFC-0006 0006-006).
///
/// # Errors
/// Fails when the request does not convert to a JSON object.
pub fn approval_metadata(request: &ApprovalRequest) -> Result<Struct, serde_json::Error> {
    serde_json::from_value(serde_json::to_value(request)?)
}

/// Status message carrying the full approval request in `metadata["urn:a2abus:ext:approval:1"]`
/// (RFC-0006 0006-007).
fn approval_message(task_id: &str, context_id: &str, body: Struct) -> pb::Message {
    let mut top = HashMap::new();
    top.insert(
        a2a_types::vocab::ext::APPROVAL.to_owned(),
        Value {
            kind: Some(Kind::StructValue(body)),
        },
    );
    pb::Message {
        message_id: MessageId::generate().as_str().to_owned(),
        context_id: context_id.to_owned(),
        task_id: task_id.to_owned(),
        role: pb::Role::Agent as i32,
        metadata: Some(Struct { fields: top }),
        ..Default::default()
    }
}

/// Status message carrying the failure code as text and as metadata (RFC-0005).
fn failure_message(task_id: &str, context_id: &str, failure: &Failure) -> Option<pb::Message> {
    let code = failure.code()?.as_str();
    Some(pb::Message {
        message_id: MessageId::generate().as_str().to_owned(),
        context_id: context_id.to_owned(),
        task_id: task_id.to_owned(),
        role: pb::Role::Agent as i32,
        parts: vec![text_part(code.to_owned())],
        metadata: Some(failure_metadata(code, failure.detail.as_deref())),
        ..Default::default()
    })
}

fn text_part(text: String) -> pb::Part {
    pb::Part {
        content: Some(pb::part::Content::Text(text)),
        ..Default::default()
    }
}

fn artifact_part(payload: Vec<u8>) -> pb::Part {
    match String::from_utf8(payload) {
        Ok(text) => text_part(text),
        Err(err) => pb::Part {
            content: Some(pb::part::Content::Raw(err.into_bytes().into())),
            ..Default::default()
        },
    }
}

/// Puts an open approval request on a task snapshot as its status message, in the shape of the
/// approval event (0006-007), so that GetTask and ListTasks readers see what is awaited.
pub fn attach_approval(task: &mut pb::Task, body: Struct) {
    if let Some(status) = task.status.as_mut() {
        status.message = Some(approval_message(&task.id, &task.context_id, body));
    }
}

/// Builds the protobuf `Task` for a GetTask, ListTasks or CancelTask answer.
#[must_use]
pub fn task_proto(view: &TaskView) -> pb::Task {
    let message = view
        .failure
        .as_ref()
        .and_then(|failure| failure_message(&view.task_id, &view.context_id, failure));
    let metadata = Some(event_metadata(
        view.seq,
        view.state.is_terminal().then_some(&view.usage),
    ));
    let artifacts = view
        .artifacts
        .iter()
        .map(|artifact| pb::Artifact {
            artifact_id: artifact.artifact_id.clone(),
            name: artifact.name.clone(),
            parts: vec![artifact_part(artifact.payload.clone())],
            ..Default::default()
        })
        .collect();
    pb::Task {
        id: view.task_id.clone(),
        context_id: view.context_id.clone(),
        status: Some(pb::TaskStatus {
            state: view.state as i32,
            message,
            timestamp: Some(timestamp(view.updated_ms)),
        }),
        metadata,
        artifacts,
        ..Default::default()
    }
}

/// The child's final `BudgetUsage` as the budget metadata entry of a terminal task
/// (RFC-0006 0006-041). `None` only when the message fails to convert to a JSON object.
#[must_use]
pub fn usage_metadata(usage: &BudgetUsage) -> Option<Struct> {
    let json = serde_json::to_value(usage).ok()?;
    let body: Struct = serde_json::from_value(json).ok()?;
    Some(Struct {
        fields: HashMap::from([(
            a2a_types::vocab::ext::BUDGET.to_owned(),
            Value {
                kind: Some(Kind::StructValue(body)),
            },
        )]),
    })
}

/// Event metadata: the task sequence (RFC-0002 0002-042) and, on a terminal Status event, the
/// hop's usage (RFC-0006 0006-041).
#[allow(clippy::cast_precision_loss)]
#[must_use]
pub fn event_metadata(seq: u64, usage: Option<&BudgetUsage>) -> Struct {
    let mut fields = usage
        .and_then(usage_metadata)
        .map(|s| s.fields)
        .unwrap_or_default();
    let body = Struct {
        fields: HashMap::from([(
            "seq".to_owned(),
            Value {
                kind: Some(Kind::NumberValue(seq as f64)),
            },
        )]),
    };
    fields.insert(
        a2a_types::vocab::ext::SEQUENCE.to_owned(),
        Value {
            kind: Some(Kind::StructValue(body)),
        },
    );
    Struct { fields }
}

/// Builds the `StreamResponse` for one committed event.
///
/// `first_chunk` is true when the artifact has not been published before by this worker run;
/// it decides the A2A `append` flag of artifact updates (RFC-0005 0005-022).
///
/// `approval_body` is the full approval request for a Status event that carries an approval id,
/// built with [`approval_metadata`] (RFC-0006 0006-006). The caller MUST supply it for those
/// events; it is ignored otherwise.
///
/// `usage` is the hop's `BudgetUsage` for a terminal Status event; it is carried in that event's
/// metadata (RFC-0006 0006-041). Callers pass `None` for every other event.
#[must_use]
pub fn stream_response(
    event: &Event,
    context_id: &str,
    first_chunk: bool,
    approval_body: Option<Struct>,
    usage: Option<&BudgetUsage>,
) -> pb::StreamResponse {
    match &event.body {
        EventBody::Status {
            state,
            failure,
            approval: _,
        } => {
            let message = match approval_body {
                Some(body) => Some(approval_message(&event.task_id, context_id, body)),
                None => failure
                    .as_ref()
                    .and_then(|failure| failure_message(&event.task_id, context_id, failure)),
            };
            pb::StreamResponse {
                payload: Some(pb::stream_response::Payload::StatusUpdate(
                    pb::TaskStatusUpdateEvent {
                        task_id: event.task_id.clone(),
                        context_id: context_id.to_owned(),
                        status: Some(pb::TaskStatus {
                            state: *state,
                            message,
                            timestamp: Some(timestamp(event.at_ms)),
                        }),
                        metadata: Some(event_metadata(event.seq, usage)),
                    },
                )),
            }
        }
        EventBody::Artifact {
            artifact_id,
            last_chunk,
            payload,
            ..
        } => pb::StreamResponse {
            payload: Some(pb::stream_response::Payload::ArtifactUpdate(
                pb::TaskArtifactUpdateEvent {
                    task_id: event.task_id.clone(),
                    context_id: context_id.to_owned(),
                    artifact: Some(pb::Artifact {
                        artifact_id: artifact_id.clone(),
                        parts: vec![artifact_part(payload.clone())],
                        ..Default::default()
                    }),
                    append: !first_chunk,
                    last_chunk: *last_chunk,
                    metadata: Some(event_metadata(event.seq, None)),
                },
            )),
        },
    }
}

/// Maps a backend terminal outcome to the task engine's stop reason (RFC-0009 4.11).
#[must_use]
pub fn stop_reason(outcome: &Outcome) -> StopReason {
    match outcome {
        Outcome::Completed => StopReason::EndTurn,
        Outcome::Canceled => StopReason::Cancelled,
        Outcome::Refused { reason, .. } => match reason {
            ReasonToken::AgentRefused => StopReason::Refusal,
            other => registry_or_unknown(*other),
        },
        Outcome::Failed { reason, .. } => match reason {
            ReasonToken::BackendCrashed => StopReason::Crashed,
            ReasonToken::AgentLimit => StopReason::MaxTokens,
            ReasonToken::AgentRefused => StopReason::Refusal,
            other => registry_or_unknown(*other),
        },
    }
}

/// The failure detail a terminal outcome carries into the task record (0005-031), if any.
#[must_use]
pub fn outcome_detail(outcome: &Outcome) -> Option<serde_json::Value> {
    match outcome {
        Outcome::Failed { detail, .. } if !detail.is_empty() => {
            Some(serde_json::Value::String(detail.clone()))
        }
        _ => None,
    }
}

fn registry_or_unknown(reason: ReasonToken) -> StopReason {
    match reason {
        ReasonToken::BackendUnavailable => StopReason::Backend(FailureCode::BackendUnavailable),
        ReasonToken::BackendTimeout => StopReason::Backend(FailureCode::BackendTimeout),
        ReasonToken::BackendLostTask => StopReason::Backend(FailureCode::BackendLostTask),
        ReasonToken::BackendError => StopReason::Backend(FailureCode::BackendError),
        ReasonToken::BackendCrashed => StopReason::Crashed,
        ReasonToken::AgentLimit => StopReason::MaxTokens,
        ReasonToken::AgentRefused => StopReason::Refusal,
        ReasonToken::RateLimited => StopReason::Unknown,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// Guards 0005-005/0005-007: timestamps split seconds and nanoseconds without loss.
    #[test]
    fn req_0005_timestamp_splits_milliseconds() {
        let ts = timestamp(1_700_000_000_123);
        assert_eq!(ts.seconds, 1_700_000_000);
        assert_eq!(ts.nanos, 123_000_000);
    }

    /// Guards 0005-012: refused and failed reasons map onto the registry codes.
    #[test]
    fn req_0005_012_failed_reasons_map_to_stop_reasons() {
        let refused = Outcome::Refused {
            reason: ReasonToken::AgentRefused,
            health_class: None,
        };
        assert_eq!(stop_reason(&refused), StopReason::Refusal);
        let limit = Outcome::Failed {
            reason: ReasonToken::AgentLimit,
            health_class: None,
            detail: String::new(),
        };
        assert_eq!(stop_reason(&limit), StopReason::MaxTokens);
        let timeout = Outcome::Failed {
            reason: ReasonToken::BackendTimeout,
            health_class: None,
            detail: String::new(),
        };
        assert_eq!(
            stop_reason(&timeout),
            StopReason::Backend(FailureCode::BackendTimeout)
        );
        assert_eq!(stop_reason(&Outcome::Completed), StopReason::EndTurn);
        assert_eq!(stop_reason(&Outcome::Canceled), StopReason::Cancelled);
    }

    /// Guards 0005-022: the first artifact chunk is not an append; later chunks are.
    #[test]
    fn req_0005_022_artifact_append_follows_first_chunk() {
        let event = Event {
            task_id: "t".to_owned(),
            seq: 2,
            msg_id: "m".to_owned(),
            at_ms: 0,
            body: EventBody::Artifact {
                artifact_id: "a".to_owned(),
                append: true,
                last_chunk: false,
                payload: b"hi".to_vec(),
            },
        };
        let first = stream_response(&event, "c", true, None, None);
        let later = stream_response(&event, "c", false, None, None);
        let append_of = |resp: pb::StreamResponse| match resp.payload {
            Some(pb::stream_response::Payload::ArtifactUpdate(update)) => update.append,
            _ => panic!("expected artifact update"),
        };
        assert!(!append_of(first));
        assert!(append_of(later));
    }

    /// Guards 0005-005: a failure is carried as text and under `a2abus.failure` metadata.
    #[test]
    fn req_0005_failure_metadata_carries_code_and_detail() {
        let meta = failure_metadata("BACKEND_CRASHED", Some("exit 1"));
        let Some(Kind::StructValue(inner)) = meta.fields[FAILURE_KEY].kind.clone() else {
            panic!("failure object missing");
        };
        assert!(matches!(
            &inner.fields["code"].kind,
            Some(Kind::StringValue(s)) if s == "BACKEND_CRASHED"
        ));
        let Some(Kind::StructValue(detail)) = &inner.fields["detail"].kind else {
            panic!("detail must be an object (0005-049)");
        };
        assert!(matches!(
            &detail.fields["message"].kind,
            Some(Kind::StringValue(s)) if s == "exit 1"
        ));
    }

    /// Guards 0006-006: INPUT_REQUIRED carries the full approval request under the approval key.
    #[test]
    fn req_0006_006_input_required_metadata_carries_full_request() {
        let request = ApprovalRequest {
            request_id: "req-1".to_owned(),
            task_id: "t".to_owned(),
            tool: "shell".to_owned(),
            summary: "run ls".to_owned(),
            ..Default::default()
        };
        let body = approval_metadata(&request).expect("request converts to a struct");
        let event = Event {
            task_id: "t".to_owned(),
            seq: 3,
            msg_id: "m".to_owned(),
            at_ms: 0,
            body: EventBody::Status {
                state: pb::TaskState::InputRequired as i32,
                failure: None,
                approval: Some("req-1".to_owned()),
            },
        };
        let response = stream_response(&event, "c", false, Some(body), None);
        let Some(pb::stream_response::Payload::StatusUpdate(update)) = response.payload else {
            panic!("expected status update");
        };
        let metadata = update
            .status
            .and_then(|status| status.message)
            .and_then(|message| message.metadata)
            .expect("approval metadata on status message");
        let Some(Kind::StructValue(inner)) = metadata.fields[a2a_types::vocab::ext::APPROVAL]
            .kind
            .clone()
        else {
            panic!("approval object missing");
        };
        assert!(matches!(
            &inner.fields["requestId"].kind,
            Some(Kind::StringValue(s)) if s == "req-1"
        ));
        assert!(matches!(
            &inner.fields["summary"].kind,
            Some(Kind::StringValue(s)) if s == "run ls"
        ));
    }
}
