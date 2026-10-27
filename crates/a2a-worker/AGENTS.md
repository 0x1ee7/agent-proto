# a2a-worker

Layer L3 worker (RFC-0001 section 4.2). Serves one agent's RPC subjects, runs tasks through a backend driver, publishes events and audit records, and delegates to peers over a local IPC endpoint (RFC-0015).

## Owns

- `backend`: the `Backend` seam for agent drivers (RFC-0009). Stable public API.
- `worker`, `rpc`, `config`, `workspace`: turn lifecycle, RPC handling, configuration, and workspace leases.
- `delegation`: the peer-call tool and delegation state (`policy`, `send`, `result`). Grants, the IPC endpoint, and the lockout clock live here.
- `presence`, `store`, `wire`, `sanitize`, `audit`: publication, the task journal adapter, wire mapping, input sanitizing, and audit notes.
- `signing`: operator-decision verification when signing is enabled (RFC-0010 [0010-079]). Opt-in through `WorkerBuilder::signing`; the replay cache lives on `Worker`.
- `mock` and `backend_contract` (feature `testkit`, also compiled under test): scriptable mock backend and event-stream contract checks.

## Provides

- `backend::{Backend, BackendSink, ChannelSink, BackendEvent, BackendError, StartTurn, Capabilities, Health, Outcome, ReasonToken, FaultKind, PermissionRequest, PermissionAnswer, TurnRef, RequestId, SessionHandle}`.
- `config::WorkerConfig` with `max_active` and `max_waiting_per_context` (per-context QUEUE_FULL quota).
- `testkit` feature: `mock::{MockBackend, MockControl, Step, Command}`, `backend_contract::{check_turn_stream, check_all_turns_closed, turns_seen, Violation}`.

## Uses

- `bus-api` (the only bus dependency; no bus implementation outside tests).
- `a2a-core`, `a2a-types`, `a2a-security`, `a2a-audit`, `a2a-workspace`, `a2a-mcp` (IPC server and grant trait) as L2 libraries.
- `sha2` (SHA-256 for the derived session workspace key in `workspace`).
- Dev only: `bus-mem`, `a2a-conformance`, tokio (tests drive peer helpers through it).

## Rules

- The crate is runtime-agnostic: it does not depend on tokio in `src`. Hosts provide timers and wakeups. The delegation clock is injected (`Clock`).
- Driver code never assigns ids, sequence numbers, or task states. The worker assigns `event_no` per turn (0009-006).
- Exactly one terminal event per turn (0009-007). Reason tokens come only from their producing protocol (0009-072).
- Per (origin, context_id), turns run one at a time in admission order; turns of different contexts run concurrently (0009-020, 0005-054).
- A completed parent turn commits COMPLETED only after its open delegated children are terminal (0015-067).
- Peer-call authentication failures are counted in a sliding window; the threshold locks out the endpoint (0015-043).
- Signing off: operator decisions over the bus are accepted pending a supervisor operator channel. RFC-0007 [0007-095] and RFC-0010 [0010-079] say that with signing off they come only from the supervisor's local operator channel (RFC-0012); RFC-0012 [0012-043] makes unsigned decisions the M1 default. This crate follows RFC-0012 [0012-043] until that channel exists. The conflict is open and is not resolved here.
- No `unsafe`. No `unwrap`, `expect`, `panic`, `todo!`, or `unimplemented!` outside tests.
- Bytes are passed without copying.
- Each requirement ID appears in the name of the test that guards it.

## Requirement IDs guarded

- 0005-028: a context over its waiting quota gets QUEUE_FULL; other contexts are unaffected. Guarded in `tests::req_0005`.
- 0005-054: contexts run concurrently and each context stays FIFO. Guarded in `tests::req_0005`.
- 0006-006: a lease context supplies path and mode. Guarded in `workspace` tests.
- 0006-034: a settled child returns its unused allowance to the parent. No defect was found; a test pins it. Guarded in `tests::req_0015`.
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
- 0010-079: with signing enabled, an operator decision must verify under an operator-role key and be relayed by `cli` or `tui`. An unsigned decision is refused with SIGNATURE_MISSING and a non-operator key with KEY_ROLE_DENIED; a refusal changes no state. Signing off is unchanged. Guarded in `tests::req_0010`.
- 0012-067: a session workspace is keyed by its context id. An opaque (non-UUID) context id maps to a derived version-8 UUID, so its workspace is leased; a canonical UUID keeps its short id. Guarded in `workspace` tests.
- 0015-043: five authentication failures in 60 s lock out the endpoint, including the correct credential. Guarded in `delegation` and `tests::req_0015` tests.
- 0015-054, 0015-058: refused and return-to-chain calls create no child. Guarded in `tests::req_0015`.
- 0015-062: a crash between the remote accept and the `sent` record does not orphan the child. The `prepared` record carries the wire message, its transport id and its `return_immediately` flag; recovery resends it with the same ids and flag, so the remote returns the child it already holds, and `sent` is written once. A send that fails is closed by `abandoned`. Guarded in `tests::req_0015`.
- 0015-067: parent stays non-terminal until its children are terminal; a deferred parent cancelled by the operator commits CANCELED at once (cancel path). Guarded in `tests::req_0015`.
- 0015-070: a failed child fails the delegating task with CHILD_FAILED. Guarded in `tests::req_0015`.
- 0015-075: each delegation outcome writes one audit note. Guarded in `tests::req_0015`.
- 0015-110 and 0015-111: discover applies the allowlist. Guarded in `tests::req_0015`.
