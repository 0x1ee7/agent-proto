# a2a-worker

Layer L3 worker (RFC-0001 section 4.2). Serves one agent's RPC subjects, runs tasks through a backend driver, and publishes events and audit records.

## Owns

- `backend`: the `Backend` seam for agent drivers (RFC-0009). Stable public API.
- `mock` and `backend_contract` (feature `testkit`, also compiled under test): scriptable mock backend and event-stream contract checks.

## Provides

- `backend::{Backend, BackendSink, ChannelSink, BackendEvent, BackendError, StartTurn, Capabilities, Health, Outcome, ReasonToken, FaultKind, PermissionRequest, PermissionAnswer, TurnRef, RequestId, SessionHandle}`.
- `testkit` feature: `mock::{MockBackend, MockControl, Step, Command}`, `backend_contract::{check_turn_stream, check_all_turns_closed, turns_seen, Violation}`.

## Uses

- `bus-api` (the only bus dependency; no bus implementation outside tests).
- `a2a-core`, `a2a-types`, `a2a-security`, `a2a-audit`, `a2a-workspace` as L2 libraries.
- Dev only: `bus-mem`, `a2a-conformance`.

## Rules

- The crate is runtime-agnostic: it does not depend on tokio. Hosts provide timers and wakeups.
- Driver code never assigns ids, sequence numbers, or task states. The worker assigns `event_no` per turn (0009-006).
- Exactly one terminal event per turn (0009-007). Reason tokens come only from their producing protocol (0009-072).
- No `unsafe`. No `unwrap`, `expect`, `panic`, `todo!`, or `unimplemented!` outside tests.
- Bytes are passed without copying.

## Requirement IDs guarded

- 0009-006: per-turn `event_no` starts at 1 and is contiguous. Guarded in `mock` and `backend_contract` tests.
- 0009-007: exactly one terminal per turn; no events after it. Guarded in `backend_contract` tests.
- 0009-008: restart drops the prior turn without a terminal. Guarded in `mock` tests.
- 0009-009: cancel is idempotent. Guarded in `mock` tests.
- 0009-010: unknown permission id is ignored. Guarded in `mock` tests.
- 0009-020: start is refused when busy or without a sink. Guarded in `mock` tests.
- 0009-033: permission answer resumes the held turn. Guarded in `mock` tests.
- 0009-036: ignored cancel leaves the turn open. Guarded in `mock` tests.
- 0009-059 and 0009-060: crash ends the open turn as BACKEND_CRASHED and closes the backend. Guarded in `mock` tests.
- 0009-072: reason tokens come from their producing protocol. Guarded in `backend` and `backend_contract` tests.
- 0009-074: mock injects faults without a turn reference. Guarded in `mock` tests.
