# a2ad

Owns: the A2A-Bus supervisor (RFC-0012): configuration, the supervisor state machine, process containment, credentials, the local NATS broker lifecycle and the CLI. Modules `config`, `supervisor`, `platform`, `creds`, `broker`, `cli`, `host`, and the crate root.
Provides: the `a2ad` binary (`run`, `check-config`, `status`, `stop`, hidden `host`) and the library modules above.
Uses: `bus-nats` (provisioning and JetStream connections), `a2a-audit`, `a2a-security`, `a2a-types` (ids), `a2a-worker` and `driver-acp`/`driver-a2a-http` (worker hosting), `a2a-workspace`, `gateway-http`, `clap`, `ctrlc`, `fd-lock`, `nkeys`, `nix` (unix) and `windows-sys` (windows).

## Rules

- Unsafe code exists only in module `platform` (RFC-0012 [0012-060]): `#![deny(unsafe_code)]` at the crate root, `#[allow(unsafe_code)]` on `pub mod platform`, and a SAFETY comment on every `unsafe` block.
- Validate the whole configuration before starting anything (0012-004). Unknown keys are refused. `[[agent]] id = "bus"` is refused: the broker is configured in `[bus]` (0010-001).
- Provider keys are never read or forwarded unless listed in `env_allow` (0012-026, 0012-062). Child environment carries `A2AB_*` identity variables and paths only; seed bytes and grant credentials never appear in argv or environment (0012-027).
- One supervisor per state directory (0012-005): `run/supervisor.lock` via `fd-lock`; the pid is in `run/supervisor.pid`.
- Probes are loopback-only and send no credentials (0012-010). Worker readiness is a local ready file written by the worker host.
- Stop order: close the child's stdin (lifeline), then the graceful signal, then forced kill only after grace (0012-014, 0012-016, 0012-061). Windows uses CTRL_BREAK for workers and job-object termination for forced stop (0012-017, 0012-064).
- Restart backoff 1 s doubling to 60 s; abandon after 5 failures inside 10 min (0012-011, 0012-012).
- Exit codes follow 0012-055: 0 ok, 1 runtime, 2 usage, 3 config file missing, 4 rejected or denied, 5 stop timed out, 6 bus failure or lock held.
- Windows and Linux parity; long paths enabled when the OS allows it (0012-066).
- No `unwrap`/`expect`/`panic`/`todo!` outside tests. Requirement IDs appear in test names.

## Requirement tests

- 0010-001, 0010-002, 0010-069: broker identities, rendered configuration and binary digest (`broker`).
- 0012-002, 0012-003, 0012-004, 0012-007, 0012-010, 0012-015, 0012-022, 0012-023, 0012-026, 0012-027, 0012-030, 0012-051, 0012-060, 0012-062: configuration rules (`config`).
- 0012-005, 0012-007, 0012-010, 0012-011, 0012-012, 0012-014, 0012-015, 0012-026, 0012-027: supervisor lock, ordering, probes, restart policy, drain and env (`supervisor`).
- 0012-017, 0012-019, 0012-021, 0012-025, 0012-066: process containment, start tokens, owner-only ACLs and path length (`platform`).
- 0012-023, 0012-024, 0015-081, 0015-082: credentials and delegation file (`creds`).
- 0013-036: `health_listen` is loopback only (`config`).
- 0015-086: delegation limits (`config`).

## Live tests

- `worker_host::tests::live_worker_host_smoke` is `#[ignore]` and needs `A2AB_LIVE_CONFIG` and `A2AB_LIVE_AGENT`, plus a running bus. Run with `cargo test -p a2ad -- --ignored`.
