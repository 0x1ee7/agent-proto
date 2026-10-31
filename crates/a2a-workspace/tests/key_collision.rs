//! Short-key collisions: two canonical keys that share the 12-hex-digit directory name must never
//! share a workspace (RFC-0012 sections 4.8, 0012-031).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use a2a_workspace::{ErrorKind, ReleaseOutcome, WorkspaceMode};
use common::*;

// Same first 48 bits (the UUIDv7 millisecond timestamp), different canonical keys.
const FIRST: &str = "0190f3a2-7c1d-7e4a-9b2f-5d6e7f8a9b0c";
const SECOND: &str = "0190f3a2-7c1d-7000-8000-000000000001";

#[test]
fn req_0012_031_live_lease_is_not_reused_for_a_different_key_with_the_same_short_id() {
    let env = Env::fresh();
    env.init_repo();
    let ws = env.open();
    let first = ws
        .acquire(request("worker-1", session(FIRST), &env.source))
        .unwrap();
    let err = ws
        .acquire(request("worker-1", session(SECOND), &env.source))
        .expect_err("colliding key must not share the workspace");
    assert_eq!(err.kind(), ErrorKind::SharedCheckout);
    assert!(err.to_string().contains("0190f3a27c1d"), "{err}");
    assert_eq!(env.lock_count(), 1);
    // The original holder is unaffected and still reuses its own lease.
    let again = ws
        .acquire(request("worker-1", session(FIRST), &env.source))
        .unwrap();
    assert_eq!(again.path(), first.path());
    assert_eq!(ws.release(again).unwrap(), ReleaseOutcome::Removed);
}

#[test]
fn req_0012_031_collision_is_refused_across_scopes() {
    let env = Env::fresh();
    env.init_repo();
    let ws = env.open_with(|c| c.mode = WorkspaceMode::PerTask);
    let first = ws
        .acquire(request("worker-1", task(FIRST), &env.source))
        .unwrap();
    let err = ws
        .acquire(request("worker-1", task(SECOND), &env.source))
        .expect_err("colliding task key must be refused");
    assert_eq!(err.kind(), ErrorKind::SharedCheckout);
    assert_eq!(ws.release(first).unwrap(), ReleaseOutcome::Removed);
}
