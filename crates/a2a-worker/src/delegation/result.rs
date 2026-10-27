//! Outcome mapping for remote children (RFC-0015 0015-032 to 0015-036, 0015-070):
//! the result a delegating task's call receives, framed and bounded.

use a2a_mcp::ipc::{
    ArtifactSummary, IpcError, MAX_RESULT_TEXT_BYTES, Provenance, TaskResult, reason,
};
use a2a_security::frame::frame_untrusted;
use a2a_types::proto::a2a::{self as pb, TaskState};
use a2a_types::proto::a2abus::TrustClass;

use super::Child;
use crate::sanitize::redact_outbound;

/// The outcome of a settled child: its result, or the refusal a call receives.
pub(crate) fn outcome(
    local_id: &str,
    child: &Child,
    task: &pb::Task,
    state: TaskState,
    code: Option<&str>,
) -> Result<TaskResult, IpcError> {
    match state {
        TaskState::Completed => completed(local_id, child, task),
        TaskState::Canceled if child.cancel_requested => Ok(TaskResult {
            state: state_name(state).to_owned(),
            ..snapshot(local_id, child)
        }),
        _ => Err(IpcError::new(code.unwrap_or(reason::CHILD_FAILED))
            .with_meta("taskId", local_id)
            .with_meta("state", state_name(state))),
    }
}

/// The current view of a child that has not settled.
pub(crate) fn snapshot(local_id: &str, child: &Child) -> TaskResult {
    TaskResult {
        task_id: local_id.to_owned(),
        context_id: child.context_id.clone(),
        state: state_name(child.state).to_owned(),
        text: String::new(),
        artifacts: Vec::new(),
        truncated: false,
        omitted_bytes: 0,
        provenance: provenance(child),
    }
}

/// Wire name of a task state as the tools report it.
pub(crate) fn state_name(state: TaskState) -> &'static str {
    match state {
        TaskState::Submitted => "SUBMITTED",
        TaskState::Working => "WORKING",
        TaskState::InputRequired => "INPUT_REQUIRED",
        TaskState::AuthRequired => "AUTH_REQUIRED",
        TaskState::Completed => "COMPLETED",
        TaskState::Failed => "FAILED",
        TaskState::Canceled => "CANCELED",
        TaskState::Rejected => "REJECTED",
        TaskState::Unspecified => "UNSPECIFIED",
    }
}

/// Frames untrusted tool output for the caller; a framing failure answers `UNAVAILABLE`.
pub(crate) fn frame(text: &str, origin: &str) -> Result<String, IpcError> {
    frame_untrusted(text, TrustClass::ToolOutput, origin)
        .map_err(|_| IpcError::new(reason::UNAVAILABLE))
}

fn completed(local_id: &str, child: &Child, task: &pb::Task) -> Result<TaskResult, IpcError> {
    let raw = task
        .status
        .as_ref()
        .and_then(|s| s.message.as_ref())
        .map(message_text)
        .unwrap_or_default();
    if raw
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r'))
    {
        return Err(IpcError::new(reason::MALFORMED_MESSAGE)
            .with_meta("taskId", local_id)
            .with_meta("state", state_name(TaskState::Completed)));
    }
    let redacted = redact_outbound(&raw).map_err(|_| IpcError::new(reason::UNAVAILABLE))?;
    let origin = child.target.as_str();
    let overhead = frame("", origin)?.len();
    let (kept, omitted) = truncate_utf8(&redacted, MAX_RESULT_TEXT_BYTES.saturating_sub(overhead));
    let text = frame(kept, origin)?;
    let artifacts = task
        .artifacts
        .iter()
        .map(|artifact| ArtifactSummary {
            artifact_id: artifact.artifact_id.clone(),
            name: artifact.name.clone(),
            media_type: artifact
                .parts
                .first()
                .map(|part| part.media_type.clone())
                .unwrap_or_default(),
            bytes: artifact
                .parts
                .iter()
                .fold(0u64, |sum, part| sum.saturating_add(part_bytes(part))),
            blob_ref: None,
        })
        .collect();
    Ok(TaskResult {
        task_id: local_id.to_owned(),
        context_id: child.context_id.clone(),
        state: state_name(TaskState::Completed).to_owned(),
        text,
        artifacts,
        truncated: omitted > 0,
        omitted_bytes: u64::try_from(omitted).unwrap_or(u64::MAX),
        provenance: provenance(child),
    })
}

fn provenance(child: &Child) -> Provenance {
    Provenance {
        source_agent: child.target.as_str().to_owned(),
        task_id: child.remote.as_str().to_owned(),
        delegating_task_id: Some(child.parent.clone()),
        chain_length: child.chain_length,
    }
}

fn message_text(message: &pb::Message) -> String {
    message
        .parts
        .iter()
        .filter_map(|part| match &part.content {
            Some(pb::part::Content::Text(text)) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn part_bytes(part: &pb::Part) -> u64 {
    let len = match &part.content {
        Some(pb::part::Content::Text(text)) => text.len(),
        Some(pb::part::Content::Raw(raw)) => raw.len(),
        Some(pb::part::Content::Url(url)) => url.len(),
        _ => 0,
    };
    u64::try_from(len).unwrap_or(u64::MAX)
}

/// Splits `text` at the largest char boundary within `max` bytes.
/// Returns the kept prefix and the number of omitted bytes.
fn truncate_utf8(text: &str, max: usize) -> (&str, usize) {
    if text.len() <= max {
        return (text, 0);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text.get(..end).unwrap_or(""), text.len() - end)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// req_0015_034: truncation never splits a UTF-8 sequence and reports the omitted bytes.
    #[test]
    fn req_0015_034_truncation_keeps_char_boundaries() {
        let text = "aé"; // 'é' is two bytes, at offsets 1..3
        assert_eq!(truncate_utf8(text, 2), ("a", 2));
        assert_eq!(truncate_utf8(text, 3), ("aé", 0));
        assert_eq!(truncate_utf8(text, 0), ("", 3));
    }
}
