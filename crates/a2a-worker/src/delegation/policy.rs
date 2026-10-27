//! Pure checks of RFC-0015 section 4.6: the refusal-order helpers (allowlist,
//! rate windows, concurrency), budget refusal mapping, and deadline arithmetic.
//! Argument validation lives in `a2a_mcp::ipc` (`validate_*`).

use a2a_client::ClientError;
use a2a_core::budget::{DelegationRefused, UNLIMITED};
use a2a_mcp::ipc::{IpcError, MAX_DEADLINE_MS, reason};
use a2a_types::ids::{AgentId, ContextId, TaskId};

/// Width of a rate window (0015-072).
pub(crate) const WINDOW_MS: u64 = 60_000;

/// A fixed one-minute window that counts admitted events only.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Window {
    start: u64,
    count: u32,
}

impl Window {
    /// Whether one more event would be admitted at `now`. Does not count it.
    pub(crate) fn allows(&self, now: u64, limit: u32) -> bool {
        self.current(now).is_none_or(|count| count < limit)
    }

    /// Counts one admitted event at `now`.
    pub(crate) fn admit(&mut self, now: u64) {
        if now < self.start || now.saturating_sub(self.start) >= WINDOW_MS {
            self.start = now;
            self.count = 0;
        }
        self.count = self.count.saturating_add(1);
    }

    /// Admits one event when fewer than `limit` were admitted in the window.
    /// A refused event is not counted.
    pub(crate) fn try_admit(&mut self, now: u64, limit: u32) -> bool {
        if !self.allows(now, limit) {
            return false;
        }
        self.admit(now);
        true
    }

    fn current(&self, now: u64) -> Option<u32> {
        if now < self.start || now.saturating_sub(self.start) >= WINDOW_MS {
            None
        } else {
            Some(self.count)
        }
    }
}

/// Refusal (a): the target is the worker itself, unknown, or not in the allowlist.
/// Unknown and unlisted targets get the same answer, so the tools reveal no agent ids (0015-021).
pub(crate) fn check_allowlist(
    peers: &[AgentId],
    own: &AgentId,
    target: &AgentId,
) -> Result<(), IpcError> {
    if target == own || !peers.contains(target) {
        return Err(IpcError::new(reason::PERMISSION_DENIED));
    }
    Ok(())
}

/// Refusal (f): the parent may hold at most `limit` non-terminal children (0015-071).
pub(crate) fn check_inflight(open_children: u32, limit: u32) -> Result<(), IpcError> {
    if open_children >= limit {
        return Err(IpcError::new(reason::QUEUE_FULL));
    }
    Ok(())
}

/// Maps a budget refusal from `a2a_client::budget::child` to its IPC reason (0015-054 c, d).
/// Loops are returned as `LOOP_DETECTED`; the caller additionally fails the delegating
/// task (0015-057).
pub(crate) fn budget_refusal(err: &ClientError) -> IpcError {
    match err {
        ClientError::LoopDetected => IpcError::new(reason::LOOP_DETECTED),
        ClientError::Delegation(refused) => budget_exhausted(dimension_of(refused)),
        _ => budget_exhausted("hops"),
    }
}

/// `BUDGET_EXHAUSTED` with the exhausted dimension (0015-054 d).
pub(crate) fn budget_exhausted(dimension: &str) -> IpcError {
    IpcError::new(reason::BUDGET_EXHAUSTED).with_meta("dimension", dimension)
}

/// The first budget dimension whose child share is unusable (0015-055): zero leaves the parent
/// nothing to hand a child, and a sentinel wall share grants no time (0006-032). The send is
/// refused before any delegation or network traffic. Unlimited tokens and cost are valid shares.
pub(crate) fn exhausted_dimension(
    tokens: u64,
    cost_units: u64,
    wall_ms: u64,
) -> Option<&'static str> {
    if tokens == 0 {
        return Some("tokens");
    }
    if cost_units == 0 {
        return Some("cost");
    }
    if wall_ms == 0 || wall_ms == UNLIMITED {
        return Some("wall_ms");
    }
    None
}

/// The budget dimension a delegation refusal names.
pub(crate) fn dimension_of(refused: &DelegationRefused) -> &'static str {
    match refused {
        DelegationRefused::Depth => "depth",
        DelegationRefused::Children => "children",
        _ => "hops",
    }
}

