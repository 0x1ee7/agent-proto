//! RFC-0002 approval wire shape: the request is an INPUT_REQUIRED event, the decision is a send.

use super::*;

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
