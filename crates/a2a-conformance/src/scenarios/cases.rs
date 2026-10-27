//! Case bodies for the in-process scenarios. Each builds its own rig and returns `Err` with the
//! observed difference when an assertion fails. Requirement IDs are cited on the test side in
//! `scenarios/mod.rs`.

use a2a_client::approval::{self, PendingApproval};
use a2a_client::{CallOptions, ClientError, Outbound, SendReply, text};
use a2a_core::approval::{ALLOW_ONCE_ID, REJECT_ONCE_ID};
use a2a_core::envelope::EncodeError;
use a2a_core::errors::{LocalError, WireError};
use a2a_types::failure::{FailureCode, FailureReason};
use a2a_types::ids::{MessageId, TaskId};
use a2a_types::proto::a2a as pb;
use a2a_worker::backend::{
    Outcome, PermissionAnswer, PermissionOption, PermissionRequest, RequestId,
};
use a2a_worker::mock::{Command, Step};
use bus_api::error::BusError;

use super::rig::{
    Bus, OBSERVER, REQUESTER, Rig, WORKER, advance, agent, client, count_commands, drive,
    poll_once, reopen, rig, task_id, task_state, wait_for_state,
};
use super::rig::{RigSpec, rig_with, yield_now};
use super::wire::{Tamper, accepted, raw, refusal, send_body};
use a2a_client::Client;
use a2a_core::envelope;
use a2a_core::subject::Op;
use a2a_types::ids::ContextId;
use bus_api::control::BusControl;
use std::pin::pin;
use std::task::Poll;

/// Approval expiry in milliseconds (RFC-0006 default, 300 s).
const EXPIRY_MS: u64 = super::rig::APPROVAL_EXPIRY_MS;
/// Larger than any inline limit, so the payload must be refused without a blob reference.
const OVERSIZE_BYTES: usize = 8 * 1024 * 1024;
/// Bound on polls waiting for a task to reach a terminal state.
const TERMINAL_POLLS: usize = 10_000;

fn outbound(content: &str) -> Outbound {
    let mut outbound = Outbound::new(vec![text(content)]);
    outbound.return_immediately = true;
    outbound
}

fn completed() -> Step {
    Step::Terminal(Outcome::Completed)
}

/// An `edit` permission request offering allow once and reject once (RFC-0006).
fn permission_step(request_id: &str) -> Step {
    Step::Permission(PermissionRequest {
        request_id: RequestId::new(request_id),
        tool_kind: "edit".to_owned(),
        tool_title: "edit file".to_owned(),
        options: vec![
            PermissionOption {
                option_id: ALLOW_ONCE_ID.to_owned(),
                kind: ALLOW_ONCE_ID.to_owned(),
            },
            PermissionOption {
                option_id: REJECT_ONCE_ID.to_owned(),
                kind: REJECT_ONCE_ID.to_owned(),
            },
        ],
    })
}

fn expect_eq(actual: usize, want: usize, what: &str) -> Result<(), String> {
    if actual == want {
        Ok(())
    } else {
        Err(format!("{what}: expected {want}, observed {actual}"))
    }
}

/// Sends `outbound` and returns the Task the worker answered with.
async fn send_task(client: &Client<Bus>, outbound: &Outbound) -> Result<pb::Task, String> {
    let reply = client
        .send(&agent(WORKER)?, outbound, &CallOptions::default())
        .await
        .map_err(|e| format!("send: {e:?}"))?;
    match reply.value {
        SendReply::Task(task) => Ok(task),
        other => Err(format!("expected a Task reply, got {other:?}")),
    }
}

/// Waits for INPUT_REQUIRED and returns the pending approval the worker opened.
async fn pending_approval(client: &Client<Bus>, id: &TaskId) -> Result<PendingApproval, String> {
    // The approval request travels in the status message of the INPUT_REQUIRED event, not in
    // the task snapshot (RFC-0006 approval extension), so the event stream is read from seq 0.
    let (snapshot, mut events) = client
        .subscribe(&agent(WORKER)?, id, Some(0), &CallOptions::default())
        .await
        .map_err(|e| format!("subscribe: {e:?}"))?;
    loop {
        let Some(event) = events.next().await.map_err(|e| format!("event: {e:?}"))? else {
            return Err("stream ended before an approval request arrived".to_owned());
        };
        if let Some(pb::stream_response::Payload::StatusUpdate(update)) = event.response.payload {
            let mut task = snapshot.clone();
            task.status = update.status;
            if let Some(pending) =
                PendingApproval::from_task(&task).map_err(|e| format!("approval: {e:?}"))?
            {
                return Ok(pending);
            }
        }
    }
}

