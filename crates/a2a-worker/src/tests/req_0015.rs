//! RFC-0015 delegation scenarios: a parent and a child Worker on one bus-mem bus.

use a2a_mcp::client::{CallOutcome, ClientConfig};
use a2a_mcp::ipc::{MAX_RESULT_TEXT_BYTES, Secret, Wait, reason};

use super::peer::*;
use super::*;
use crate::backend::{Outcome, ReasonToken, UsageSample, UsageSource};

fn usage(tokens_in: u64, tokens_out: u64) -> Step {
    Step::Usage(UsageSample {
        source: UsageSource::DriverCounted,
        turns: None,
        wall_ms: None,
        tokens_in: Some(tokens_in),
        tokens_out: Some(tokens_out),
        cost: None,
        gauges: None,
    })
}

/// S1: the parent delegates to the child; both complete, share the context, and provenance
/// names the child (0015-054 accepted path, 0015-060).
#[test]
fn req_0015_054_delegation_round_trip_shares_context() {
    let mut m = mesh(Spec::default());
    let h = m.handles();
    m.child_mock.enqueue(vec![
        text("child-answer"),
        Step::Terminal(Outcome::Completed),
    ]);
    m.parent_mock
        .enqueue(vec![Step::Hold, Step::Terminal(Outcome::Completed)]);
    drive_mesh(&mut m, async move {
        let parent = submit_to(&h.bus, PARENT, "delegate").await;
        let tools = peer_tools(&h.parent).await;
        let mut call = ipc_call(&tools, send_frame(CHILD, "compute", "k1", Wait::Blocking));
        let result = expect_result(finished(&mut call).await);
        assert_eq!(result.state, "COMPLETED");
        assert!(
            result.text.contains("child-answer"),
            "framed text carries the answer"
        );
        assert!(
            result.text.contains(CHILD),
            "framed text names the source agent (0015-035)"
        );
        assert_eq!(
            result.context_id, parent.context_id,
            "contexts must be shared"
        );
        assert_eq!(result.provenance.source_agent, CHILD);
        assert_eq!(
            result.provenance.delegating_task_id.as_deref(),
            Some(parent.id.as_str())
        );
        assert_eq!(result.provenance.chain_length, 1);
        h.parent.release();
        let done = until_state(&h.bus, PARENT, &parent.id, pb::TaskState::Completed).await;
        assert_eq!(state(&done), pb::TaskState::Completed as i32);
    });
}

/// 0015-032: a child answer of 200000 bytes comes back at most 65536 bytes, truncated on a
/// UTF-8 boundary, with `omittedBytes` counting exactly the bytes left out.
#[test]
fn req_0015_032_oversized_child_text_is_truncated() {
    let chunk = "\u{e9}"; // two bytes in UTF-8
    let answer = chunk.repeat(100_000);
    let mut m = mesh(Spec::default());
    let h = m.handles();
    m.child_mock
        .enqueue(vec![text(&answer), Step::Terminal(Outcome::Completed)]);
    m.parent_mock
        .enqueue(vec![Step::Hold, Step::Terminal(Outcome::Completed)]);
    drive_mesh(&mut m, async move {
        let parent = submit_to(&h.bus, PARENT, "big").await;
        let tools = peer_tools(&h.parent).await;
        let mut call = ipc_call(
            &tools,
            send_frame(CHILD, "compute", "k-big", Wait::Blocking),
        );
        let result = expect_result(finished(&mut call).await);
        assert_eq!(result.state, "COMPLETED");
        assert!(result.truncated, "a 200000-byte answer is truncated");
        assert!(
            result.text.len() <= MAX_RESULT_TEXT_BYTES,
            "text stays within the limit"
        );
        let kept = result.text.matches(chunk).count() * chunk.len();
        assert!(kept > 0, "the kept prefix is not empty");
        assert_eq!(result.omitted_bytes, (answer.len() - kept) as u64);
        h.parent.release();
        until_state(&h.bus, PARENT, &parent.id, pb::TaskState::Completed).await;
    });
}

