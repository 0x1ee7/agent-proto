# a2ad end-to-end tests

Tests in `crates/a2ad/tests/e2e.rs` run the real `a2ad` binary. Plain tests run on every
`cargo test`. Live tests are `#[ignore]` and start a real nats-server and an omp worker. omp is
pointed only at the local stub `spikes/omp-acp/stub/stub_openai.py`, which the test starts itself.
No real model is called.

The commands below are the ones run for this change. The shell is bash on Windows, run from the
repo root. `CARGO_TARGET_DIR=target-audit-e2e` keeps build output out of the shared `target/`.
`CARGO_INCREMENTAL=0` was set because the disk was full: a build failed with
"There is not enough space on the disk. (os error 112)".

## Plain tests

    CARGO_TARGET_DIR=target-audit-e2e cargo test -p a2ad --test e2e

Last result: 6 passed, 2 ignored.

## Live tests

Environment variables (values are local paths and a public digest, no secrets):

- `A2AD_E2E_NATS_BIN`: path to `nats-server.exe` (v2.15.2, Windows x64, under `.spikes/nats/`).
- `A2AD_E2E_NATS_SHA256`: lowercase SHA-256 of that file. Computed with:

      certutil -hashfile <nats-server.exe> SHA256

- `A2AD_E2E_OMP`: path to `omp.exe`.
- `A2AD_E2E_STUB_PORT`: stub port. Used `18798`; the default `18796` was held by another process.

Optional: `A2AD_E2E_PYTHON` (default `python`), `A2AD_E2E_BUS_PORT` (default 4292),
`A2AD_E2E_HEALTH_PORT` (default 9496).

All live tests:

    A2AD_E2E_NATS_BIN=<nats-server.exe> A2AD_E2E_NATS_SHA256=<sha256> A2AD_E2E_OMP=<omp.exe> A2AD_E2E_STUB_PORT=18798 CARGO_TARGET_DIR=target-audit-e2e cargo test -p a2ad --test e2e -- --ignored --test-threads=1

One test (same environment), used for the lock check:

    ... cargo test -p a2ad --test e2e req_0012_005 -- --ignored --test-threads=1

Last results:

- `req_0012_005_second_supervisor_on_same_state_exits_6`: ok (second supervisor exits 6 and names the lock holder).
- `req_0013_036_live_supervisor_reaches_readyz_and_stops_with_exit_0`: FAILED with
  `/readyz was not 200 within 180s`.

### Why the readyz test fails

A manual `a2ad run` with the test's generated config (`target-audit-e2e/tmp/e2e/live-run/supervisor.toml`)
showed the health listener is up, and `/readyz` returns 503 with this body:

    {"agents":{},"bus":"connected","sink":"unavailable","status":"unavailable"}

Non-test code in `crates/a2ad/src/supervisor.rs` and `crates/a2ad/src/health.rs` sets only
`bus_connected`. It never sets `sink_writing`, never updates `spool_ratio`, and never fills the
`agents` map. `a2a-audit/src/health.rs::is_ready` therefore cannot pass. RFC 0013-036 and 0013-037
describe the intended behavior. The readiness wiring is not implemented yet, and that is outside
this change.

Diagnostic commands run against the same scratch config:

    target-audit-e2e/debug/a2ad.exe run --config target-audit-e2e/tmp/e2e/live-run/supervisor.toml
    curl -i http://127.0.0.1:9496/readyz
    curl -i http://127.0.0.1:9496/healthz
    target-audit-e2e/debug/a2ad.exe stop --config target-audit-e2e/tmp/e2e/live-run/supervisor.toml --timeout-secs 60

`/healthz` returned 200 with the same body. `stop` reported the supervisor as stopped.

## Other findings

- `a2ad status` exits 1 with `cannot read run records` when `<state>/run/` does not exist yet, which is also the case during supervisor startup.
- `a2ad run` prints `processed file: ...` lines on stdout from its icacls ACL check.
- `env_passthrough` accepts only `"all"` or `"none"`. The test config uses `"all"`, so the worker inherits the full environment. Keep provider keys out of the shell that runs the live tests.

Scratch output is under `target-audit-e2e/tmp/e2e/<name>/`. Each test recreates its own directory.
