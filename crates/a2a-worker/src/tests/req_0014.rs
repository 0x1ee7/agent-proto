//! RFC-0014 operator approval of a file write, and the N9 refusal of an unavailable agent
//! (0014-029). The MockBackend performs no file I/O, so these tests assert the gate the Worker
//! controls: which answer the backend receives, the durable audit of the verified operator, and
//! whether a request reaches the Worker at all. File content is not observable here.

use super::*;

/// 0014-013: an operator approves a write. The task moves INPUT_REQUIRED, WORKING, COMPLETED;
/// the backend receives the allow answer; the audit records the verified operator.
#[test]
fn req_0014_013_operator_approval_applies_write() {
    let mut rig = rig();
    let (bus, mock, store) = (&rig.bus, &rig.mock, &rig.store);
    drive(&mut rig.worker, async {
        let mut events = observer(bus, "obs-0014-013").await;
        mock.enqueue(vec![permission("p1", "edit"), completed()]);
        let task = submit(bus, "write notes.txt").await;
        let (_, request) = next_approval(&mut events, &task.id).await;

        let digest = approval_digest(store, &request.request_id);
        decide(
            bus,
            &task.id,
            &task.context_id,
            &decision(&request, ALLOW_ONCE_ID, &digest),
        )
        .await
        .unwrap();

        // The INPUT_REQUIRED event was consumed by `next_approval` above.
        let mut states = vec![pb::TaskState::InputRequired as i32];
        loop {
            let ev = next_event(&mut events).await;
            if ev.task != task.id {
                continue;
            }
            if let Some(state) = event_state(&ev) {
                states.push(state);
                if state == pb::TaskState::Completed as i32 {
                    break;
                }
            }
        }
        assert_eq!(
            states,
            vec![
                pb::TaskState::InputRequired as i32,
                pb::TaskState::Working as i32,
                pb::TaskState::Completed as i32,
            ]
        );
        assert_eq!(
            count_commands(mock, |c| matches!(
                c,
                Command::Resolve(id, PermissionAnswer::Selected { option_id })
                    if id.as_str() == "p1" && option_id == ALLOW_ONCE_ID
            )),
            1
        );
        let record = approval_record(store, &request.request_id).expect("approval is durable");
        assert_eq!(record["decided_by"].as_str(), Some(OPERATOR));
    });
}

/// 0014-014: an operator denies a write. The backend never receives the allow answer; with a
/// turn that then succeeds the task is COMPLETED without the write.
#[test]
fn req_0014_014_operator_denial_never_applies_write() {
    let mut rig = rig();
    let (bus, mock, store) = (&rig.bus, &rig.mock, &rig.store);
    drive(&mut rig.worker, async {
        let mut events = observer(bus, "obs-0014-014").await;
        mock.enqueue(vec![permission("p1", "edit"), completed()]);
        let task = submit(bus, "write notes.txt").await;
        let (_, request) = next_approval(&mut events, &task.id).await;

        let digest = approval_digest(store, &request.request_id);
        decide(
            bus,
            &task.id,
            &task.context_id,
            &decision(&request, REJECT_ONCE_ID, &digest),
        )
        .await
        .unwrap();

        wait_for_state(bus, &task.id, pb::TaskState::Completed).await;
        assert_eq!(
            count_commands(mock, |c| matches!(
                c,
                Command::Resolve(_, PermissionAnswer::Selected { option_id })
                    if option_id == ALLOW_ONCE_ID
            )),
            0,
            "a denied write is never allowed"
        );
    });
}

