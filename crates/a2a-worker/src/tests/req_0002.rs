//! RFC-0002 approval wire shape: the request is an INPUT_REQUIRED event, the decision is a send.

use super::*;
use bus_api::bus::RequestStream;
use std::pin::pin;
use std::sync::Arc;
use std::task::Poll;

use super::peer::text;
use crate::rpc::{Response, plain};
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

/// A `SendMessageRequest` body that is not `return_immediately`, as `SendStreamingMessage` carries it.
fn stream_body(text: &str) -> RequestBody {
    RequestBody::SendMessage(pb::SendMessageRequest {
        tenant: TENANT.to_owned(),
        message: Some(pb::Message {
            message_id: MessageId::generate().as_str().to_owned(),
            role: pb::Role::User as i32,
            parts: vec![a2a_client::text(text)],
            ..Default::default()
        }),
        ..Default::default()
    })
}

/// 0002-052: the `stream` reply is a `StreamResponse` whose first element is the Task, it is not
/// held until the task settles, and it carries `A2AB-Last-Seq` that matches the Task's sequence.
#[test]
fn req_0002_052_stream_reply_is_first_stream_element_with_last_seq() {
    let mut rig = rig();
    let (bus, mock) = (&rig.bus, &rig.mock);
    drive(&mut rig.worker, async {
        mock.enqueue(vec![Step::Hold, completed()]);
        let reply = call(
            bus,
            REQUESTER,
            Op::Stream,
            stream_body("streamed"),
            Vec::new(),
        )
        .await;
        let first = envelope::decode_reply::<pb::StreamResponse>(&reply)
            .expect("stream reply decodes as StreamResponse");
        let Some(pb::stream_response::Payload::Task(task)) = first.payload else {
            panic!("expected a Task as the first stream element, got {first:?}");
        };
        assert_eq!(state(&task), pb::TaskState::Working as i32);
        let last_seq = a2a_core::headers::optional(&reply.headers, a2a_core::headers::LAST_SEQ)
            .expect("stream reply carries A2AB-Last-Seq");
        assert_eq!(
            sequence_field(task.metadata.as_ref()),
            Some(last_seq.parse::<u64>().unwrap()),
            "Last-Seq matches the Task's sequence"
        );
    });
}

/// 0002-053: a client that sends with `stream` follows the task's events to the terminal one.
/// This is the `a2a send --follow` path.
#[test]
fn req_0002_053_client_stream_follows_events_to_completion() {
    let mut rig = rig();
    let (bus, mock) = (&rig.bus, &rig.mock);
    drive(&mut rig.worker, async {
        mock.enqueue(vec![Step::Hold, text("answer"), completed()]);
        let client = Client::new(
            bus.clone(),
            Arc::new(VirtualClock(bus.control())),
            ClientConfig::default(),
            tenant(),
            agent(REQUESTER),
        )
        .unwrap();
        let outbound = a2a_client::Outbound::new(vec![a2a_client::text("follow")]);
        let started = client
            .stream(&agent(WORKER), &outbound, &CallOptions::default())
            .await
            .unwrap();
        let a2a_client::StreamStart::Follow { task, mut events } = started else {
            panic!("expected a followable stream");
        };
        assert_eq!(state(&task), pb::TaskState::Working as i32);

        mock.release();
        let last = loop {
            let event = events
                .next()
                .await
                .unwrap()
                .expect("stream stays open until the terminal event");
            if event.stream_end {
                break event.response.payload;
            }
        };
        let Some(pb::stream_response::Payload::StatusUpdate(update)) = last else {
            panic!("the stream ends on a status update, got {last:?}");
        };
        assert_eq!(
            update.status.as_ref().map(|s| s.state),
            Some(pb::TaskState::Completed as i32)
        );
    });
}

/// A reply that cannot be written ends that one request, never the Worker: a body that is not
/// the operation's reply type is answered with a failure, the error text is audited, and the
/// Worker serves the next request.
#[test]
fn req_0002_040_unanswerable_request_does_not_stop_the_worker() {
    let mut rig = rig();
    let bus = rig.bus.clone();
    let get = RequestBody::GetTask(pb::GetTaskRequest {
        tenant: TENANT.to_owned(),
        id: "t".to_owned(),
        history_length: None,
    });
    let mut caller = pin!(call_ttl(&bus, REQUESTER, Op::Get, get, Vec::new(), TTL_MS));
    assert!(poll_once(caller.as_mut()).is_pending());
    let request = block_on(async {
        loop {
            let requests = rig.worker.requests.as_mut().expect("worker is serving");
            let mut next = pin!(requests.next());
            if let Poll::Ready(request) = poll_once(next.as_mut()) {
                break request.unwrap();
            }
            yield_now().await;
        }
    });
    let Ok(inbound) =
        envelope::receive(&rig.worker.config.wire, rig.worker.clock.as_ref(), request)
    else {
        panic!("request is accepted");
    };
    // `GetTask` is answered with a Task; a SendMessageResponse cannot be its reply.
    let wrong = plain(Response::Send(pb::SendMessageResponse::default()));
    block_on(rig.worker.answer(inbound, Ok(wrong))).expect("an unanswerable request is not fatal");
    let reply = block_on(async {
        loop {
            if let Poll::Ready(reply) = poll_once(caller.as_mut()) {
                break reply;
            }
            yield_now().await;
        }
    });
    assert!(
        envelope::decode_reply::<pb::Task>(&reply).is_err(),
        "the caller is answered with a failure"
    );
    let note = rig
        .worker
        .audit
        .iter()
        .find(|note| note.reason.starts_with("REPLY_FAILED"))
        .expect("the failure is audited");
    assert!(
        note.reason.contains("does not match the operation"),
        "the audit note carries the error text: {}",
        note.reason
    );

    let (bus, mock) = (&rig.bus, &rig.mock);
    drive(&mut rig.worker, async {
        mock.enqueue(vec![completed()]);
        let task = submit(bus, "after").await;
        wait_for_state(bus, &task.id, pb::TaskState::Completed).await;
    });
}