/// The FAILED reason the worker attached to a task, when it carries one.
fn failure_code(task: &pb::Task) -> Result<Option<FailureCode>, String> {
    let Some(message) = task
        .status
        .as_ref()
        .and_then(|status| status.message.as_ref())
    else {
        return Ok(None);
    };
    let reason = FailureReason::from_message(message).map_err(|e| format!("failure: {e:?}"))?;
    Ok(reason.map(|reason| reason.code))
}

fn expect_failure(task: &pb::Task, want: FailureCode) -> Result<(), String> {
    let got = failure_code(task)?;
    if got == Some(want) {
        Ok(())
    } else {
        Err(format!("failure code: expected {want:?}, observed {got:?}"))
    }
}

/// S4a (RFC-0014 [0014-013]): the operator approves the write; the turn completes.
///
/// In-process substitute: the approval is decided through the real client, against the
/// MockBackend. The live operator path through the TUI and the audit log was not run.
pub(crate) fn s4a_operator_approves() -> Result<(), String> {
    let mut rig = rig()?;
    let requester = client(&rig, REQUESTER)?;
    let Rig { mock, worker, .. } = &mut rig;
    drive(worker, async {
        mock.enqueue(vec![permission_step("perm-1"), completed()]);
        let task = send_task(&requester, &outbound("write")).await?;
        let id = task_id(&task)?;
        let pending = pending_approval(&requester, &id).await?;
        let option = approval::allow_once(&pending).ok_or("no allow-once option offered")?;
        requester
            .decide(&agent(WORKER)?, &pending, option, &CallOptions::default())
            .await
            .map_err(|e| format!("decide: {e:?}"))?;
        wait_for_state(&requester, &id, pb::TaskState::Completed).await?;
        expect_eq(
            count_commands(mock, |c| {
                matches!(
                    c,
                    Command::Resolve(id, PermissionAnswer::Selected { option_id })
                        if id.as_str() == "perm-1" && option_id == ALLOW_ONCE_ID
                )
            }),
            1,
            "allow-once resolutions",
        )
    })
}

/// S4b (RFC-0014 [0014-014]): the operator denies; the backend receives the reject option and
/// the turn ends, so the task is COMPLETED without the write (RFC-0006 [0006-019]).
pub(crate) fn s4b_operator_denies() -> Result<(), String> {
    let mut rig = rig()?;
    let requester = client(&rig, REQUESTER)?;
    let Rig { mock, worker, .. } = &mut rig;
    drive(worker, async {
        mock.enqueue(vec![permission_step("perm-1"), completed()]);
        let task = send_task(&requester, &outbound("write")).await?;
        let id = task_id(&task)?;
        let pending = pending_approval(&requester, &id).await?;
        requester
            .decide(
                &agent(WORKER)?,
                &pending,
                REJECT_ONCE_ID,
                &CallOptions::default(),
            )
            .await
            .map_err(|e| format!("decide: {e:?}"))?;
        wait_for_state(&requester, &id, pb::TaskState::Completed).await?;
        expect_eq(
            count_commands(mock, |c| {
                matches!(
                    c,
                    Command::Resolve(id, PermissionAnswer::Selected { option_id })
                        if id.as_str() == "perm-1" && option_id == REJECT_ONCE_ID
                )
            }),
            1,
            "reject-once resolutions",
        )
    })
}

