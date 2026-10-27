//! RFC-0005 task lifecycle: cancel and deadline behaviour.

use super::*;

const WORKING: i32 = pb::TaskState::Working as i32;
const CANCELED: i32 = pb::TaskState::Canceled as i32;
const FAILED: i32 = pb::TaskState::Failed as i32;

/// 0005-036: CancelTask on a SUBMITTED task commits CANCELED at once, and the queued turn
/// never starts.
#[test]
fn req_0005_036_cancel_submitted_commits_canceled_immediately() {
    let mut rig = rig();
    let (bus, mock) = (&rig.bus, &rig.mock);
    drive(&mut rig.worker, async {
        // The first turn holds the single slot, so the second task stays SUBMITTED.
        mock.enqueue(vec![Step::Hold]);
        let first = submit(bus, "first").await;
        wait_for_state(bus, &first.id, pb::TaskState::Working).await;
        // Same context: the second task waits behind the first (0005-054).
        let second = submit_in(bus, &first.context_id, "second").await;
        assert_eq!(state(&second), pb::TaskState::Submitted as i32);

        let reply = cancel(bus, &second.id).await.unwrap();
        assert_eq!(state(&reply), CANCELED);
        assert_eq!(state(&get(bus, &second.id).await), CANCELED);
        assert_eq!(count_commands(mock, |c| matches!(c, Command::Start(_))), 1);
    });
}

/// 0005-037: CancelTask on a WORKING task asks the backend to cancel; the task reaches CANCELED
/// once the backend acks, and the Worker does not kill a backend that acks.
#[test]
fn req_0005_037_cancel_working_task_commits_after_backend_ack() {
    let mut rig = rig();
    let (bus, mock) = (&rig.bus, &rig.mock);
    drive(&mut rig.worker, async {
        mock.enqueue(vec![Step::Hold]);
        let task = submit(bus, "work").await;
        wait_for_state(bus, &task.id, pb::TaskState::Working).await;

        // The reply carries the current state; the backend has not acked yet, so WORKING is valid.
        let reply = cancel(bus, &task.id).await.unwrap();
        assert!(matches!(state(&reply), WORKING | CANCELED));

        wait_for_state(bus, &task.id, pb::TaskState::Canceled).await;
        assert_eq!(count_commands(mock, |c| matches!(c, Command::Cancel(_))), 1);
        assert_eq!(
            count_commands(mock, |c| matches!(c, Command::Shutdown(_))),
            0
        );
    });
}

/// 0005-037: a backend that never acks cancel is killed when the 15 s grace expires, and
/// CANCELED commits only then. Boundary: 1 ms before the grace expires the task is still WORKING.
#[test]
fn req_0005_037_cancel_unacked_backend_killed_after_grace() {
    let mut rig = rig();
    let (bus, mock) = (&rig.bus, &rig.mock);
    drive(&mut rig.worker, async {
        mock.set_ignore_cancel(true);
        mock.enqueue(vec![Step::Hold]);
        let task = submit(bus, "stubborn").await;
        wait_for_state(bus, &task.id, pb::TaskState::Working).await;
        cancel(bus, &task.id).await.unwrap();

        advance(bus, CANCEL_GRACE_MS - 1);
        assert_eq!(state(&get(bus, &task.id).await), WORKING);

        advance(bus, 1);
        wait_for_state(bus, &task.id, pb::TaskState::Canceled).await;
        assert_eq!(
            count_commands(mock, |c| matches!(c, Command::Shutdown(_))),
            1,
            "the unacked backend is killed after the grace"
        );
    });
}

/// 0005-041: a non-terminal task past its deadline has its backend killed without grace and
/// fails with DEADLINE_EXCEEDED. Boundary: 1 ms before the deadline the task is still WORKING.
#[test]
fn req_0005_041_deadline_kills_backend_and_fails_task() {
    const DEADLINE_MS: u64 = 10_000;
    let mut rig = rig_with(ApprovalMode::AskAll, DEADLINE_MS);
    let (bus, mock) = (&rig.bus, &rig.mock);
    drive(&mut rig.worker, async {
        mock.enqueue(vec![Step::Hold]);
        let task = submit(bus, "slow").await;
        wait_for_state(bus, &task.id, pb::TaskState::Working).await;

        advance(bus, DEADLINE_MS - 1);
        assert_eq!(state(&get(bus, &task.id).await), WORKING);

        advance(bus, 1);
        let failed = wait_for_state(bus, &task.id, pb::TaskState::Failed).await;
        assert_eq!(state(&failed), FAILED);
        assert_eq!(failure_code(&failed).as_deref(), Some("DEADLINE_EXCEEDED"));
        assert_eq!(
            count_commands(
                mock,
                |c| matches!(c, Command::Shutdown(d) if *d == Duration::ZERO)
            ),
            1,
            "a deadline kills the backend without grace"
        );
    });
}