/// Parses a task id from a `get` or `cancel` frame. Anything else is `TASK_NOT_FOUND`
/// (0015-023), whether or not the task exists.
pub(crate) fn parse_task_id(raw: &str) -> Result<TaskId, IpcError> {
    TaskId::parse(raw).map_err(|_| IpcError::new(reason::TASK_NOT_FOUND))
}

/// The child's `contextId` (0015-059): the delegating task's context when the model
/// gives none; any other supplied value is refused.
pub(crate) fn child_context(parent: &str, supplied: Option<&str>) -> Result<String, IpcError> {
    match supplied {
        None => Ok(parent.to_owned()),
        Some(value) if value == parent => Ok(parent.to_owned()),
        Some(_) => Err(IpcError::new(reason::MALFORMED_MESSAGE)),
    }
}

/// Parses the parent's context id, which must be valid because core assigned it.
pub(crate) fn context_id(raw: &str) -> Result<ContextId, IpcError> {
    ContextId::parse(raw).map_err(|_| IpcError::new(reason::MALFORMED_MESSAGE))
}

/// Blocking deadline for a call (0015-022, 0015-066): the requested timeout, bounded by
/// the child's wall budget. A zero wall budget is exhausted and bounds the call to zero
/// (0015-055).
pub(crate) fn call_deadline(now: u64, timeout_ms: Option<u64>, wall_ms: u64) -> u64 {
    now.saturating_add(timeout_ms.unwrap_or(MAX_DEADLINE_MS).min(wall_ms))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {

    use super::*;

    fn agent(raw: &str) -> AgentId {
        AgentId::parse(raw).expect("valid agent id in test")
    }

    /// req_0015_054_a: unknown, unlisted and own ids all get PERMISSION_DENIED.
    #[test]
    fn req_0015_054_a_allowlist_refuses_unlisted_and_self() {
        let own = agent("alpha");
        let peers = vec![agent("beta")];
        assert!(check_allowlist(&peers, &own, &agent("beta")).is_ok());
        for target in [agent("gamma"), own.clone()] {
            let err = check_allowlist(&peers, &own, &target).expect_err("refused");
            assert_eq!(err.reason, reason::PERMISSION_DENIED);
        }
    }

    /// req_0015_072: refused events are not counted, so a window can still admit later.
    #[test]
    fn req_0015_072_window_counts_only_admitted_events() {
        let mut window = Window::default();
        assert!(window.try_admit(0, 2));
        assert!(window.try_admit(1, 2));
        assert!(!window.try_admit(2, 2));
        assert!(!window.allows(3, 2));
        assert!(window.allows(WINDOW_MS, 2));
    }

    /// req_0015_071: the in-flight limit refuses at the limit with QUEUE_FULL.
    #[test]
    fn req_0015_071_inflight_limit_refuses_at_limit() {
        assert!(check_inflight(3, 4).is_ok());
        let err = check_inflight(4, 4).expect_err("full");
        assert_eq!(err.reason, reason::QUEUE_FULL);
    }

    /// req_0015_059: a child without a supplied context inherits the parent's.
    #[test]
    fn req_0015_059_child_context_defaults_to_parent() {
        assert_eq!(child_context("ctx-1", None).as_deref(), Ok("ctx-1"));
        assert!(child_context("ctx-1", Some("other")).is_err());
    }

    /// req_0015_022: the requested timeout, or the maximum request deadline when none is given,
    /// is bounded by a finite wall budget.
    #[test]
    fn req_0015_022_deadline_bounded_by_wall_budget() {
        assert_eq!(call_deadline(1_000, Some(500), 200), 1_200);
        assert_eq!(call_deadline(1_000, None, 200), 1_200);
        assert_eq!(call_deadline(u64::MAX, Some(5), 10), u64::MAX);
    }

    /// req_0006_032, req_0015_055: a zero share, or a sentinel wall share (which grants no
    /// time), refuses the send before any traffic. A sentinel token or cost share is valid.
    #[test]
    fn req_0006_032_wall_share_is_finite_and_positive() {
        assert_eq!(exhausted_dimension(1, 1, UNLIMITED), Some("wall_ms"));
        assert_eq!(exhausted_dimension(1, 1, 0), Some("wall_ms"));
        assert_eq!(exhausted_dimension(0, 1, 1), Some("tokens"));
        assert_eq!(exhausted_dimension(UNLIMITED, UNLIMITED, 1), None);
    }

    /// req_0015_055: an exhausted wall allowance (zero) bounds the call to zero.
    #[test]
    fn req_0015_055_zero_wall_budget_is_exhausted() {
        assert_eq!(call_deadline(1_000, None, 0), 1_000);
    }
}