/// S4c (RFC-0014 [0014-015]): no decision until expiry. The backend is answered `cancelled`,
/// the task FAILs with APPROVAL_TIMEOUT, and a late decision is refused.
pub(crate) fn s4c_approval_expires() -> Result<(), String> {
    let mut rig = rig()?;
    let requester = client(&rig, REQUESTER)?;
    let Rig {
        mock,
        worker,
        clock,
        ..
    } = &mut rig;
    drive(worker, async {
        mock.enqueue(vec![permission_step("perm-1"), completed()]);
        let task = send_task(&requester, &outbound("write")).await?;
        let id = task_id(&task)?;
        let pending = pending_approval(&requester, &id).await?;

        advance(clock, EXPIRY_MS - 1)?;
        expect_eq(
            usize::try_from(task_state(
                &wait_for_state(&requester, &id, pb::TaskState::InputRequired).await?,
            ))
            .map_err(|e| format!("state: {e:?}"))?,
            usize::try_from(pb::TaskState::InputRequired as i32).map_err(|e| format!("{e:?}"))?,
            "state one millisecond before expiry",
        )?;

        advance(clock, 1)?;
        let failed = wait_for_state(&requester, &id, pb::TaskState::Failed).await?;
        expect_failure(&failed, FailureCode::ApprovalTimeout)?;
        expect_eq(
            count_commands(mock, |c| {
                matches!(
                    c,
                    Command::Resolve(id, PermissionAnswer::Cancelled) if id.as_str() == "perm-1"
                )
            }),
            1,
            "cancelled resolutions",
        )?;

        let late = requester
            .decide(
                &agent(WORKER)?,
                &pending,
                ALLOW_ONCE_ID,
                &CallOptions::default(),
            )
            .await;
        if late.is_ok() {
            return Err("late decision after expiry was accepted".to_owned());
        }
        expect_eq(
            count_commands(mock, |c| {
                matches!(c, Command::Resolve(_, PermissionAnswer::Selected { .. }))
            }),
            0,
            "resolutions after a late decision",
        )
    })
}

/// S7 (RFC-0014 [0014-018]): the same messageId sent three times starts the backend once and
/// returns the existing Task each time.
pub(crate) fn s7_duplicate_submission() -> Result<(), String> {
    let mut rig = rig()?;
    let requester = client(&rig, REQUESTER)?;
    let Rig { mock, worker, .. } = &mut rig;
    drive(worker, async {
        mock.enqueue(vec![Step::Hold]);
        let out = outbound("write").with_message_id(MessageId::generate());
        let first = send_task(&requester, &out).await?;
        let second = send_task(&requester, &out).await?;
        let third = send_task(&requester, &out).await?;
        let id = task_id(&first)?;
        if task_id(&second)? != id || task_id(&third)? != id {
            return Err("a duplicate delivery returned a different task id".to_owned());
        }
        wait_for_state(&requester, &id, pb::TaskState::Working).await?;
        expect_eq(
            count_commands(mock, |c| matches!(c, Command::Start(_))),
            1,
            "backend starts",
        )
    })
}

/// S8 (RFC-0014 [0014-019]): CancelTask on a WORKING turn stops it and commits CANCELED.
///
/// In-process substitute: the turn is a MockBackend turn; no child process exists to check and
/// the 15 s wall-clock bound was not measured.
pub(crate) fn s8_cancel_long_task() -> Result<(), String> {
    let mut rig = rig()?;
    let requester = client(&rig, REQUESTER)?;
    let Rig { mock, worker, .. } = &mut rig;
    drive(worker, async {
        mock.enqueue(vec![Step::Hold]);
        let task = send_task(&requester, &outbound("long")).await?;
        let id = task_id(&task)?;
        wait_for_state(&requester, &id, pb::TaskState::Working).await?;
        let reply = requester
            .cancel_task(&agent(WORKER)?, &id, &CallOptions::default())
            .await
            .map_err(|e| format!("cancel: {e:?}"))?;
        // The reply is the snapshot at request time; the worker commits CANCELED after the
        // backend acknowledges (RFC-0005 0005-037). Only a turn that ended some other way fails.
        let at_request = task_state(&reply.value);
        if at_request == pb::TaskState::Completed as i32
            || at_request == pb::TaskState::Failed as i32
        {
            return Err(format!("cancel reply already terminal: state {at_request}"));
        }
        wait_for_state(&requester, &id, pb::TaskState::Canceled).await?;
        expect_eq(
            count_commands(mock, |c| matches!(c, Command::Cancel(_))),
            1,
            "cancel commands",
        )
    })
}

