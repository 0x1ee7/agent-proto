# E2E M1 verdicts

Scope: RFC-0014 section 4.2 catalogue (0014-010 to 0014-029). Verdicts are from
runs on an isolated stack only: config `.spikes/e2e/debug.toml`, state
`.spikes/e2e/state-dbg`, bus `nats://127.0.0.1:4232`, health `127.0.0.1:9475`,
omp stub `127.0.0.1:18766`, Claude Messages stub `127.0.0.1:18767`. No real model
calls. No real user config or credential was read or changed.

Legend: PASS = observed as specified. FAIL = observed defect (see evidence).
NOT RUN = not exercised in this pass; reason given.

## Defects found and fixed in this pass

| Defect | Evidence | Status |
|---|---|---|
| Worker-host teardown panicked `there is no reactor running` (async-nats `Subscriber` dropped outside `block_on`) | `crates/a2ad/src/worker_host.rs` `drop_in_runtime`; tests `drop_in_runtime_gives_drop_a_reactor`, `dropping_outside_runtime_panics_without_reactor` | Fixed (a2ad). `cargo test -p a2ad --lib worker_host::tests`: 17 passed, 1 ignored |
| Blocking `SendMessage` returned WORKING, ignoring `--wait` (RFC-0002 [0002-050], RFC-0005 [0005-046]) | a2a-worker `rpc.rs` | Fixed by WorkerFinish4. Verified live: CLI receives terminal state |
| Failure `detail` dropped (`detail: null`) (RFC [0005-031]) | a2a-worker | Fixed by WorkerFinish4. Verified live: `{"token":"shared_checkout"}` |
| Audit `Publish Violation` on `$JS.API.STREAM.INFO.A2A_AUDIT_local`, spool not draining | bus-nats | Fix in working tree (NatsBindingAgent). Not re-verified on a clean stack in this pass |
| Spike config: repo not git-backed under a git checkout made `git::toplevel` resolve to the outer repo, triggering `SharedCheckout` (`crates/a2a-workspace/src/workspace.rs` 139-147) | `.spikes/e2e/repo-dbg` failed with `WORKSPACE_FAILED {"token":"shared_checkout"}` | Config fix (debug config uses git-backed `.spikes/e2e/repo`). Open question for a2a-workspace owner: diagnostic is opaque |
| CLI does not derive bus port from `--config` (default 4222) | `a2a: unavailable ... authorization violation` on another agent's bus | Spike usage: pass `--bus-url nats://127.0.0.1:4232` |

## Case verdicts

| Case | Scenario | Verdict | Evidence / reason |
|---|---|---|---|
| 0014-010 (S1) | omp to claude via bus peer call (RFC-0015 [0015-022]) | FAIL | Task `01a17b19-56c3-7216-8bc3-b12930a7ead6` (repeat `01a17b16-8e21-72ca-8898-5caa5aaf52e8`): `TASK_STATE_FAILED`, `BACKEND_CRASHED`, `detail: null`. Worker log: created 620611 ms, `backend_started` true, `workspace-summary` and `BACKEND_CRASHED` both at 620914 ms, so the turn failed 303 ms after creation with `session_alive` false. omp's session file for that turn is stamped 02:03:41.202 and its prompt 02:03:41.760 (clocks agree with the worker; checked on plain hello). The model tool list has no `a2a_*` tools (stub request log: read, bash, edit, eval, glob, grep, task, wait, todo, web_search, write). The `a2abus` shim starts and answers `initialize` and `tools/list`, but its tools never reach the model. AcpDriverAgent's probe with the same entry shows the same. Plain `hello` on the same config COMPLETED (task `01a17b17-2b05-7686-9247-45ee376d0101`). Cause open: the failure is not from driver `exited()` (that path always sets a detail). Candidates: a driver Fault from `session/new` with no detail, or a worker kill |
| 0014-011 (S2) | Hermes `a2a_call` to gateway, SSE WORKING before COMPLETED | NOT RUN | Hermes gateway not started in this pass. Needs profile `.spikes/hermes/profiles/a2a-spike`, peer tokens from `.spikes/hermes/secrets/`, stub on 9912. Not started; user gateway not touched |
| 0014-012 (S3) | claude to Hermes via driver-a2a-http, `A2A-Version: 1.0` | NOT RUN | Depends on S2 gateway. Not started |
| 0014-013 (S4a) | approve: INPUT_REQUIRED, WORKING, COMPLETED, audit `decided_by` | NOT RUN | Not reached in this pass; approval flow requires WRITE_FILE turn under `ask-exec` plus `a2a approvals` |
| 0014-014 (S4b) | deny | NOT RUN | Same as S4a |
| 0014-015 (S4c) | no decision: `cancelled`, FAILED APPROVAL_TIMEOUT | NOT RUN | Same as S4a |
| 0014-016 (S5) | loop: LOOP_DETECTED | NOT RUN | Not exercised |
| 0014-017 (S6) | kill bus mid-task | NOT RUN | Not exercised |
| 0014-018 (S7) | duplicate delivery | NOT RUN | Not exercised |
| 0014-019 (S8) | cancel within 15 s | NOT RUN | Not exercised |
| 0014-020 (S9) | a2a-sdk Python client via gateway | NOT RUN | Depends on gateway |
| 0014-021 (N1) | nats | NOT RUN | Not exercised |
| 0014-022 (N2) | two workers one checkout | NOT RUN | Not exercised |
| 0014-023 (N3) | gateway without creds gives 401 | NOT RUN | Depends on gateway |
| 0014-024 (N4) | mem | NOT RUN | Not exercised |
| 0014-025 (N5) | REQUEST_EXPIRED | NOT RUN | Not exercised |
| 0014-026 (N6) | duplicate agent id | NOT RUN | Not exercised |
| 0014-027 (N7) | dangerous mode `config-refused` | NOT RUN | Config refusal for `ask-exec` on acp-claude was observed at load (`approval_mode ask-exec is not valid for backend acp-claude`); N7 proper not run |
| 0014-028 (N8) | supervisor killed | NOT RUN | Not exercised |
| 0014-029 (N9) | worker crash, BACKEND_CRASHED | NOT RUN | BACKEND_CRASHED seen on S1 but not a controlled crash test |
| Restart | supervisor restart, task store recovery | NOT RUN (partial) | Supervisor stop and start reproduced twice on the isolated stack and both workers reached READY. Task persistence across restart not checked |

## Open items

- S1 `BACKEND_CRASHED`: the turn fails 303 ms after creation, before omp receives the prompt, and the a2a tools never reach the model. Next: log the driver's `session/new` response and the Fault that sets `session_alive` false on the S1 turn, and check whether omp honors the `mcpServers` env array.
- Run run7 `TASK_STATE_FAILED` (`01a17b12`) cause: not re-verified.
- Re-test the audit fix on a clean stack.
- Hermes-dependent cases S2, S3, S9 and N3 need the gateway started with its secrets from `.spikes/hermes/secrets/`.
