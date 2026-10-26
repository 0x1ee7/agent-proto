//! proto3-JSON round trips of the RFC-0005 worked examples (RFC-0005 sec 4.10).
//!
//! `pbjson-types` 0.9 emits timestamps as RFC 3339 with a `+00:00` offset and no zero
//! fractional digits. The canonical proto3 JSON form uses `Z` and 0/3/6/9 digits. The tests
//! therefore compare timestamps as instants and compare everything else as exact JSON.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use a2a_types::extension::{Activation, read_activated};
use a2a_types::failure::{FailureCode, FailureReason};
use a2a_types::lifecycle::{LifecycleError, check_emittable, check_status};
use a2a_types::proto::a2a::{Task, TaskState, TaskStatusUpdateEvent};
use a2a_types::vocab::ext;
use pbjson_types::Timestamp;
use serde_json::{Value, json};

/// Removes `status.timestamp` from both JSON documents and checks that the instants agree.
/// Everything else must match exactly.
fn assert_same_except_status_timestamp(emitted: &Value, wire: &Value) {
    let mut emitted = emitted.clone();
    let mut wire = wire.clone();
    let emitted_ts = emitted["status"]
        .as_object_mut()
        .unwrap()
        .remove("timestamp");
    let wire_ts = wire["status"].as_object_mut().unwrap().remove("timestamp");
    assert_eq!(emitted, wire);
    let emitted_ts: Timestamp = serde_json::from_value(emitted_ts.unwrap()).unwrap();
    let wire_ts: Timestamp = serde_json::from_value(wire_ts.unwrap()).unwrap();
    assert_eq!(emitted_ts, wire_ts);
}

// [0005-049] RFC-0005 example 3: a REJECTED task with RATE_LIMITED and a sequence counter.
#[test]
fn req_0005_049_example3_rejected_task_round_trips() {
    let wire = json!({
        "id": "0195f3a2-6c30-7e21-9b4d-2a7c8e9f0a13",
        "contextId": "0195f3a2-6c60-7d3c-8f10-4b5a6c7d8e9f",
        "status": {
            "state": "TASK_STATE_REJECTED",
            "timestamp": "2025-06-01T10:07:45.002Z",
            "message": {
                "messageId": "7e8f9a0b-1c2d-4e3f-8a4b-5c6d7e8f9a0b",
                "role": "ROLE_AGENT",
                "parts": [{"text": "task rejected: rate limit exceeded"}],
                "metadata": {"a2abus.failure": {"code": "RATE_LIMITED"}}
            }
        },
        "metadata": {"urn:a2abus:ext:sequence:1": {"seq": "1"}}
    });
    let task: Task = serde_json::from_value(wire.clone()).unwrap();
    assert_same_except_status_timestamp(&serde_json::to_value(&task).unwrap(), &wire);

    let status = task.status.as_ref().unwrap();
    assert_eq!(
        TaskState::from_wire(status.state).unwrap(),
        TaskState::Rejected
    );
    // Creation may end REJECTED with RATE_LIMITED (Table 4.2).
    check_status(None, status).unwrap();
    let reason = FailureReason::from_message(status.message.as_ref().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(reason.code, FailureCode::RateLimited);

    let activation = Activation::parse(ext::SEQUENCE).unwrap();
    let seq = read_activated(task.metadata.as_ref().unwrap(), ext::SEQUENCE, &activation)
        .unwrap()
        .unwrap();
    assert_eq!(serde_json::to_value(seq).unwrap(), json!({"seq": "1"}));
}

// [0005-049] RFC-0005 example 4: a FAILED status event with DEADLINE_EXCEEDED and seq 9.
#[test]
fn req_0005_049_example4_failed_event_round_trips() {
    let wire = json!({
        "taskId": "0195f3a2-6c20-7e21-9b4d-2a7c8e9f0a12",
        "contextId": "0195f3a2-6c50-7d3c-8f10-4b5a6c7d8e9f",
        "status": {
            "state": "TASK_STATE_FAILED",
            "timestamp": "2025-06-01T10:40:00.000Z",
            "message": {
                "messageId": "9a0b1c2d-3e4f-4a5b-8c6d-7e8f9a0b1c2d",
                "role": "ROLE_AGENT",
                "parts": [{"text": "task failed: deadline exceeded"}],
                "metadata": {"a2abus.failure": {"code": "DEADLINE_EXCEEDED"}}
            }
        },
        "metadata": {"urn:a2abus:ext:sequence:1": {"seq": "9"}}
    });
    let event: TaskStatusUpdateEvent = serde_json::from_value(wire.clone()).unwrap();
    assert_same_except_status_timestamp(&serde_json::to_value(&event).unwrap(), &wire);

    let status = event.status.as_ref().unwrap();
    assert_eq!(
        TaskState::from_wire(status.state).unwrap(),
        TaskState::Failed
    );
    // WORKING -> FAILED with DEADLINE_EXCEEDED is an admitted edge.
    check_status(Some(TaskState::Working), status).unwrap();
    let reason = FailureReason::from_message(status.message.as_ref().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(reason.code, FailureCode::DeadlineExceeded);
    assert_eq!(reason.text(), "task failed: deadline exceeded");
}

// [0005-005] a wire status naming TASK_STATE_UNSPECIFIED is refused on read.
#[test]
fn req_0005_005_unspecified_wire_state_is_refused() {
    let wire = json!({
        "taskId": "0195f3a2-6c20-7e21-9b4d-2a7c8e9f0a12",
        "contextId": "0195f3a2-6c50-7d3c-8f10-4b5a6c7d8e9f",
        "status": {"state": "TASK_STATE_UNSPECIFIED"}
    });
    let event: TaskStatusUpdateEvent = serde_json::from_value(wire).unwrap();
    let status = event.status.unwrap();
    assert!(matches!(
        check_status(Some(TaskState::Working), &status),
        Err(LifecycleError::Unspecified)
    ));
}

// [0005-009] a status naming AUTH_REQUIRED is not emitted in M1.
#[test]
fn req_0005_009_auth_required_wire_state_is_not_emitted() {
    let wire = json!({
        "taskId": "0195f3a2-6c20-7e21-9b4d-2a7c8e9f0a12",
        "contextId": "0195f3a2-6c50-7d3c-8f10-4b5a6c7d8e9f",
        "status": {"state": "TASK_STATE_AUTH_REQUIRED"}
    });
    let event: TaskStatusUpdateEvent = serde_json::from_value(wire).unwrap();
    let state = TaskState::from_wire(event.status.unwrap().state).unwrap();
    assert_eq!(state, TaskState::AuthRequired);
    assert!(matches!(
        check_emittable(state),
        Err(LifecycleError::NotEmittable(TaskState::AuthRequired))
    ));
}

// [0005-049] a failure code outside the registry on the wire is refused, not coerced.
#[test]
fn req_0005_049_unknown_failure_code_on_the_wire_is_refused() {
    let wire = json!({
        "id": "0195f3a2-6c30-7e21-9b4d-2a7c8e9f0a13",
        "contextId": "0195f3a2-6c60-7d3c-8f10-4b5a6c7d8e9f",
        "status": {
            "state": "TASK_STATE_FAILED",
            "message": {
                "messageId": "7e8f9a0b-1c2d-4e3f-8a4b-5c6d7e8f9a0b",
                "role": "ROLE_AGENT",
                "parts": [],
                "metadata": {"a2abus.failure": {"code": "EXPIRED"}}
            }
        }
    });
    let task: Task = serde_json::from_value(wire).unwrap();
    let message = task.status.unwrap().message.unwrap();
    assert!(FailureReason::from_message(&message).is_err());
}