/// 0005-041: time spent in INPUT_REQUIRED counts toward the deadline. The approval is still
/// open (its expiry is 300 s), so the deadline is what fails the task.
#[test]
fn req_0005_041_deadline_runs_while_input_required() {
    const DEADLINE_MS: u64 = 10_000;
    let mut rig = rig_with(ApprovalMode::AskAll, DEADLINE_MS);
    let (bus, mock) = (&rig.bus, &rig.mock);
    drive(&mut rig.worker, async {
        mock.enqueue(vec![permission("p1", "edit"), completed()]);
        let task = submit(bus, "write").await;
        wait_for_state(bus, &task.id, pb::TaskState::InputRequired).await;

        advance(bus, DEADLINE_MS - 1);
        assert_eq!(
            state(&get(bus, &task.id).await),
            pb::TaskState::InputRequired as i32
        );

        advance(bus, 1);
        let failed = wait_for_state(bus, &task.id, pb::TaskState::Failed).await;
        assert_eq!(failure_code(&failed).as_deref(), Some("DEADLINE_EXCEEDED"));
    });
}

/// 0005-054: a second task in the same context stays SUBMITTED until the first turn closes,
/// then starts in admission order.
#[test]
fn req_0005_054_same_context_turn_waits_for_holder() {
    let mut rig = rig();
    let (bus, mock) = (&rig.bus, &rig.mock);
    drive(&mut rig.worker, async {
        mock.enqueue(vec![Step::Hold]);
        mock.enqueue(vec![completed()]);
        let first = submit(bus, "first").await;
        wait_for_state(bus, &first.id, pb::TaskState::Working).await;
        let second = submit_in(bus, &first.context_id, "second").await;
        assert_eq!(state(&second), pb::TaskState::Submitted as i32);
        assert_eq!(count_commands(mock, |c| matches!(c, Command::Start(_))), 1);

        cancel(bus, &first.id).await.unwrap();
        wait_for_state(bus, &first.id, pb::TaskState::Canceled).await;
        wait_for_state(bus, &second.id, pb::TaskState::Completed).await;
        assert_eq!(count_commands(mock, |c| matches!(c, Command::Start(_))), 2);
    });
}

/// RFC-0005 0005-028: admission refuses a task with QUEUE_FULL once its context has
/// `quota` tasks waiting; the refused task is REJECTED and starts no backend turn.
#[test]
fn req_0005_028_queue_full_is_a_per_context_quota() {
    let mut rig = rig_with_quota(ApprovalMode::AskAll, 3_600_000, 2);
    let (bus, mock) = (&rig.bus, &rig.mock);
    drive(&mut rig.worker, async {
        mock.enqueue(vec![Step::Hold]);
        let first = submit(bus, "first").await;
        wait_for_state(bus, &first.id, pb::TaskState::Working).await;
        let second = submit_in(bus, &first.context_id, "second").await;
        let third = submit_in(bus, &first.context_id, "third").await;
        assert_eq!(state(&second), pb::TaskState::Submitted as i32);
        assert_eq!(state(&third), pb::TaskState::Submitted as i32);

        let fourth = submit_in(bus, &first.context_id, "fourth").await;
        assert_eq!(state(&fourth), pb::TaskState::Rejected as i32);
        assert_eq!(
            count_commands(mock, |c| matches!(c, Command::Start(_))),
            1,
            "the refused task starts no backend turn"
        );
    });
}

