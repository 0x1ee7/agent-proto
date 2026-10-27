# a2ad end-to-end tests

Tests in `crates/a2ad/tests/e2e.rs` run the real `a2ad` binary. Plain tests run on every
`cargo test`. Live tests are `#[ignore]`. Each one starts a real nats-server and an omp worker.
The test starts the omp stub `spikes/omp-acp/stub/stub_openai.py` itself and points omp only at
that stub. No real model is called.

Commands run from the repo root, in bash on Windows. `CARGO_INCREMENTAL=0` is set because the disk
is tight. The default `target/` directory is used.

## Plain tests

    CARGO_INCREMENTAL=0 cargo test -p a2ad --test e2e

Last result: 7 passed, 2 ignored.

## Live tests

Environment variables (values are local paths and a public digest, no secrets):

- `A2AD_E2E_NATS_BIN`: absolute path to `nats-server.exe` (v2.15.2, Windows x64, under
  `.spikes/nats/`). A relative path fails with `nats-server ... not found (dependency_missing)`,
  because the test binary runs from `crates/a2ad`.
- `A2AD_E2E_NATS_SHA256`: lowercase SHA-256 of that file.
- `A2AD_E2E_OMP`: path to `omp.exe`.
- `A2AD_E2E_STUB_PORT`: stub port. The default is 18796. The last run used 18798.

Optional, with defaults: `A2AD_E2E_PYTHON` (`python`), `A2AD_E2E_BUS_PORT` (4292),
`A2AD_E2E_HEALTH_PORT` (9496).

Command (placeholders in angle brackets):

    A2AD_E2E_NATS_BIN=<absolute path to nats-server.exe> A2AD_E2E_NATS_SHA256=<sha256> A2AD_E2E_OMP=<omp.exe> A2AD_E2E_STUB_PORT=18798 CARGO_INCREMENTAL=0 cargo test -p a2ad --test e2e -- --ignored --test-threads=1

Last result: 2 passed, 0 failed, 7 filtered out.

- `req_0012_005_second_supervisor_on_same_state_exits_6`: ok.
- `req_0013_036_live_supervisor_reaches_readyz_and_stops_with_exit_0`: ok. `/readyz` reaches 200
  and the supervisor stops with exit 0.

Ports 4292, 9496 and 18798 had no listener after the run.

## Scratch output

Scratch output is under `target/tmp/e2e/<name>/`. Each test removes its own directory before it
starts.

## Verified fixes

- Readiness (RFC-0013 [0013-036], [0013-037]): the supervisor publishes bus, audit sink, spool fill
  and per-agent state on each tick. Covered by the `req_0013_036_*` and `req_0013_037_*` unit tests
  and by the live readyz test above.
- `a2ad status` without `run/` reports not running (RFC-0012 [0012-055]). Test:
  `req_0012_055_status_without_run_dir_reports_not_running`.
- icacls output no longer goes to stdout. It appears only inside the error when the ACL step fails
  (RFC-0012 [0012-025]). Test: `req_0012_025_icacls_output_stays_off_stdout`.
- The state layout creates `creds/`, `ws/`, `artifacts/`, `audit/` and `run/` (RFC-0012 [0012-006]).
  Test: `req_0012_006_ensure_layout_creates_every_state_dir`.

## Other findings

- `env_passthrough` accepts only `"all"` or `"none"` (`crates/a2ad/src/config.rs`). The live test
  config sets `"all"`, so the worker inherits the full environment. Keep provider keys out of the
  shell that runs the live tests.
