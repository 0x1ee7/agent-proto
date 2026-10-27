//! RFC-0002 approval wire shape: the request is an INPUT_REQUIRED event, the decision is a send.

use super::*;
use std::pin::pin;
use std::sync::Arc;
use std::task::Poll;

use super::peer::text;
use a2a_client::{CallOptions, Client, Config as ClientConfig};
use a2a_types::ids::TaskId;
use a2a_types::vocab::ext::SEQUENCE;
use pbjson_types::{Struct, value::Kind};

/// 0002-065: an approval request is an INPUT_REQUIRED status event on the task's event subject,
/// carrying the ApprovalRequest under the approval extension URI; the decision is a `send` for
/// the same agent, task and context. Failure path: a decision with a mismatched context is
/// refused, and the approval stays open for a correct decision.
#[test]
fn req_0002_065_approval_is_event_and_decision_is_send() {
    let mut rig = rig();
    let (bus, mock, store) = (&rig.bus, &rig.mock, &rig.store);
    drive(&mut rig.worker, async {
        let mut events = observer(bus, "obs-0002-065").await;
        mock.enqueue(vec![permission("p1", "edit"), completed()]);
        let task = submit(bus, "write").await;

        let (ev, request) = next_approval(&mut events, &task.id).await;
        assert_eq!(ev.task, task.id, "event subject carries the task id");
        assert_eq!(event_state(&ev), Some(pb::TaskState::InputRequired as i32));
        assert_eq!(request.task_id, task.id);

        let digest = approval_digest(store, &request.request_id);
        let wrong_context = decide(
            bus,
            &task.id,
            "not-the-context",
            &decision(&request, ALLOW_ONCE_ID, &digest),
        )
        .await;
        assert!(wrong_context.is_err(), "mismatched context is refused");
        assert_eq!(
            state(&get(bus, &task.id).await),
            pb::TaskState::InputRequired as i32
        );

        let accepted = decide(
            bus,
            &task.id,
            &task.context_id,
            &decision(&request, ALLOW_ONCE_ID, &digest),
        )
        .await
        .unwrap();
        match accepted.payload {
            Some(pb::send_message_response::Payload::Task(reply)) => {
                assert_eq!(reply.id, task.id);
                assert_eq!(reply.context_id, task.context_id);
            }
            other => panic!("expected Task reply to the decision, got {other:?}"),
        }
        wait_for_state(bus, &task.id, pb::TaskState::Completed).await;
    });
}

/// Reads the sequence from a metadata struct under the sequence extension (RFC-0002 0002-042).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn sequence_field(metadata: Option<&Struct>) -> Option<u64> {
    let Some(Kind::StructValue(ext)) = metadata?.fields.get(SEQUENCE)?.kind.as_ref() else {
        return None;
    };
    let Some(Kind::NumberValue(n)) = ext.fields.get("seq")?.kind.as_ref() else {
        return None;
    };
    Some(*n as u64)
}

/// 0002-042 / 0002-051: every streamed event and the subscribe snapshot carry the task sequence,
/// and the client's own event decoder accepts them. Events are contiguous from the snapshot.
#[test]
fn req_0002_042_events_carry_sequence_through_client_decoder() {
    let mut rig = rig();
    let (bus, mock) = (&rig.bus, &rig.mock);
    drive(&mut rig.worker, async {
        mock.enqueue(vec![Step::Hold, text("answer"), completed()]);
        let task = submit(bus, "sequenced").await;
        wait_for_state(bus, &task.id, pb::TaskState::Working).await;

        let client = Client::new(
            bus.clone(),
            Arc::new(VirtualClock(bus.control())),
            ClientConfig::default(),
            tenant(),
            agent(REQUESTER),
        )
        .unwrap();
        let id = TaskId::parse(&task.id).unwrap();
        let (snapshot, mut events) = client
            .subscribe(&agent(WORKER), &id, Some(0), &CallOptions::default())
            .await
            .unwrap();
        let snapshot_seq =
            sequence_field(snapshot.metadata.as_ref()).expect("snapshot carries the task sequence");

        mock.release();
        let mut seqs = Vec::new();
        let mut kinds = Vec::new();
        loop {
            let event = events
                .next()
                .await
                .unwrap()
                .expect("stream stays open until the terminal event");
            seqs.push(event.seq);
            let terminal = match &event.response.payload {
                Some(pb::stream_response::Payload::StatusUpdate(update)) => {
                    kinds.push("status");
                    update.status.as_ref().map(|s| s.state) == Some(pb::TaskState::Completed as i32)
                }
                Some(pb::stream_response::Payload::ArtifactUpdate(_)) => {
                    kinds.push("artifact");
                    false
                }
                other => panic!("unexpected payload {other:?}"),
            };
            if terminal {
                break;
            }
        }
        assert!(
            kinds.contains(&"artifact"),
            "an artifact event was streamed"
        );
        assert_eq!(
            kinds.last(),
            Some(&"status"),
            "the stream ends on the terminal status"
        );
        assert!(snapshot_seq >= 1, "snapshot carries a committed sequence");
        assert_eq!(seqs.first().copied(), Some(1));
        assert!(
            seqs.windows(2).all(|w| w[1] == w[0] + 1),
            "sequence is contiguous: {seqs:?}"
        );
    });
}

/// 0002-050: a blocking send (no `return_immediately`) does not answer while its task is in
/// flight. The reply is held until the caller's deadline, then carries the task's current state.
#[test]
fn req_0002_050_blocking_send_holds_reply_until_deadline() {
    let mut rig = rig();
    let (bus, mock) = (&rig.bus, &rig.mock);
    drive(&mut rig.worker, async {
        mock.enqueue(vec![Step::Hold, completed()]);
        let body = RequestBody::SendMessage(pb::SendMessageRequest {
            tenant: TENANT.to_owned(),
            message: Some(pb::Message {
                message_id: MessageId::generate().as_str().to_owned(),
                role: pb::Role::User as i32,
                parts: vec![pb::Part {
                    content: Some(pb::part::Content::Text("blocking".to_owned())),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        });
        let mut send = pin!(call_ttl(bus, REQUESTER, Op::Send, body, Vec::new(), 10_000));
        let mut early = None;
        for _ in 0..50 {
            yield_now().await;
            if early.is_none()
                && let Poll::Ready(reply) = poll_once(send.as_mut())
            {
                early = Some(reply);
            }
        }
        assert!(early.is_none(), "blocking reply left before the deadline");

        advance(bus, 10_000);
        let reply = loop {
            yield_now().await;
            if let Poll::Ready(reply) = poll_once(send.as_mut()) {
                break reply;
            }
        };
        let response = envelope::decode_reply::<pb::SendMessageResponse>(&reply).unwrap();
        let task = match response.payload {
            Some(pb::send_message_response::Payload::Task(task)) => task,
            other => panic!("unexpected payload {other:?}"),
        };
        assert_eq!(state(&task), pb::TaskState::Working as i32);
    });
}