/// S6 (RFC-0014 [0014-017]): the bus restarts while a turn is WORKING. No backend turn starts
/// twice for one admitted message, the task ends terminal (COMPLETED, or FAILED BACKEND_CRASHED),
/// and the event `seq` values are contiguous.
///
/// In-process substitute on bus-mem: `restart_bus` stands in for killing and restarting
/// nats-server, which the nats tier does not run here.
pub(crate) fn s6_bus_restart() -> Result<(), String> {
    let mut rig = rig()?;
    let requester = client(&rig, REQUESTER)?;
    rig.mock
        .enqueue(vec![Step::Hold, Step::Terminal(Outcome::Completed)]);
    let (id, seqs) = {
        let Rig { bus, worker, .. } = &mut rig;
        drive(worker, async {
            let task = send_task(&requester, &outbound("long")).await?;
            let id = task_id(&task)?;
            wait_for_state(&requester, &id, pb::TaskState::Working).await?;
            let seqs = delivered_seqs(&requester, &id).await?;
            bus.control()
                .restart_bus()
                .map_err(|e| format!("restart_bus: {e:?}"))?;
            Ok((id, seqs))
        })?
    };
    check_contiguous(&seqs)?;
    let first_starts = count_commands(&rig.mock, |c| matches!(c, Command::Start(_)));
    let mut rig = reopen(rig)?;
    let second_starts = count_commands(&rig.mock, |c| matches!(c, Command::Start(_)));
    expect_eq(
        first_starts + second_starts,
        1,
        "backend turn starts across the restart",
    )?;
    drive(&mut rig.worker, async {
        let done = until_terminal(&requester, &id).await?;
        match task_state(&done) {
            s if s == pb::TaskState::Completed as i32 => Ok(()),
            s if s == pb::TaskState::Failed as i32 => {
                expect_failure(&done, FailureCode::BackendCrashed)
            }
            s => Err(format!("task ended in state {s}, want COMPLETED or FAILED")),
        }
    })
}

/// Polls `get_task` until the task is terminal. Bounded.
async fn until_terminal(client: &Client<Bus>, id: &TaskId) -> Result<pb::Task, String> {
    for _ in 0..TERMINAL_POLLS {
        let task = client
            .get_task(&agent(WORKER)?, id, &CallOptions::default())
            .await
            .map_err(|e| format!("get_task: {e:?}"))?
            .value;
        if matches!(
            task_state(&task),
            s if s == pb::TaskState::Completed as i32
                || s == pb::TaskState::Failed as i32
                || s == pb::TaskState::Canceled as i32
        ) {
            return Ok(task);
        }
        yield_now().await;
    }
    Err(format!("task {id} never reached a terminal state"))
}

/// Subscribes from seq 0 and returns the seq of every event already delivered. Only events that
/// are ready now are read; the call never waits on the live task.
async fn delivered_seqs(client: &Client<Bus>, id: &TaskId) -> Result<Vec<u64>, String> {
    let (_, mut events) = client
        .subscribe(&agent(WORKER)?, id, Some(0), &CallOptions::default())
        .await
        .map_err(|e| format!("subscribe: {e:?}"))?;
    let mut seqs = Vec::new();
    loop {
        let mut next = pin!(events.next());
        match poll_once(next.as_mut()) {
            Poll::Ready(Ok(Some(event))) => seqs.push(event.seq),
            Poll::Ready(Ok(None)) | Poll::Pending => return Ok(seqs),
            Poll::Ready(Err(e)) => return Err(format!("event: {e:?}")),
        }
    }
}

/// Asserts the sequence is non-empty and each value is one more than the last.
fn check_contiguous(seqs: &[u64]) -> Result<(), String> {
    if seqs.is_empty() {
        return Err("no events delivered".to_owned());
    }
    for pair in seqs.windows(2) {
        if pair[1] != pair[0] + 1 {
            return Err(format!("event seq gap: {} then {}", pair[0], pair[1]));
        }
    }
    Ok(())
}

/// N1 (RFC-0014 [0014-021]): a principal without a publish grant is refused at the bus; no
/// task is created and the backend sees nothing.
///
/// In-process substitute on bus-mem; the RFC row lists the nats tier, which was not run.
pub(crate) fn n1_publish_without_grant() -> Result<(), String> {
    let mut rig = rig()?;
    let observer = client(&rig, OBSERVER)?;
    let Rig { mock, worker, .. } = &mut rig;
    drive(worker, async {
        match observer
            .send(&agent(WORKER)?, &outbound("write"), &CallOptions::default())
            .await
        {
            Err(ClientError::Bus(BusError::PermissionDenied)) => {}
            Err(other) => return Err(format!("expected PermissionDenied, got {other:?}")),
            Ok(_) => return Err("a publish without a grant was accepted".to_owned()),
        }
        expect_eq(count_commands(mock, |_| true), 0, "backend commands")
    })
}

