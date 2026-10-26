//! Requirement tests for a2a-client on bus-mem with a virtual clock and a fake worker.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::Duration;

use a2a_client::approval::{PendingApproval, allow_once};
use a2a_client::{
    ApprovalError, CallOptions, ClientError, Directory, Outbound, Outcome, Presence, PresenceState,
    Query, SendReply, text,
};
use a2a_core::approval::ALLOW_ONCE_ID;
use a2a_core::envelope::{ReplyOptions, RequestBody};
use a2a_core::subject::Op;
use a2a_security::decision::request_digest;
use a2a_types::ids::AgentId;
use a2a_types::proto::a2a as pb;
use a2a_types::vocab::ext::APPROVAL;
use common::{Fixture, NOW_MS};
use pbjson_types::{Struct, Value as PbValue};
use serde_json::json;

/// Task with the given state, carrying no approval.
fn task(id: &str, state: pb::TaskState) -> pb::Task {
    pb::Task {
        id: id.to_owned(),
        context_id: "ctx-1".to_owned(),
        status: Some(pb::TaskStatus {
            state: state as i32,
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// `SendMessage` answer carrying `task`.
fn send_answer(task: pb::Task) -> pb::SendMessageResponse {
    pb::SendMessageResponse {
        payload: Some(pb::send_message_response::Payload::Task(task)),
    }
}

/// Input-required task whose status message carries an approval request.
/// `expires_at` is RFC 3339, as proto-JSON encodes `Timestamp`.
fn approval_task(task_id: &str, expires_at: &str) -> pb::Task {
    let request = json!({
        "requestId": "req-1",
        "taskId": task_id,
        "tool": "shell",
        "summary": "remove build directory",
        "requester": "worker-a",
        "nonce": "0123456789abcdef0123",
        "options": [
            {"optionId": ALLOW_ONCE_ID, "name": "Allow once"},
            {"optionId": "reject-once", "name": "Reject once"}
        ],
        "expiresAt": expires_at
    });
    let approval: Struct = serde_json::from_value(request).expect("approval struct");
    let mut metadata = Struct::default();
    metadata.fields.insert(
        APPROVAL.to_owned(),
        PbValue {
            kind: Some(pbjson_types::value::Kind::StructValue(approval)),
        },
    );
    pb::Task {
        id: task_id.to_owned(),
        context_id: "ctx-1".to_owned(),
        status: Some(pb::TaskStatus {
            state: pb::TaskState::InputRequired as i32,
            message: Some(pb::Message {
                metadata: Some(metadata),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Expiry 300 s after NOW_MS, as RFC 3339 (2023-11-14T22:18:20Z).
const EXPIRES_AT: &str = "2023-11-14T22:18:20Z";

/// Pulls the approval decision out of a received `SendMessage` request.
fn decision_of(request: &RequestBody) -> serde_json::Value {
    let RequestBody::SendMessage(send) = request else {
        panic!("expected SendMessage");
    };
    let message = send.message.as_ref().expect("message");
    let part = message.parts.first().expect("decision part");
    let Some(pb::part::Content::Data(value)) = &part.content else {
        panic!("decision is not a data part");
    };
    serde_json::to_value(value).expect("decision json")
}

// [0002-050] A blocking send returns the terminal task it waited for.
#[tokio::test]
async fn req_0002_050_blocking_send_returns_terminal_task() {
    let fx = Fixture::new();
    let client = fx.client();
    let mut requests = fx.serve(Op::Send).await;
    let outbound = Outbound::new(vec![text("hello")]);
    let worker = async {
        let inbound = fx.next_inbound(&mut requests).await;
        let answer = send_answer(task("t-1", pb::TaskState::Completed));
        fx.reply(inbound, &answer);
    };
    let options = CallOptions::default();
    let (outcome, ()) = tokio::join!(
        client.send_and_wait(&fx.worker_id, &outbound, &options),
        worker
    );
    match outcome.expect("outcome") {
        Outcome::Terminal(task) => {
            let state = task.status.expect("status").state;
            assert_eq!(state, pb::TaskState::Completed as i32);
        }
        other => panic!("expected terminal task, got {other:?}"),
    }
}

// [0002-051] The reply's Last-Seq is returned with the value.
#[tokio::test]
async fn req_0002_051_reply_reports_last_seq() {
    let fx = Fixture::new();
    let client = fx.client();
    let mut requests = fx.serve(Op::Send).await;
    let outbound = Outbound::new(vec![text("hello")]);
    let worker = async {
        let inbound = fx.next_inbound(&mut requests).await;
        let answer = send_answer(task("t-2", pb::TaskState::Working));
        inbound
            .reply(
                &answer,
                &ReplyOptions {
                    last_seq: Some(7),
                    applied_extensions: Vec::new(),
                },
            )
            .expect("reply");
    };
    let options = CallOptions::default();
    let (reply, ()) = tokio::join!(client.send(&fx.worker_id, &outbound, &options), worker);
    let reply = reply.expect("send");
    assert_eq!(reply.last_seq, Some(7));
    assert!(matches!(reply.value, SendReply::Task(_)));
}

// [0002-050] A call with no worker answering fails with a bus error instead of hanging.
#[tokio::test]
async fn req_0002_050_send_without_worker_fails() {
    let fx = Fixture::new();
    let client = fx.client();
    let outbound = Outbound::new(vec![text("hello")]);
    let options = CallOptions {
        timeout: Some(Duration::from_millis(50)),
        ..Default::default()
    };
    let result = client.send(&fx.worker_id, &outbound, &options).await;
    assert!(result.is_err(), "send with no responder must fail");
}

// [0006-012] A decision is a SendMessage continuing the waiting task, carrying the request digest.
#[tokio::test]
async fn req_0006_012_decision_continues_task_with_digest() {
    let fx = Fixture::new();
    let client = fx.client();
    let mut requests = fx.serve(Op::Send).await;

    // The worker asks for approval through an input-required task.
    let asked = approval_task("t-approve", EXPIRES_AT);
    let pending = PendingApproval::from_task(&asked)
        .expect("decode")
        .expect("approval present");
    assert_eq!(pending.digest(), request_digest(pending.request()));
    let option = allow_once(&pending).expect("allow-once offered").to_owned();

    let worker = async {
        let inbound = fx.next_inbound(&mut requests).await;
        let decision = decision_of(&inbound.body);
        let reply = send_answer(task("t-approve", pb::TaskState::Working));
        fx.reply(inbound, &reply);
        decision
    };
    let options = CallOptions::default();
    let (sent, decision) = tokio::join!(
        client.decide(&fx.worker_id, &pending, &option, &options),
        worker
    );
    sent.expect("decide");

    assert_eq!(decision["requestSha256"], pending.digest());
    assert_eq!(decision["optionId"], ALLOW_ONCE_ID);
    assert_eq!(decision["taskId"], "t-approve");
    assert_eq!(decision["decidedBy"], "cli");
}

// [0006-013] An option the request does not offer is refused before anything is sent.
#[tokio::test]
async fn req_0006_013_option_not_offered_is_refused() {
    let fx = Fixture::new();
    let client = fx.client();
    let pending = PendingApproval::from_task(&approval_task("t-x", EXPIRES_AT))
        .expect("decode")
        .expect("present");
    let result = client
        .decide(
            &fx.worker_id,
            &pending,
            "no-such-option",
            &CallOptions::default(),
        )
        .await;
    assert!(matches!(
        result,
        Err(ClientError::Approval(ApprovalError::OptionNotOffered))
    ));
}

// [0006-013] An expired request is refused; the check is ordered after the option check.
#[tokio::test]
async fn req_0006_013_expired_request_is_refused() {
    let pending = PendingApproval::from_task(&approval_task("t-y", EXPIRES_AT))
        .expect("decode")
        .expect("present");
    let before_expiry = pending.check(ALLOW_ONCE_ID, NOW_MS);
    assert!(before_expiry.is_ok(), "request is live before expiry");
    let after_expiry = pending.check(ALLOW_ONCE_ID, NOW_MS + 600_000);
    assert!(matches!(
        after_expiry,
        Err(ClientError::Approval(ApprovalError::Expired))
    ));
    let unknown_option = pending.check("no-such-option", NOW_MS + 600_000);
    assert!(matches!(
        unknown_option,
        Err(ClientError::Approval(ApprovalError::OptionNotOffered))
    ));
}

// [0006-013] A task without an approval is not a pending request.
#[test]
fn req_0006_013_plain_task_has_no_approval() {
    let plain = task("t-plain", pb::TaskState::InputRequired);
    let pending = PendingApproval::from_task(&plain).expect("decode");
    assert!(pending.is_none());
}

// [0004-009] Presence records above 1 KiB are refused.
#[test]
fn req_0004_009_oversized_presence_is_refused() {
    let value = vec![b' '; a2a_client::presence::MAX_PRESENCE_BYTES + 1];
    assert!(Presence::parse("worker-a", "t1", &value).is_err());
}

// [0004-008] A presence record must name its agent and tenant; a mismatch is refused.
#[test]
fn req_0004_008_presence_identity_mismatch_is_refused() {
    let record = json!({
        "agentId": "other", "tenant": "t1", "instanceId": "i-1",
        "cardVersion": "1.0.0", "state": "PRESENCE_STATE_READY", "activeTasks": 0
    });
    let bytes = serde_json::to_vec(&record).expect("json");
    assert!(Presence::parse("worker-a", "t1", &bytes).is_err());
    assert!(Presence::parse("other", "t2", &bytes).is_err());
}

// [0004-008] A READY presence record decodes with its fields.
#[test]
fn req_0004_008_ready_presence_decodes() {
    let record = json!({
        "agentId": "worker-a", "tenant": "t1", "instanceId": "i-1",
        "cardVersion": "1.0.0", "state": "PRESENCE_STATE_READY", "activeTasks": 2
    });
    let bytes = serde_json::to_vec(&record).expect("json");
    let presence = Presence::parse("worker-a", "t1", &bytes).expect("presence");
    assert_eq!(presence.state, PresenceState::Ready);
    assert!(presence.is_ready());
    assert_eq!(presence.active_tasks, 2);
}

fn card_json(version: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "name": "Worker A",
        "version": version,
        "skills": [{"id": "echo", "name": "Echo", "tags": ["demo"]}]
    }))
    .expect("card json")
}

fn presence_json(state: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "agentId": "worker-a", "tenant": "t1", "instanceId": "i-1",
        "cardVersion": "1.0.0", "state": state, "activeTasks": 0
    }))
    .expect("presence json")
}

// [0004-019] A synced view holds the card and the live presence of an agent.
// [0004-020] The view is served within 60 s by the local clock.
#[tokio::test]
async fn req_0004_020_view_is_fresh_for_sixty_seconds() {
    let fx = Fixture::new();
    fx.kv_put("a2ab-t1-cards", "worker-a", &card_json("1.0.0"))
        .await;
    fx.kv_put(
        "a2ab-t1-presence",
        "worker-a",
        &presence_json("PRESENCE_STATE_READY"),
    )
    .await;
    let client = fx.client();
    let mut directory = Directory::new(&client);
    directory.sync().await.expect("sync");

    let worker = AgentId::parse("worker-a").expect("id");
    let card = directory
        .card(&worker)
        .expect("fresh")
        .expect("card present");
    assert_eq!(card.version(), "1.0.0");
    let presence = directory
        .presence(&worker)
        .expect("fresh")
        .expect("presence");
    assert!(presence.is_ready());

    fx.clock.advance(60_000);
    assert!(
        directory.card(&worker).is_ok(),
        "exactly 60 s is still fresh"
    );
    fx.clock.advance(1);
    assert!(
        matches!(directory.card(&worker), Err(ClientError::Discovery(_))),
        "older than 60 s must not be served"
    );
}

// [0004-020] A view that was never synced is not served.
#[tokio::test]
async fn req_0004_020_unsynced_view_is_not_served() {
    let fx = Fixture::new();
    let client = fx.client();
    let directory = Directory::new(&client);
    let worker = AgentId::parse("worker-a").expect("id");
    assert!(matches!(
        directory.card(&worker),
        Err(ClientError::Discovery(_))
    ));
}

// [0004-021] A card without a live presence record is offline and is excluded by default.
// [0004-014] Only READY agents are routable by default.
// [0004-023] READY agents are listed first.
#[tokio::test]
async fn req_0004_021_offline_cards_are_excluded_unless_asked() {
    let fx = Fixture::new();
    fx.kv_put("a2ab-t1-cards", "worker-a", &card_json("1.0.0"))
        .await;
    let client = fx.client();
    let mut directory = Directory::new(&client);
    directory.sync().await.expect("sync");

    let query = Query::Skill("echo".to_owned());
    let online = directory.query(&query, false).expect("query");
    assert!(online.is_empty(), "card without presence is offline");
    let everyone = directory.query(&query, true).expect("query");
    assert_eq!(everyone.len(), 1);
    assert!(!everyone[0].is_ready());
}

// [0004-022] The query form selects agents by skill; a non-matching skill finds nothing.
#[tokio::test]
async fn req_0004_022_query_matches_skill_only() {
    let fx = Fixture::new();
    fx.kv_put("a2ab-t1-cards", "worker-a", &card_json("1.0.0"))
        .await;
    fx.kv_put(
        "a2ab-t1-presence",
        "worker-a",
        &presence_json("PRESENCE_STATE_READY"),
    )
    .await;
    let client = fx.client();
    let mut directory = Directory::new(&client);
    directory.sync().await.expect("sync");

    let hit = directory
        .query(&Query::Skill("echo".to_owned()), false)
        .expect("query");
    assert_eq!(hit.len(), 1);
    let miss = directory
        .query(&Query::Skill("absent".to_owned()), false)
        .expect("query");
    assert!(miss.is_empty());
}

// [0004-014] Only READY agents are routed to; a DRAINING agent is not.
#[tokio::test]
async fn req_0004_014_draining_agent_is_not_routable() {
    let fx = Fixture::new();
    fx.kv_put("a2ab-t1-cards", "worker-a", &card_json("1.0.0"))
        .await;
    fx.kv_put(
        "a2ab-t1-presence",
        "worker-a",
        &presence_json("PRESENCE_STATE_DRAINING"),
    )
    .await;
    let client = fx.client();
    let mut directory = Directory::new(&client);
    directory.sync().await.expect("sync");
    let listed = directory
        .query(&Query::Skill("echo".to_owned()), false)
        .expect("query");
    assert!(listed.is_empty(), "draining agents are not routed to");
}