/// RFC-0005 0005-054: turns of different contexts run concurrently; a turn queued behind an
/// open turn of its own context waits, so each context stays FIFO.
#[test]
fn req_0005_054_contexts_run_concurrently_and_each_context_stays_fifo() {
    let mut rig = rig();
    let (bus, mock) = (&rig.bus, &rig.mock);
    mock.set_max_open(8);
    drive(&mut rig.worker, async {
        mock.enqueue(vec![Step::Hold]);
        let held = submit(bus, "held").await;
        wait_for_state(bus, &held.id, pb::TaskState::Working).await;

        // A fresh context runs to completion while the first context's turn stays open.
        mock.enqueue(vec![completed()]);
        let other = submit(bus, "other context").await;
        wait_for_state(bus, &other.id, pb::TaskState::Completed).await;
        assert_eq!(state(&get(bus, &held.id).await), WORKING);

        // Same context as the held turn: admitted behind it and not started.
        let queued = submit_in(bus, &held.context_id, "queued").await;
        assert_eq!(state(&queued), pb::TaskState::Submitted as i32);
        assert_eq!(
            count_commands(mock, |c| matches!(c, Command::Start(_))),
            2,
            "only the held and the other-context turns started"
        );
    });
}

/// 0005-031: the backend's fault detail survives on the FAILED task's failure metadata, so the
/// caller can see why the backend stopped (for example a refused connection).
#[test]
fn req_0005_031_fault_detail_is_kept_on_failed_task() {
    let mut rig = rig();
    let (bus, mock) = (&rig.bus, &rig.mock);
    drive(&mut rig.worker, async {
        mock.enqueue(vec![Step::Hold]);
        let task = submit(bus, "faulted").await;
        wait_for_state(bus, &task.id, pb::TaskState::Working).await;
        mock.fault(
            crate::backend::FaultKind::StreamEnded,
            "Couldn't connect to 127.0.0.1:18765 (connection refused)",
        );
        let failed = wait_for_state(bus, &task.id, pb::TaskState::Failed).await;
        assert!(
            format!("{failed:?}").contains("connection refused"),
            "failure metadata lost the backend detail: {failed:?}"
        );
    });
}

/// 0005-031: a terminal failure's own detail is kept on the FAILED task, not only a stream fault's.
#[test]
fn req_0005_031_terminal_failure_detail_is_kept() {
    let mut rig = rig();
    let (bus, mock) = (&rig.bus, &rig.mock);
    drive(&mut rig.worker, async {
        mock.enqueue(vec![Step::Terminal(crate::backend::Outcome::Failed {
            reason: crate::backend::ReasonToken::BackendCrashed,
            health_class: None,
            detail: "exit status 1: Couldn't connect to 127.0.0.1:18765".to_owned(),
        })]);
        let task = submit(bus, "terminal detail").await;
        let failed = wait_for_state(bus, &task.id, pb::TaskState::Failed).await;
        assert!(
            format!("{failed:?}").contains("Couldn't connect"),
            "terminal failure lost its detail: {failed:?}"
        );
    });
}

/// 0005-049, 0005-053: a backend that stops being live fails its open turn with a detail that
/// carries the driver's reason.
#[test]
fn req_0005_053_health_not_live_fails_turn_with_detail() {
    let mut rig = rig();
    let (bus, mock) = (&rig.bus, &rig.mock);
    drive(&mut rig.worker, async {
        mock.enqueue(vec![Step::Hold]);
        let task = submit(bus, "health").await;
        wait_for_state(bus, &task.id, pb::TaskState::Working).await;
        mock.go_down("connection to agent lost");
        advance(bus, 1);
        // Let the worker observe the loss before the driver is back.
        yield_now().await;
        // The worker refuses RPC while the backend cannot start (0014-029), so the outcome is
        // read once the driver is back.
        mock.restart();
        advance(bus, 1);
        // Let the worker re-serve the restored backend before the outcome is read.
        yield_now().await;
        let failed = wait_for_state(bus, &task.id, pb::TaskState::Failed).await;
        assert_eq!(failure_code(&failed).as_deref(), Some("BACKEND_CRASHED"));
        let detail = failure_detail(&failed).expect("health failure carries a detail");
        assert!(
            detail.contains("backend not live: connection to agent lost"),
            "health failure detail: {detail}"
        );
    });
}

/// 0005-049: a backend that exits mid-turn fails the task with its own non-empty detail.
#[test]
fn req_0005_049_backend_exit_fails_turn_with_detail() {
    let mut rig = rig();
    let (bus, mock) = (&rig.bus, &rig.mock);
    drive(&mut rig.worker, async {
        mock.enqueue(vec![Step::Hold]);
        let task = submit(bus, "exit").await;
        wait_for_state(bus, &task.id, pb::TaskState::Working).await;
        mock.crash("agent exited with status 1");
        // A down mock withdraws the RPC subscription (0014-029); restart it to read the outcome.
        mock.restart();
        let failed = wait_for_state(bus, &task.id, pb::TaskState::Failed).await;
        let detail = failure_detail(&failed).expect("backend exit carries a detail");
        assert!(detail.contains("agent exited with status 1"), "{detail}");
    });
}

