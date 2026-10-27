# a2ad

Owns: the A2A-Bus supervisor (RFC-0012): configuration, the supervisor state machine, process containment, credentials, the local NATS broker lifecycle and the CLI. Modules `config`, `supervisor`, `platform`, `creds`, `broker`, `cli`, `host`, and the crate root.
Provides: the `a2ad` binary (`run`, `check-config`, `status`, `stop`, hidden `host`) and the library modules above.
Uses: `bus-nats` (provisioning and JetStream connections), `a2a-audit`, `a2a-security`, `a2a-types` (ids), `a2a-worker` and `driver-acp`/`driver-a2a-http` (worker hosting), `a2a-workspace`, `gateway-http`, `clap`, `ctrlc`, `fd-lock`, `nkeys`, `nix` (unix) and `windows-sys` (windows).

## Rules

- Unsafe code exists only in module `platform` (RFC-0012 [0012-060]): `#![deny(unsafe_code)]` at the crate root, `#[allow(unsafe_code)]` on `pub mod platform`, and a SAFETY comment on every `unsafe` block.
- Validate the whole configuration before starting anything (0012-004). Unknown keys are refused. `[[agent]] id = "bus"` is refused: the broker is configured in `[bus]` (0010-001).
- Provider keys are never read or forwarded unless listed in `env_allow` (0012-026, 0012-062). Child environment carries `A2AB_*` identity variables and paths only, plus the log filter variables `A2AB_LOG` and `RUST_LOG` forwarded from the parent (0013-038). Seed bytes and grant credentials never appear in argv or environment (0012-027).
- `[log]` (optional): `level` (EnvFilter directive, default `info`) and `format` (`text` or `json`, default `text`), validated at load (0013-038, 0013-039). Child stdout/stderr logs are opened with `platform::open_log_for_append`, which shares delete on Windows so the host can rotate them.
- `[[agent]] permission_timeout` (optional, seconds, acp-* workers only): validated at load with `driver_acp::check_permission_timeout`, which requires more than the 300 s worker approval expiry (0009-033). Absent keeps `DEFAULT_PERMISSION_TIMEOUT` (330 s). Refused for other kinds and for `a2a-http`.
- One supervisor per state directory (0012-005): `run/supervisor.lock` via `fd-lock`; the pid is in `run/supervisor.pid`.
- Probes are loopback-only and send no credentials (0012-010). Worker readiness is a local ready file written by the worker host.
- Stop order: close the child's stdin (lifeline), then the graceful signal, then forced kill only after grace (0012-014, 0012-016, 0012-061). Windows uses CTRL_BREAK for workers and job-object termination for forced stop (0012-017, 0012-064).
- Restart backoff 1 s doubling to 60 s; abandon after 5 failures inside 10 min (0012-011, 0012-012).
- Exit codes follow 0012-055: 0 ok, 1 runtime, 2 usage, 3 config file missing, 4 rejected or denied, 5 stop timed out, 6 bus failure or lock held.
- Windows and Linux parity; long paths enabled when the OS allows it (0012-066).
- `icacls` output is captured, not printed; it appears only inside the error when the ACL step fails (0012-025).
- No `unwrap`/`expect`/`panic`/`todo!` outside tests. Requirement IDs appear in test names.
- Operator key (RFC-0007 [0007-006], [0007-010], [0007-020], RFC-0012 [0012-043]): every supervisor start runs `creds::ensure_operator`, which creates `<state>/creds/operator-<K>.key` (owner-only unencrypted PKCS#8, never under `config/keys`) and the allowlist entry in the trust file. The trust file defaults to `<state>/trust.toml` and is overridden by `[security] trust_file` (`Config::trust_file`); it is never in the per-user config dir, so tests and parallel runs stay isolated. Entry: agent `operator`, role `operator`, tenants `local`; relay identities `tui` and `cli` are fixed by `sender_matches_key`. Idempotent; an operator entry whose key file is missing is fatal and never regenerated. Bus seeds `creds/<identity>.nk` are separate and untouched. Children receive `A2AB_TRUST_FILE`.

## Requirement tests

- 0010-001, 0010-002, 0010-069: broker identities, rendered configuration and binary digest (`broker`).
- 0007-020, 0012-002, 0012-003, 0012-004, 0012-007, 0012-010, 0012-015, 0012-022, 0012-023, 0012-026, 0012-027, 0012-030, 0012-051, 0012-060, 0012-062: configuration rules and trust file location (`config`).
- 0009-033: `[[agent]] permission_timeout` validation and refusal rules (`config`).
- 0012-005, 0012-007, 0012-010, 0012-011, 0012-012, 0012-014, 0012-015, 0012-026, 0012-027: supervisor lock, ordering, probes, restart policy, drain and env (`supervisor`).
- 0013-036, 0013-037: readiness inputs published each tick (sink, spool, per-agent state) and the `/readyz` body (`supervisor`). Sink state means the current file opens for append; M1 has no sink consumer.
- 0012-017, 0012-019, 0012-021, 0012-025, 0012-066: process containment, start tokens, owner-only ACLs and path length (`platform`).
- 0007-006, 0007-010, 0007-014, 0012-023, 0012-024, 0012-043, 0015-081, 0015-082: operator key bootstrap, credentials and delegation file (`creds`).
- 0012-006: state layout creates `creds/`, `ws/`, `artifacts/`, `audit/`, `run/` (`creds`).
- 0013-036: `health_listen` is loopback only (`config`).
- 0012-055: `status` without `run/` reports not running (`tests/e2e.rs`).
- 0015-086: delegation limits (`config`).

## Live tests

- `worker_host::tests::live_worker_host_smoke` is `#[ignore]` and needs `A2AB_LIVE_CONFIG` and `A2AB_LIVE_AGENT`, plus a running bus. Run with `cargo test -p a2ad -- --ignored`.
- Live e2e tests (`tests/e2e.rs`, `#[ignore]`) need `A2AD_E2E_NATS_BIN`, `A2AD_E2E_NATS_SHA256`, `A2AD_E2E_OMP` and `A2AD_E2E_STUB_PORT`; run `cargo test -p a2ad --test e2e -- --ignored --test-threads=1`.

## Hosts (worker_host, gateway_host, health, hosts/)

- Worker and gateway hosts expose sync `run_*` entry points; only those touch NATS. Inner logic is generic over `Bus` and tested with bus-mem.
- Secrets are `env:`/`file:` references only; errors never echo values; `file:` requires an owner-only ACL (`platform::verify_owner_only`).
- Health is served by `health::serve` from the supervisor. Each tick the supervisor publishes bus, audit sink, spool fill and per-agent state into `HealthHandle` (0013-036, 0013-037); hosts report worker readiness via `HostControl::report_ready`.
- Delegation endpoints are per run; shim is `delegation.shim_path` or `a2abus-mcp-shim` next to the current exe (`.exe` on Windows).
- Tests are named `req_<rfc>_<id>_*`; live tests are `#[ignore]`.