/// 0015-054 (a) before (d): a non-allowlisted target is refused with PERMISSION_DENIED even
/// when the budget is also exhausted; allowlist precedes budget.
#[test]
fn req_0015_054_allowlist_refusal_precedes_budget() {
    let mut m = mesh(Spec {
        parent_budget: Allowance {
            max_children: 0,
            ..Allowance::default()
        },
        ..Spec::default()
    });
    let h = m.handles();
    m.parent_mock
        .enqueue(vec![Step::Hold, Step::Terminal(Outcome::Completed)]);
    drive_mesh(&mut m, async move {
        let parent = submit_to(&h.bus, PARENT, "delegate").await;
        let tools = peer_tools(&h.parent).await;
        let mut outsider = ipc_call(&tools, send_frame(OUTSIDER, "x", "k1", Wait::Blocking));
        assert_eq!(
            refusal(finished(&mut outsider).await),
            reason::PERMISSION_DENIED
        );
        let mut over = ipc_call(&tools, send_frame(CHILD, "x", "k2", Wait::Blocking));
        assert_eq!(refusal(finished(&mut over).await), reason::BUDGET_EXHAUSTED);
        h.parent.release();
        until_state(&h.bus, PARENT, &parent.id, pb::TaskState::Completed).await;
    });
}

/// 0015-057 / 0006-045 (re-entrancy): A delegates to B; B calls back to A. A's admission sees its
/// own agent id in the incoming chain, so the new task fails LOOP_DETECTED at once, unqueued
/// (per-context serialization would otherwise deadlock until the deadline). The refusal fails B.
#[test]
fn req_0015_057_loop_a_b_a_is_detected() {
    let mut m = mesh(Spec::default());
    let h = m.handles();
    m.parent_mock.enqueue(vec![Step::Hold]);
    m.child_mock.enqueue(vec![Step::Hold]);
    drive_mesh(&mut m, async move {
        let _parent = submit_to(&h.bus, PARENT, "start-loop").await;
        let parent_tools = peer_tools(&h.parent).await;
        let mut to_child = ipc_call(&parent_tools, send_frame(CHILD, "b", "k1", Wait::Blocking));
        let child_tools = peer_tools(&h.child).await;
        let mut back = ipc_call(&child_tools, send_frame(PARENT, "a", "k2", Wait::Blocking));
        assert_eq!(refusal(finished(&mut back).await), reason::LOOP_DETECTED);
        finished(&mut to_child).await;
    });
}

/// 0015-040, 0015-048: a repeated idempotency key replays the first child, not a new one.
#[test]
fn req_0015_048_idempotency_key_replays_the_same_child() {
    let mut m = mesh(Spec::default());
    let h = m.handles();
    m.child_mock
        .enqueue(vec![text("once"), Step::Terminal(Outcome::Completed)]);
    m.parent_mock
        .enqueue(vec![Step::Hold, Step::Terminal(Outcome::Completed)]);
    drive_mesh(&mut m, async move {
        let parent = submit_to(&h.bus, PARENT, "idem").await;
        let tools = peer_tools(&h.parent).await;
        let mut first = ipc_call(&tools, send_frame(CHILD, "x", "same", Wait::Blocking));
        let one = expect_result(finished(&mut first).await);
        let mut second = ipc_call(&tools, send_frame(CHILD, "x", "same", Wait::Blocking));
        let two = expect_result(finished(&mut second).await);
        assert_eq!(one.task_id, two.task_id);
        let starts = h
            .child
            .commands()
            .iter()
            .filter(|c| matches!(c, Command::Start(_)))
            .count();
        assert_eq!(starts, 1, "the child must be created once");
        h.parent.release();
        until_state(&h.bus, PARENT, &parent.id, pb::TaskState::Completed).await;
    });
}