/// 0014-015: no operator decision before expiry. The backend receives `cancelled`, the task
/// fails with APPROVAL_TIMEOUT, and a decision that arrives after expiry is refused and
/// changes nothing.
#[test]
fn req_0014_015_expired_approval_refuses_late_decision() {
    let mut rig = rig();
    let (bus, mock, store) = (&rig.bus, &rig.mock, &rig.store);
    drive(&mut rig.worker, async {
        let mut events = observer(bus, "obs-0014-015").await;
        mock.enqueue(vec![permission("p1", "edit"), completed()]);
        let task = submit(bus, "write notes.txt").await;
        let (_, request) = next_approval(&mut events, &task.id).await;

        advance(bus, APPROVAL_EXPIRY_MS);
        let failed = wait_for_state(bus, &task.id, pb::TaskState::Failed).await;
        assert_eq!(failure_code(&failed).as_deref(), Some("APPROVAL_TIMEOUT"));
        assert_eq!(
            count_commands(mock, |c| matches!(
                c,
                Command::Resolve(_, PermissionAnswer::Cancelled)
            )),
            1
        );

        let digest = approval_digest(store, &request.request_id);
        let late = decide(
            bus,
            &task.id,
            &task.context_id,
            &decision(&request, ALLOW_ONCE_ID, &digest),
        )
        .await;
        assert!(late.is_err(), "decision after expiry is refused");
        assert_eq!(
            count_commands(mock, |c| matches!(
                c,
                Command::Resolve(_, PermissionAnswer::Selected { .. })
            )),
            0
        );
        assert_eq!(
            state(&get(bus, &task.id).await),
            pb::TaskState::Failed as i32
        );
    });
}

/// 0014-029 (N9): an agent whose backend is not live and cannot start refuses requests. The
/// worker withdraws its RPC subscription, so the bus has no responder (RFC-0011: local
/// `UNAVAILABLE`, which the gateway reports as `AGENT_UNAVAILABLE`, RFC-0008). No task is created
/// and no start is attempted. The subscription returns once the backend is live again.
#[test]
fn req_0014_029_unstartable_backend_refuses_requests() {
    let mut rig = rig();
    let (bus, mock) = (&rig.bus, &rig.mock);
    drive(&mut rig.worker, async {
        mock.go_down("agent process is not running");
        yield_now().await;
        let refused = try_send_text(bus, "", "", "while down").await;
        assert!(
            matches!(refused, Err(BusError::NoResponder)),
            "request to an unstartable agent was answered"
        );
        assert_eq!(count_commands(mock, |c| matches!(c, Command::Start(_))), 0);

        mock.restart();
        yield_now().await;
        mock.enqueue(vec![completed()]);
        let task = submit(bus, "after restart").await;
        let done = wait_for_state(bus, &task.id, pb::TaskState::Completed).await;
        assert_eq!(state(&done), pb::TaskState::Completed as i32);
    });
}

/// 0014-029: a backend that is not live but can still start (a cold start, or a respawn in
/// progress) is not refused. The request is admitted and waits for the backend.
#[test]
fn req_0014_029_startable_backend_is_not_refused() {
    let mut rig = rig_starting();
    let (bus, mock) = (&rig.bus, &rig.mock);
    drive(&mut rig.worker, async {
        mock.enqueue(vec![Step::Hold, completed()]);
        let task = submit(bus, "cold start").await;
        wait_for_state(bus, &task.id, pb::TaskState::Working).await;

        mock.set_starting(false);
        mock.release();
        let done = wait_for_state(bus, &task.id, pb::TaskState::Completed).await;
        assert_eq!(state(&done), pb::TaskState::Completed as i32);
    });
}

/// 0014-029 with 0005-053: a turn admitted while the backend was starting has not reached an
/// agent child. When the backend can no longer start, that turn fails at once with
/// `BACKEND_CRASHED` and a detail carrying `BACKEND_NOT_LIVE`; it does not wait for its deadline
/// for a respawn that will never come.
#[test]
fn req_0014_029_turn_waiting_on_unstartable_backend_fails() {
    let mut rig = rig_starting();
    let (bus, mock) = (&rig.bus, &rig.mock);
    drive(&mut rig.worker, async {
        mock.enqueue(vec![Step::Hold, completed()]);
        let task = submit(bus, "waiting on start").await;
        wait_for_state(bus, &task.id, pb::TaskState::Working).await;

        mock.go_down("agent process is not running");
        // The worker refuses RPC while the backend cannot start (0014-029), so the outcome is read
        // once the driver is back.
        advance(bus, 1);
        // Let the worker observe the loss before the driver is back.
        yield_now().await;
        mock.restart();
        advance(bus, 1);
        // Let the worker re-serve the restored backend before the outcome is read.
        yield_now().await;
        let failed = wait_for_state(bus, &task.id, pb::TaskState::Failed).await;
        assert_eq!(failure_code(&failed).as_deref(), Some("BACKEND_CRASHED"));
        let detail = failure_detail(&failed).expect("the failure carries a detail");
        assert!(detail.contains("BACKEND_NOT_LIVE"), "{detail}");
    });
}
