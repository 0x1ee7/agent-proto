//! RFC-0014 operator approval of a file write. The MockBackend performs no file I/O, so these
//! tests assert the gate the Worker controls: which answer the backend receives, and the
//! durable audit of the verified operator. File content is not observable here.

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
