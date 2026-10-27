# a2a-worker

Layer L3 worker (RFC-0001 section 4.2). Serves one agent's RPC subjects, runs tasks through a backend driver, publishes events and audit records, and delegates to peers over a local IPC endpoint (RFC-0015).

## Owns

- `backend`: the `Backend` seam for agent drivers (RFC-0009). Stable public API.
- `worker`, `rpc`, `config`, `workspace`: turn lifecycle, RPC handling, configuration, and workspace leases.
- `delegation`: the peer-call tool and delegation state (`policy`, `send`, `result`). Grants, the IPC endpoint, and the lockout clock live here.
- `presence`, `store`, `wire`, `sanitize`, `audit`: publication, the task journal adapter, wire mapping, input sanitizing, and audit notes.
- `mock` and `backend_contract` (feature `testkit`, also compiled under test): scriptable mock backend and event-stream contract checks.

## Provides

- `backend::{Backend, BackendSink, ChannelSink, BackendEvent, BackendError, StartTurn, Capabilities, Health, Outcome, ReasonToken, FaultKind, PermissionRequest, PermissionAnswer, TurnRef, RequestId, SessionHandle}`.
- `config::WorkerConfig` with `max_active` and `max_waiting_per_context` (per-context QUEUE_FULL quota).
- `testkit` feature: `mock::{MockBackend, MockControl, Step, Command}`, `backend_contract::{check_turn_stream, check_all_turns_closed, turns_seen, Violation}`.

## Uses

- `bus-api` (the only bus dependency; no bus implementation outside tests).
- `a2a-core`, `a2a-types`, `a2a-security`, `a2a-audit`, `a2a-workspace`, `a2a-mcp` (IPC server and grant trait) as L2 libraries.
- Dev only: `bus-mem`, `a2a-conformance`, tokio (tests drive peer helpers through it).

## Rules

- The crate is runtime-agnostic: it does not depend on tokio in `src`. Hosts provide timers and wakeups. The delegation clock is injected (`Clock`).
- Driver code never assigns ids, sequence numbers, or task states. The worker assigns `event_no` per turn (0009-006).
- Exactly one terminal event per turn (0009-007). Reason tokens come only from their producing protocol (0009-072).
- Per (origin, context_id), turns run one at a time in admission order; turns of different contexts run concurrently (0009-020, 0005-054).
- A completed parent turn commits COMPLETED only after its open delegated children are terminal (0015-067).
- Peer-call authentication failures are counted in a sliding window; the threshold locks out the endpoint (0015-043).
- No `unsafe`. No `unwrap`, `expect`, `panic`, `todo!`, or `unimplemented!` outside tests.
- Bytes are passed without copying.
- Each requirement ID appears in the name of the test that guards it.

## Requirement IDs guarded

- 0005-028: a context over its waiting quota gets QUEUE_FULL; other contexts are unaffected. Guarded in `tests::req_0005`.
- 0005-054: contexts run concurrently and each context stays FIFO. Guarded in `tests::req_0005`.
- 0006-006: a lease context supplies path and mode. Guarded in `workspace` tests.
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
- 0015-043: five authentication failures in 60 s lock out the endpoint, including the correct credential. Guarded in `delegation` and `tests::req_0015` tests.
- 0015-054, 0015-058: refused and return-to-chain calls create no child. Guarded in `tests::req_0015`.
- 0015-067: parent stays non-terminal until its children are terminal. Guarded in `tests::req_0015`.
- 0015-070: a failed child fails the delegating task with CHILD_FAILED. Guarded in `tests::req_0015`.
- 0015-075: each delegation outcome writes one audit note. Guarded in `tests::req_0015`.
- 0015-110 and 0015-111: discover applies the allowlist. Guarded in `tests::req_0015`.