/// 0015-046: cancelling the call stops waiting for the child; the child is not cancelled.
#[test]
fn req_0015_046_cancel_call_stops_waiting_without_cancelling_child() {
    let mut m = mesh(Spec::default());
    let h = m.handles();
    m.child_mock.enqueue(vec![Step::Hold]);
    m.parent_mock
        .enqueue(vec![Step::Hold, Step::Terminal(Outcome::Completed)]);
    drive_mesh(&mut m, async move {
        let parent = submit_to(&h.bus, PARENT, "cancel-call").await;
        let tools = peer_tools(&h.parent).await;
        let mut call = ipc_call(&tools, send_frame(CHILD, "long", "k1", Wait::Blocking));
        peer_tools(&h.child).await;
        call.cancel();
        assert!(matches!(finished(&mut call).await, CallOutcome::Cancelled));
        assert!(
            !cancel_seen(&h.child),
            "cancel-call must not cancel the child"
        );
        h.parent.release();
        until_state(&h.bus, PARENT, &parent.id, pb::TaskState::Completed).await;
    });
}

/// 0015-068: cancelling the parent cancels its running child.
#[test]
fn req_0015_068_parent_cancel_propagates_to_child() {
    let mut m = mesh(Spec::default());
    let h = m.handles();
    m.child_mock.enqueue(vec![Step::Hold]);
    m.parent_mock.enqueue(vec![Step::Hold]);
    drive_mesh(&mut m, async move {
        let parent = submit_to(&h.bus, PARENT, "propagate").await;
        let tools = peer_tools(&h.parent).await;
        let _call = ipc_call(&tools, send_frame(CHILD, "long", "k1", Wait::Blocking));
        peer_tools(&h.child).await;
        cancel_on(&h.bus, PARENT, &parent.id).await;
        while !cancel_seen(&h.child) {
            yield_now().await;
        }
    });
}

/// 0015-065: a child in INPUT_REQUIRED passes through; the parent task stays WORKING.
#[test]
fn req_0015_065_child_input_required_passes_through() {
    let mut m = mesh(Spec::default());
    let h = m.handles();
    m.child_mock
        .enqueue(vec![Step::InputRequired("need detail".to_owned())]);
    m.parent_mock
        .enqueue(vec![Step::Hold, Step::Terminal(Outcome::Completed)]);
    drive_mesh(&mut m, async move {
        let parent = submit_to(&h.bus, PARENT, "ask").await;
        let tools = peer_tools(&h.parent).await;
        let frame = send_frame_within(CHILD, "q", "k1", Wait::Blocking, Some(60_000));
        let mut call = ipc_call(&tools, frame);
        let result = expect_result(finished(&mut call).await);
        assert_eq!(result.state, "INPUT_REQUIRED");
        let parent_now = get_from(&h.bus, PARENT, &parent.id).await;
        assert_eq!(state(&parent_now), pb::TaskState::Working as i32);
        h.parent.release();
        until_state(&h.bus, PARENT, &parent.id, pb::TaskState::Completed).await;
    });
}

/// 0015-070: a FAILED child answers the call as isError with the child's code as reason, and
/// the delegating task commits FAILED with CHILD_FAILED (RFC-0006 0006-049).
#[test]
fn req_0015_070_child_failure_is_reported_as_failed() {
    let mut m = mesh(Spec::default());
    let h = m.handles();
    m.child_mock.enqueue(vec![Step::Terminal(Outcome::Failed {
        reason: ReasonToken::BackendError,
        health_class: None,
        detail: "child broke".to_owned(),
    })]);
    m.parent_mock
        .enqueue(vec![Step::Hold, Step::Terminal(Outcome::Completed)]);
    drive_mesh(&mut m, async move {
        let parent = submit_to(&h.bus, PARENT, "fail").await;
        let tools = peer_tools(&h.parent).await;
        let mut call = ipc_call(&tools, send_frame(CHILD, "x", "k1", Wait::Blocking));
        assert_eq!(refusal(finished(&mut call).await), "BACKEND_ERROR");
        let failed = until_state(&h.bus, PARENT, &parent.id, pb::TaskState::Failed).await;
        assert_eq!(failure_code(&failed).as_deref(), Some("CHILD_FAILED"));
    });
}