/// N-OVERSIZE (RFC-0002 [0002-113]): a payload above the inline limit without a blob reference
/// is refused with PAYLOAD_TOO_LARGE and nothing starts.
pub(crate) fn oversize_payload_refused() -> Result<(), String> {
    let mut rig = rig()?;
    let requester = client(&rig, REQUESTER)?;
    let Rig { mock, worker, .. } = &mut rig;
    drive(worker, async {
        let big = outbound(&"x".repeat(OVERSIZE_BYTES));
        match requester
            .send(&agent(WORKER)?, &big, &CallOptions::default())
            .await
        {
            // The client refuses an oversize inline payload before it reaches the bus, so the
            // refusal is `Encode(PayloadTooLarge)`; a wire-level refusal is also accepted.
            Err(ClientError::Encode(EncodeError::PayloadTooLarge { .. }))
            | Err(ClientError::Wire(WireError::PayloadTooLarge, _)) => {}
            Err(other) => return Err(format!("expected PayloadTooLarge, got {other:?}")),
            Ok(_) => return Err("an oversize payload was accepted".to_owned()),
        }
        expect_eq(count_commands(mock, |_| true), 0, "backend commands")
    })
}

/// N-UNKNOWN-PEER (RFC-0002 [0002-093]): a request to an agent with no responder is refused
/// locally as unavailable.
pub(crate) fn unknown_peer_refused() -> Result<(), String> {
    let mut rig = rig()?;
    let requester = client(&rig, REQUESTER)?;
    let Rig { worker, .. } = &mut rig;
    drive(worker, async {
        match requester
            .send(
                &agent("ghost")?,
                &outbound("hello"),
                &CallOptions::default(),
            )
            .await
        {
            // No responder maps to UNAVAILABLE, reported locally (RFC-0002 [0002-093]).
            Err(ClientError::Local(LocalError::Unavailable)) => Ok(()),
            Err(other) => Err(format!("expected Local(Unavailable), got {other:?}")),
            Ok(_) => Err("a request to an unserved agent was accepted".to_owned()),
        }
    })
}

/// Clock advance that makes a request timestamped at 0 stale (RFC-0002 [0002-072]).
const STALE_ADVANCE_MS: u64 = 600_000;

/// N4 (RFC-0002 [0002-023]): an operation token the bus does not serve is refused with
/// UNSUPPORTED_OPERATION, and no backend turn starts.
pub(crate) fn n4_unsupported_operation() -> Result<(), String> {
    let mut rig = rig()?;
    let Rig {
        bus,
        clock,
        mock,
        worker,
        ..
    } = &mut rig;
    drive(worker, async {
        let tamper = Tamper {
            op_token: Some("frobnicate"),
            ..Tamper::default()
        };
        let reply = raw(
            bus,
            clock,
            REQUESTER,
            Op::Send,
            send_body("write", None),
            tamper,
        )
        .await?;
        match refusal(&reply)? {
            WireError::UnknownOperation => {}
            other => return Err(format!("expected UnknownOperation, got {other:?}")),
        }
        expect_eq(count_commands(mock, |_| true), 0, "backend commands")
    })
}

/// N5 (RFC-0002 [0002-034], [0002-072]): a request whose `A2AB-Ts` is outside the TTL plus skew
/// window is refused with REQUEST_EXPIRED, and no backend turn starts.
pub(crate) fn n5_stale_request() -> Result<(), String> {
    let mut rig = rig()?;
    let Rig {
        bus,
        clock,
        mock,
        worker,
        ..
    } = &mut rig;
    advance(clock, STALE_ADVANCE_MS)?;
    drive(worker, async {
        let tamper = Tamper {
            ts_ms: Some(0),
            ..Tamper::default()
        };
        let reply = raw(
            bus,
            clock,
            REQUESTER,
            Op::Send,
            send_body("write", None),
            tamper,
        )
        .await?;
        match refusal(&reply)? {
            WireError::RequestExpired => {}
            other => return Err(format!("expected RequestExpired, got {other:?}")),
        }
        expect_eq(count_commands(mock, |_| true), 0, "backend commands")
    })
}

