//! RFC-0005 [0005-004]: operator identities (`cli`, `tui`) act on tasks of any origin through
//! CancelTask, SubscribeToTask and contextId history; every other sender stays origin-scoped.

use super::*;

async fn cancel_as(bus: &MemBus, sender: &str, id: &str) -> Result<pb::Task, ReplyDecodeError> {
    let body = RequestBody::CancelTask(pb::CancelTaskRequest {
        tenant: TENANT.to_owned(),
        id: id.to_owned(),
        ..Default::default()
    });
    let reply = call(bus, sender, Op::Cancel, body, Vec::new()).await;
    envelope::decode_reply::<pb::Task>(&reply)
}

async fn subscribe_as(
    bus: &MemBus,
    sender: &str,
    id: &str,
) -> Result<pb::StreamResponse, ReplyDecodeError> {
    let body = RequestBody::SubscribeToTask(pb::SubscribeToTaskRequest {
        tenant: TENANT.to_owned(),
        id: id.to_owned(),
    });
    let reply = call(bus, sender, Op::Subscribe, body, Vec::new()).await;
    envelope::decode_reply::<pb::StreamResponse>(&reply)
}

async fn get_as(bus: &MemBus, sender: &str, id: &str) -> Result<pb::Task, ReplyDecodeError> {
    let body = RequestBody::GetTask(pb::GetTaskRequest {
        tenant: TENANT.to_owned(),
        id: id.to_owned(),
        history_length: None,
    });
    let reply = call(bus, sender, Op::Get, body, Vec::new()).await;
    envelope::decode_reply::<pb::Task>(&reply)
}

async fn list_as(bus: &MemBus, sender: &str) -> pb::ListTasksResponse {
    let body = RequestBody::ListTasks(pb::ListTasksRequest {
        tenant: TENANT.to_owned(),
        ..Default::default()
    });
    let reply = call(bus, sender, Op::List, body, Vec::new()).await;
    envelope::decode_reply::<pb::ListTasksResponse>(&reply).unwrap()
}

/// 0005-004: an operator cancels a task created by another origin; the task reaches CANCELED.
#[test]
fn req_0005_004_operator_cancels_task_of_another_origin() {
    for operator in ["cli", "tui"] {
        let mut rig = rig();
        let (bus, mock) = (&rig.bus, &rig.mock);
        drive(&mut rig.worker, async {
            mock.enqueue(vec![permission("p1", "edit"), completed()]);
            let task = submit(bus, "write").await;
            wait_for_state(bus, &task.id, pb::TaskState::InputRequired).await;

            let reply = cancel_as(bus, operator, &task.id).await.unwrap();
            assert_eq!(state(&reply), pb::TaskState::Canceled as i32);
            let done = wait_for_state(bus, &task.id, pb::TaskState::Canceled).await;
            assert_eq!(done.id, task.id);
        });
    }
}

/// 0005-004: an operator cancel of a task that is WORKING asks the backend to stop, and the
/// task reaches CANCELED under its owner's origin.
#[test]
fn req_0005_004_operator_cancels_working_task_of_another_origin() {
    let mut rig = rig();
    let (bus, mock) = (&rig.bus, &rig.mock);
    drive(&mut rig.worker, async {
        mock.enqueue(vec![Step::Hold]);
        let task = submit(bus, "long").await;
        wait_for_state(bus, &task.id, pb::TaskState::Working).await;

        cancel_as(bus, "cli", &task.id).await.unwrap();
        assert_eq!(
            count_commands(mock, |c| matches!(c, Command::Cancel(_))),
            1,
            "the backend turn is cancelled"
        );
    });
}

/// 0005-004: a peer origin cannot cancel, subscribe to or read another origin's task; the
/// refusal is TaskNotFound and the task is untouched.
#[test]
fn req_0005_004_peer_cannot_cancel_subscribe_or_read_other_origin_task() {
    let mut rig = rig();
    let (bus, mock) = (&rig.bus, &rig.mock);
    drive(&mut rig.worker, async {
        mock.enqueue(vec![permission("p1", "edit"), completed()]);
        let task = submit(bus, "write").await;
        wait_for_state(bus, &task.id, pb::TaskState::InputRequired).await;

        for peer in [OPERATOR] {
            assert!(cancel_as(bus, peer, &task.id).await.is_err());
            assert!(subscribe_as(bus, peer, &task.id).await.is_err());
            assert!(get_as(bus, peer, &task.id).await.is_err());
            assert!(list_as(bus, peer).await.tasks.is_empty());
        }
        assert_eq!(
            state(&get(bus, &task.id).await),
            pb::TaskState::InputRequired as i32
        );
    });
}

/// 0005-004: an operator subscribes to a task of another origin and receives its snapshot.
#[test]
fn req_0005_004_operator_subscribes_to_task_of_another_origin() {
    let mut rig = rig();
    let (bus, mock) = (&rig.bus, &rig.mock);
    drive(&mut rig.worker, async {
        mock.enqueue(vec![permission("p1", "edit"), completed()]);
        let task = submit(bus, "write").await;
        wait_for_state(bus, &task.id, pb::TaskState::InputRequired).await;

        let snapshot = subscribe_as(bus, "tui", &task.id).await.unwrap();
        let Some(pb::stream_response::Payload::Task(got)) = snapshot.payload else {
            panic!("expected a Task snapshot, got {snapshot:?}");
        };
        assert_eq!(got.id, task.id);
        assert_eq!(state(&got), pb::TaskState::InputRequired as i32);
        assert!(subscribe_as(bus, OPERATOR, &task.id).await.is_err());
    });
}

/// 0005-004: contextId history. Two origins use the same context id string; the contexts stay
/// separate (0002-089), the operator sees the tasks of both, each peer only its own.
#[test]
fn req_0005_004_operator_sees_context_history_across_origins() {
    let mut rig = rig();
    let (bus, mock) = (&rig.bus, &rig.mock);
    drive(&mut rig.worker, async {
        mock.enqueue(vec![completed(), completed()]);
        let first = submit(bus, "from planner").await;
        let reply = call(
            bus,
            OPERATOR,
            Op::Send,
            send_body(&first.context_id, "", "from operator-agent"),
            Vec::new(),
        )
        .await;
        let second = match envelope::decode_reply::<pb::SendMessageResponse>(&reply)
            .unwrap()
            .payload
        {
            Some(pb::send_message_response::Payload::Task(task)) => task,
            other => panic!("expected Task reply, got {other:?}"),
        };
        assert_eq!(second.context_id, first.context_id);
        assert_ne!(second.id, first.id);

        let all = list_as(bus, "cli").await;
        let in_context: Vec<&str> = all
            .tasks
            .iter()
            .filter(|t| t.context_id == first.context_id)
            .map(|t| t.id.as_str())
            .collect();
        assert_eq!(in_context.len(), 2);
        assert!(in_context.contains(&first.id.as_str()));
        assert!(in_context.contains(&second.id.as_str()));

        let own = list_as(bus, OPERATOR).await;
        assert_eq!(own.tasks.len(), 1);
        assert_eq!(own.tasks[0].id, second.id);
        // Each task is readable by the operator, including its message history.
        assert!(get_as(bus, "cli", &first.id).await.is_ok());
        assert!(get_as(bus, OPERATOR, &first.id).await.is_err());
    });
}
