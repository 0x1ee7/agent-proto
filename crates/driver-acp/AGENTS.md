# driver-acp

## Owns
- ACP stdio driver for omp and claude-agent-acp (RFC-0009, sections 4.5 to 4.10).
- Process spawn, environment allowlist, approval-mode gate, JSON-RPC framing, restart policy, and the per-driver actor thread.

## Provides
- `AcpDriver` implementing `a2a_worker::backend::Backend`.
- `AcpDriverConfig` (`new`, `validate`, `launch_spec`), `Profile`, `LaunchSpec`, `ConfigError`, `check_permission_timeout`, `WORKER_APPROVAL_EXPIRY`, and the `DEFAULT_*` timing constants.

## Uses
- `a2a-worker` `backend` module only (seam types, including `PeerTools` and `SecretText`). Its `testkit` feature (`backend_contract`) is a dev-dependency for contract checks.
- `serde_json`, `thiserror`.

## Static rules
- No `unsafe`; no `unwrap`, `expect`, or `panic!` outside tests.
- Spawn by absolute path, no shell. Child env is cleared, then an allowlist, then `LaunchSpec.env`, then operator `config.env`. Credential names are refused.
- Process-tree kill only after stdin is closed and the grace period expires.
- Approval modes come from operator config only. Dangerous modes require `acknowledge_dangerous`.
- The driver never answers a permission or elicitation except from `ResolvePermission` or its own timeout.
- Every turn ends with exactly one `Terminal`; nothing follows it.
- Live tests are `#[ignore = "live: ..."]` and run only with `--ignored`.

## Tests guarding requirement IDs
- Driver contract, offline (`tests.rs`, in-process fakes): 0009-006, 0009-007, 0009-009, 0009-010, 0009-011, 0009-012, 0009-013, 0009-014, 0009-016, 0009-017, 0009-018, 0009-019, 0009-020, 0009-021, 0009-022, 0009-023, 0009-024 (idle clock paused during approval; omp MCP-route write), 0009-025, 0009-032, 0009-033 (backstop ends the turn as BACKEND_TIMEOUT with a Fault), 0009-034, 0009-035, 0009-036, 0009-055, 0009-064, 0009-067, 0009-082, 0012-009 and 0004-015 (agent started on attach, before any turn), 0015-084, 0015-085.
- Unit tests beside code: `config.rs` (0006-017 backstop outlasts worker expiry, 0009-039, 0009-068, 0009-069, 0009-084), `mode.rs` (0009-063, 0009-064, 0009-065), `acp.rs` (0009-017, 0009-018, 0009-021, 0009-025, 0009-027, 0009-028, 0009-029, 0009-030, 0009-056, 0009-082, 0009-086, 0009-013, 0015-084, 0015-085), `redact.rs` (0009-070), `frame.rs` (0009-015), `process.rs` (0009-023 exit status and stderr tail), `core.rs` (0009-061 backoff, 0009-023 stream-end detail).
- Live (ignored, stub-backed): `live.rs` `live_omp_plain_turn_completes_0009_025`, `live_claude_plain_turn_completes_0009_025`.
- Not covered by a test in this crate: 0009-058 (worker-side), the full five-crash path of 0009-061, 0009-059/060 (shutdown sequence has no ID-named test), 0009-066 (supervisor records the card value), 0009-071, 0009-083 (omp `ask-all` refusal, enforced by `mode.rs` without a test of that name), 0009-085, 0009-087. `Health.reason` text while down and the tracing targets (`driver_acp`, `driver_acp::child`) have no assertion test (`reason` is asserted `None` while live).