/// N-VERSION (RFC-0002 [0002-080]): an `A2A-Version` the worker does not speak is refused with
/// VERSION_NOT_SUPPORTED, and no backend turn starts.
pub(crate) fn n_version_refused() -> Result<(), String> {
    let mut rig = rig()?;
    let Rig {
        bus,
        clock,
        mock,
        worker,
        ..
    } = &mut rig;
    drive(worker, async {
        let tamper = Tamper {
            version: Some("0.3"),
            ..Tamper::default()
        };
        let reply = raw(
            bus,
            clock,
            REQUESTER,
            Op::Send,
            send_body("write", None),
            tamper,
        )
        .await?;
        match refusal(&reply)? {
            WireError::VersionNotSupported => {}
            other => return Err(format!("expected VersionNotSupported, got {other:?}")),
        }
        expect_eq(count_commands(mock, |_| true), 0, "backend commands")
    })
}

/// N-EXTENSION (RFC-0002 [0002-081]): a worker that requires an extension refuses a request that
/// does not activate it with EXTENSION_SUPPORT_REQUIRED, and accepts the same request once the
/// extension is activated.
pub(crate) fn n_extension_required() -> Result<(), String> {
    const REQUIRED: &str = a2a_types::vocab::ext::APPROVAL;
    let mut rig = rig_with(RigSpec {
        wire: Some(envelope::Config {
            required_extensions: vec![REQUIRED.to_owned()],
            ..envelope::Config::default()
        }),
        ..RigSpec::default()
    })?;
    let Rig {
        bus,
        clock,
        mock,
        worker,
        ..
    } = &mut rig;
    drive(worker, async {
        let reply = raw(
            bus,
            clock,
            REQUESTER,
            Op::Send,
            send_body("write", None),
            Tamper::default(),
        )
        .await?;
        match refusal(&reply)? {
            WireError::ExtensionSupportRequired => {}
            other => return Err(format!("expected ExtensionSupportRequired, got {other:?}")),
        }
        expect_eq(count_commands(mock, |_| true), 0, "backend commands")?;
        let tamper = Tamper {
            extensions: vec![REQUIRED.to_owned()],
            ..Tamper::default()
        };
        let reply = raw(
            bus,
            clock,
            REQUESTER,
            Op::Send,
            send_body("write", None),
            tamper,
        )
        .await?;
        accepted(&reply).map(|_| ())
    })
}

/// N-QUEUE-FULL (RFC-0005 [0005-028]): with a waiting quota of one, a submission that would be
/// the second waiter behind a held turn in the same context is admitted to the queue, and a third
/// is REJECTED with QUEUE_FULL. Only the first turn starts.
pub(crate) fn n_queue_full_rejected() -> Result<(), String> {
    let mut rig = rig_with(RigSpec {
        max_waiting: Some(1),
        ..RigSpec::default()
    })?;
    let Rig {
        bus,
        clock,
        mock,
        worker,
        ..
    } = &mut rig;
    drive(worker, async {
        mock.enqueue(vec![Step::Hold]);
        let context = ContextId::generate();
        let context = context.as_str();
        let first = accepted(
            &raw(
                bus,
                clock,
                REQUESTER,
                Op::Send,
                send_body("hold", Some(context)),
                Tamper::default(),
            )
            .await?,
        )?;
        for _ in 0..100 {
            yield_now().await;
        }
        let second = accepted(
            &raw(
                bus,
                clock,
                REQUESTER,
                Op::Send,
                send_body("queued", Some(context)),
                Tamper::default(),
            )
            .await?,
        )?;
        let third = accepted(
            &raw(
                bus,
                clock,
                REQUESTER,
                Op::Send,
                send_body("overflow", Some(context)),
                Tamper::default(),
            )
            .await?,
        )?;
        if task_state(&first) == pb::TaskState::Rejected as i32 {
            return Err("the first submission was rejected".to_owned());
        }
        if task_state(&second) == pb::TaskState::Rejected as i32 {
            return Err("the queued submission was rejected within quota".to_owned());
        }
        if task_state(&third) != pb::TaskState::Rejected as i32 {
            return Err(format!(
                "the overflow submission was not rejected: state {}",
                task_state(&third)
            ));
        }
        expect_eq(
            count_commands(mock, |c| matches!(c, Command::Start(_))),
            1,
            "backend starts",
        )
    })
}