/// 0015-029 path for settle_unsent: an unreachable peer fails the call as UNAVAILABLE, and the
/// parent task fails with CHILD_FAILED (0015-070, settle_unsent).
#[test]
fn req_0015_070_unsent_child_fails_parent_with_child_failed() {
    let mut m = mesh(Spec {
        parent_peers: vec![GHOST],
        ..Spec::default()
    });
    let h = m.handles();
    m.parent_mock.enqueue(vec![Step::Hold]);
    drive_mesh(&mut m, async move {
        let parent = submit_to(&h.bus, PARENT, "unsent").await;
        let tools = peer_tools(&h.parent).await;
        let mut call = ipc_call(&tools, send_frame(GHOST, "x", "k1", Wait::Blocking));
        assert_eq!(refusal(finished(&mut call).await), reason::UNAVAILABLE);
        let failed = until_state(&h.bus, PARENT, &parent.id, pb::TaskState::Failed).await;
        assert_eq!(failure_code(&failed).as_deref(), Some("CHILD_FAILED"));
    });
}

/// 0015-055 / 0006-041: the child's terminal usage is charged to the parent, so a later send
/// that would exceed the parent's token allowance is refused BUDGET_EXHAUSTED.
#[test]
fn req_0015_055_child_usage_is_charged_from_terminal_metadata() {
    let mut m = mesh(Spec {
        parent_budget: Allowance {
            max_tokens: 100,
            ..Allowance::default()
        },
        ..Spec::default()
    });
    let h = m.handles();
    m.child_mock.enqueue(vec![
        usage(30, 20),
        text("first"),
        Step::Terminal(Outcome::Completed),
    ]);
    m.child_mock.enqueue(vec![
        usage(30, 20),
        text("second"),
        Step::Terminal(Outcome::Completed),
    ]);
    m.parent_mock
        .enqueue(vec![Step::Hold, Step::Terminal(Outcome::Completed)]);
    drive_mesh(&mut m, async move {
        let parent = submit_to(&h.bus, PARENT, "spend").await;
        let tools = peer_tools(&h.parent).await;
        // 50 of the parent's 100 tokens are charged from the child's terminal usage.
        let mut first = ipc_call(&tools, send_frame(CHILD, "x", "k1", Wait::Blocking));
        expect_result(finished(&mut first).await);
        // The child's share is the remaining 50 tokens; spending all of it exhausts the child.
        let mut second = ipc_call(&tools, send_frame(CHILD, "y", "k2", Wait::Blocking));
        assert_eq!(refusal(finished(&mut second).await), reason::CHILD_FAILED);
        // Spending the parent's allowance ends the parent, so its grant and slot are gone (0015-073).
        let mut third = ipc_call(&tools, send_frame(CHILD, "z", "k3", Wait::Blocking));
        let err = refusal(finished(&mut third).await);
        assert!(
            err == reason::GRANT_REVOKED || err == reason::SLOT_UNBOUND,
            "{err}"
        );
        until_terminal(&h.bus, PARENT, &parent.id).await;
    });
}

/// 0015-022 / 0015-055 / 0006-033: a child's wall share is the parent's remaining wall less the
/// margin. A parent whose wall equals the margin derives a zero share; the zero-share refusal is
/// pinned by `delegation::policy`. At mesh level the parent ends at its wall limit, so the send
/// is refused by teardown (0015-073) rather than hanging.
#[test]
fn req_0015_055_zero_wall_allowance_refuses_send() {
    let mut m = mesh(Spec {
        parent_budget: Allowance {
            max_wall_ms: a2a_core::budget::WALL_MARGIN_MS,
            ..Allowance::default()
        },
        ..Spec::default()
    });
    let h = m.handles();
    m.parent_mock
        .enqueue(vec![Step::Hold, Step::Terminal(Outcome::Completed)]);
    drive_mesh(&mut m, async move {
        let parent = submit_to(&h.bus, PARENT, "wall").await;
        let tools = peer_tools(&h.parent).await;
        let mut call = ipc_call(&tools, send_frame(CHILD, "x", "k1", Wait::Blocking));
        let err = refusal(finished(&mut call).await);
        assert!(
            err == reason::BUDGET_EXHAUSTED
                || err == reason::SLOT_UNBOUND
                || err == reason::GRANT_REVOKED,
            "{err}"
        );
        until_terminal(&h.bus, PARENT, &parent.id).await;
    });
}