/// 0005-041, 0005-049: a deadline failure carries a non-null detail.
#[test]
fn req_0005_049_deadline_failure_carries_detail() {
    const DEADLINE_MS: u64 = 10_000;
    let mut rig = rig_with(ApprovalMode::AskAll, DEADLINE_MS);
    let (bus, mock) = (&rig.bus, &rig.mock);
    drive(&mut rig.worker, async {
        mock.enqueue(vec![Step::Hold]);
        let task = submit(bus, "slow").await;
        wait_for_state(bus, &task.id, pb::TaskState::Working).await;
        advance(bus, DEADLINE_MS);
        let failed = wait_for_state(bus, &task.id, pb::TaskState::Failed).await;
        assert_eq!(failure_code(&failed).as_deref(), Some("DEADLINE_EXCEEDED"));
        let detail = failure_detail(&failed).expect("deadline failure carries a detail");
        assert!(detail.contains("task deadline reached"), "{detail}");
    });
}

/// 0005-053: a backend still starting (lazy spawn, health not live) keeps the turn the driver
/// accepted; the turn is not closed as lost, and it completes once the backend is live.
#[test]
fn req_0005_053_starting_backend_keeps_open_turn() {
    let mut rig = rig_starting();
    let (bus, mock) = (&rig.bus, &rig.mock);
    drive(&mut rig.worker, async {
        mock.enqueue(vec![
            Step::Hold,
            Step::Terminal(crate::backend::Outcome::Completed),
        ]);
        let task = submit(bus, "starting").await;
        wait_for_state(bus, &task.id, pb::TaskState::Working).await;
        advance(bus, 1);
        assert_eq!(state(&get(bus, &task.id).await), WORKING);

        mock.set_starting(false);
        advance(bus, 1);
        mock.release();
        let done = wait_for_state(bus, &task.id, pb::TaskState::Completed).await;
        assert_eq!(state(&done), pb::TaskState::Completed as i32);
    });
}

/// 0005-053, 0009-008: a turn the driver accepted while its backend was starting has not reached
/// an agent child yet. A respawn does not lose it; it completes once the backend is live.
#[test]
fn req_0005_053_turn_admitted_while_starting_survives_respawn() {
    let mut rig = rig_starting();
    let (bus, mock) = (&rig.bus, &rig.mock);
    drive(&mut rig.worker, async {
        mock.enqueue(vec![
            Step::Hold,
            Step::Terminal(crate::backend::Outcome::Completed),
        ]);
        let task = submit(bus, "starting").await;
        wait_for_state(bus, &task.id, pb::TaskState::Working).await;

        mock.restart();
        advance(bus, 1);
        assert_eq!(state(&get(bus, &task.id).await), WORKING);

        mock.release();
        let done = wait_for_state(bus, &task.id, pb::TaskState::Completed).await;
        assert_eq!(state(&done), pb::TaskState::Completed as i32);
    });
}

/// 0005-053: a backend that stops being live fails the turn in flight. A turn admitted in the
/// same window (the driver accepted it while the backend was starting) is not part of that loss,
/// even though the worker observes the flip only after admitting it.
#[test]
fn req_0005_053_turn_admitted_during_flip_is_not_closed_by_it() {
    let mut rig = rig();
    let (bus, mock) = (&rig.bus, &rig.mock);
    mock.set_max_open(2);
    drive(&mut rig.worker, async {
        mock.enqueue(vec![Step::Hold]);
        let first = submit(bus, "in flight").await;
        wait_for_state(bus, &first.id, pb::TaskState::Working).await;

        mock.set_starting(true);
        mock.enqueue(vec![Step::Hold, completed()]);
        let second = submit(bus, "admitted during flip").await;

        let lost = wait_for_state(bus, &first.id, pb::TaskState::Failed).await;
        assert_eq!(failure_code(&lost).as_deref(), Some("BACKEND_CRASHED"));
        assert_eq!(state(&get(bus, &second.id).await), WORKING);

        mock.set_starting(false);
        mock.release();
        let done = wait_for_state(bus, &second.id, pb::TaskState::Completed).await;
        assert_eq!(state(&done), pb::TaskState::Completed as i32);
    });
}
