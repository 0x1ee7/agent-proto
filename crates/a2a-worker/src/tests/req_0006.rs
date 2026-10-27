//! RFC-0006 approvals: expiry, cancel, and the allow/deny decision paths.

use super::*;
use crate::backend::{Outcome, ReasonToken};

/// 0006-017: an approval whose `expires_at` passes while open is EXPIRED; the backend is
/// answered `cancelled` and the task fails with APPROVAL_TIMEOUT. Boundary: 1 ms before expiry
/// the approval is still open.
#[test]
fn req_0006_017_expired_approval_cancels_turn_and_fails_task() {
    let mut rig = rig();
    let (bus, mock) = (&rig.bus, &rig.mock);
    drive(&mut rig.worker, async {
        mock.enqueue(vec![permission("p1", "edit"), completed()]);
        let task = submit(bus, "write").await;
        wait_for_state(bus, &task.id, pb::TaskState::InputRequired).await;

        advance(bus, APPROVAL_EXPIRY_MS - 1);
        assert_eq!(
            state(&get(bus, &task.id).await),
            pb::TaskState::InputRequired as i32
        );

        advance(bus, 1);
        let failed = wait_for_state(bus, &task.id, pb::TaskState::Failed).await;
        assert_eq!(failure_code(&failed).as_deref(), Some("APPROVAL_TIMEOUT"));
        assert_eq!(
            count_commands(mock, |c| matches!(
                c,
                Command::Resolve(id, PermissionAnswer::Cancelled) if id.as_str() == "p1"
            )),
            1
        );
    });
}

/// 0006-018: cancelling a task closes its open approval and answers the backend `cancelled`.
#[test]
fn req_0006_018_cancel_closes_open_approval() {
    let mut rig = rig();
    let (bus, mock) = (&rig.bus, &rig.mock);
    drive(&mut rig.worker, async {
        mock.enqueue(vec![permission("p1", "edit"), completed()]);
        let task = submit(bus, "write").await;
        wait_for_state(bus, &task.id, pb::TaskState::InputRequired).await;

        let reply = cancel(bus, &task.id).await.unwrap();
        assert_eq!(state(&reply), pb::TaskState::Canceled as i32);
        wait_for_state(bus, &task.id, pb::TaskState::Canceled).await;
        assert_eq!(
            count_commands(mock, |c| matches!(
                c,
                Command::Resolve(id, PermissionAnswer::Cancelled) if id.as_str() == "p1"
            )),
            1
        );
    });
}

/// 0006-019: a denied approval answers the backend with the selected reject option; when the
/// turn then completes the task is COMPLETED, with the refusal left in the transcript.
#[test]
fn req_0006_019_denied_turn_completes() {
    let mut rig = rig();
    let (bus, mock, store) = (&rig.bus, &rig.mock, &rig.store);
    drive(&mut rig.worker, async {
        let mut events = observer(bus, "obs-0006-019").await;
        mock.enqueue(vec![permission("p1", "edit"), completed()]);
        let task = submit(bus, "write").await;
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
                Command::Resolve(id, PermissionAnswer::Selected { option_id })
                    if id.as_str() == "p1" && option_id == REJECT_ONCE_ID
            )),
            1
        );
    });
}

/// 0006-019: a denied approval whose turn then fails leaves the task FAILED with APPROVAL_DENIED.
#[test]
fn req_0006_019_denied_turn_fails_with_approval_denied() {
    let mut rig = rig();
    let (bus, mock, store) = (&rig.bus, &rig.mock, &rig.store);
    drive(&mut rig.worker, async {
        let mut events = observer(bus, "obs-0006-019-fail").await;
        mock.enqueue(vec![
            permission("p1", "edit"),
            Step::Terminal(Outcome::Failed {
                reason: ReasonToken::BackendError,
                health_class: None,
                detail: "write refused".to_owned(),
            }),
        ]);
        let task = submit(bus, "write").await;
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

        let failed = wait_for_state(bus, &task.id, pb::TaskState::Failed).await;
        assert_eq!(failure_code(&failed).as_deref(), Some("APPROVAL_DENIED"));
    });
}

/// 0006-020: an allowed approval answers the backend with the selected allow option, the task
/// returns to WORKING, and the turn completes.
#[test]
fn req_0006_020_allowed_approval_resumes_turn() {
    let mut rig = rig();
    let (bus, mock, store) = (&rig.bus, &rig.mock, &rig.store);
    drive(&mut rig.worker, async {
        let mut events = observer(bus, "obs-0006-020").await;
        mock.enqueue(vec![permission("p1", "edit"), completed()]);
        let task = submit(bus, "write").await;
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

        wait_for_state(bus, &task.id, pb::TaskState::Completed).await;
        assert_eq!(
            count_commands(mock, |c| matches!(
                c,
                Command::Resolve(id, PermissionAnswer::Selected { option_id })
                    if id.as_str() == "p1" && option_id == ALLOW_ONCE_ID
            )),
            1
        );
    });
}

/// 0006-006: an approval request carries the working directory and workspace mode of the task.
/// Without a workspace binding the worker's directory applies, in `per-session` mode.
#[test]
fn req_0006_006_approval_carries_working_directory_and_mode() {
    let mut rig = rig();
    let (bus, mock) = (&rig.bus, &rig.mock);
    drive(&mut rig.worker, async {
        let mut events = observer(bus, "obs-0006-006").await;
        mock.enqueue(vec![permission("p1", "edit"), completed()]);
        let task = submit(bus, "write").await;
        let (_, request) = next_approval(&mut events, &task.id).await;

        assert_eq!(request.working_directory, super::WORK_DIR);
        assert_ne!(
            request.working_directory,
            std::env::current_dir().unwrap().to_string_lossy()
        );
        assert_eq!(request.workspace_mode, "per-session");
    });
}