/// 0015-073 / 0015-074: once the parent is terminal its grant and slot are revoked.
#[test]
fn req_0015_073_teardown_revokes_grant_after_parent_terminal() {
    let mut m = mesh(Spec::default());
    let h = m.handles();
    m.child_mock
        .enqueue(vec![text("done"), Step::Terminal(Outcome::Completed)]);
    m.parent_mock
        .enqueue(vec![Step::Hold, Step::Terminal(Outcome::Completed)]);
    drive_mesh(&mut m, async move {
        let parent = submit_to(&h.bus, PARENT, "teardown").await;
        let tools = peer_tools(&h.parent).await;
        let mut call = ipc_call(&tools, send_frame(CHILD, "x", "k1", Wait::Blocking));
        expect_result(finished(&mut call).await);
        h.parent.release();
        until_state(&h.bus, PARENT, &parent.id, pb::TaskState::Completed).await;
        let mut late = ipc_call(&tools, send_frame(CHILD, "y", "k2", Wait::Blocking));
        let err = refusal(finished(&mut late).await);
        assert!(
            err == reason::GRANT_REVOKED || err == reason::SLOT_UNBOUND,
            "unexpected refusal {err}"
        );
    });
}

/// 0015-043: a wrong grant credential is refused with UNAUTHENTICATED.
#[test]
fn req_0015_043_wrong_credential_is_unauthenticated() {
    let mut m = mesh(Spec::default());
    let h = m.handles();
    m.parent_mock
        .enqueue(vec![Step::Hold, Step::Terminal(Outcome::Completed)]);
    drive_mesh(&mut m, async move {
        let parent = submit_to(&h.bus, PARENT, "auth").await;
        let tools = peer_tools(&h.parent).await;
        let config = ClientConfig {
            endpoint: tools.endpoint.to_string_lossy().into_owned(),
            grant_id: tools.grant_id.clone(),
            credential: Some(Secret::new("not-the-credential".to_owned())),
            session_slot: tools.slot.clone(),
        };
        let mut call = ipc_call_with(config, send_frame(CHILD, "x", "k1", Wait::Blocking));
        assert_eq!(refusal(finished(&mut call).await), reason::UNAUTHENTICATED);
        h.parent.release();
        until_state(&h.bus, PARENT, &parent.id, pb::TaskState::Completed).await;
    });
}

/// 0015-060: a call whose task is WORKING cannot see its own provenance as trusted, and
/// the child answer is labelled with its source agent.
#[test]
fn req_0015_060_result_provenance_names_the_source_agent() {
    let mut m = mesh(Spec::default());
    let h = m.handles();
    m.child_mock
        .enqueue(vec![text("ans"), Step::Terminal(Outcome::Completed)]);
    m.parent_mock
        .enqueue(vec![Step::Hold, Step::Terminal(Outcome::Completed)]);
    drive_mesh(&mut m, async move {
        let parent = submit_to(&h.bus, PARENT, "prov").await;
        let tools = peer_tools(&h.parent).await;
        let mut call = ipc_call(&tools, send_frame(CHILD, "x", "k1", Wait::Blocking));
        let result = expect_result(finished(&mut call).await);
        assert_eq!(result.provenance.source_agent, CHILD);
        h.parent.release();
        until_state(&h.bus, PARENT, &parent.id, pb::TaskState::Completed).await;
    });
}
