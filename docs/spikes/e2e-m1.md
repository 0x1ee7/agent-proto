# E2E M1 verdicts

Scope: RFC-0014 section 4.2 catalogue (0014-010 to 0014-029). Verdicts are from
runs on an isolated stack only: config `.spikes/e2e/debug.toml`, state
`.spikes/e2e/state-dbg`, bus `nats://127.0.0.1:4232`, health `127.0.0.1:9475`,
omp stub `127.0.0.1:18766`, Claude Messages stub `127.0.0.1:18767`. No real model
calls. No real user config or credential was read or changed.

Legend: PASS = observed as specified. FAIL = observed defect (see evidence).
NOT RUN = not exercised in this pass; reason given.

## Pass on `.spikes/e2e/s1debug.toml` (ports 4252 / 9485 / 18777 / 18786)

Stack: `.spikes/e2e/s1debug.toml`, state `.spikes/e2e/state-s1`, built with `CARGO_TARGET_DIR=target-s1final3` (the shared `target/` binaries were locked by LiveE2EAgent). Stubs: omp `spikes/omp-acp/stub/stub_openai.py 18786`, claude `.spikes/e2e/claude_stub.py 18777`. Supervisor, stubs and nats-server were stopped at the end; no listeners remain on the four ports.

Stub-only exception (decided by Main): the claude child needs an auth key to pass the adapter's login check. The fake key `sk-stub-not-real` and `ANTHROPIC_BASE_URL=http://127.0.0.1:18777` are written to `.spikes/e2e/claude-cfg/settings.json` under `env`. That file is throwaway and lives inside `.spikes/e2e`. No real credential was read or written. The supervisor and driver env path stays key-free, and the driver still refuses provider key names in `[[agent]] env`.

Defects found and fixed or observed in this pass:

| Defect | Evidence | Status |
|---|---|---|
| Claude `ask-all` rejected by the driver mapping: `mode::canonical` had no `(Claude, "ask-all")` entry, while a2ad accepts it for `acp-claude`. `ensure_process` then returned silently, so no ACP child spawned | `crates/driver-acp/src/mode.rs` line 16; regression `claude_ask_all_config_starts_the_agent` | Fixed (AcpDriverAgent). Silent refusal fixed: reason now in `health().reason` |
| Permission request ids were `perm-N` with a counter reset per process. The persisted task store refused the restarted host's request with `request id already exists` | `crates/driver-acp/src/core.rs` `next_pending_id`; now `perm-<epoch>-<n>`. Observed live: `perm-1793073600661953500-1` to `-4` across the run | Fixed. No dedicated regression test yet |
| Restart cascade: a crashed host's presence record survives its death, so the restarted host got `served by another live instance` and the supervisor logged `AGENT_ABANDONED` after 5 failures | `.spikes/e2e/state-s1/logs/supervisor.jsonl` (earlier runs) | Fix landed (SupervisorHosts): claim retried every 1 s for up to 45 s. Runtime path not yet exercised: check omp log for `previous presence record still live; retrying claim` |
| S4c: approval expiry ends the task `BACKEND_CRASHED` with null detail, not `APPROVAL_TIMEOUT` | See S4c below | Open |
| `a2a send` without `--wait` blocks until the task is terminal, so a cancel must come from a separate process | Timing in S8 below | Observation (CLI) |
| `README.md` task fixture: `.spikes/e2e/repo` has `README.txt` only | Not changed | Open |
| `a2a watch` returns `wire: UnsupportedOperation` | Not investigated | Open (CLI) |
| `approvals list` fails when any configured agent is unavailable | `crates/a2a-cli/src/approvals.rs` ~103-128. Not re-checked | Open (CLI) |

Case results:

| Case | Verdict | Evidence |
|---|---|---|
| Main task `A2A_SEND:claude:review README.md` (0014-010, child hops == 1) | PASS | `a2a send omp "A2A_SEND:claude:review README.md" --wait` returned `TASK_STATE_INPUT_REQUIRED` for the write-to-xd-route approval `perm-1793073600661953500-1`. `a2a approvals approve <id> --scope once --yes` moved the task to WORKING. Parent task `01a17b84-3f61-72aa-aaf2-5e8478e3d0eb` reached state 3 (COMPLETED). Child claude task `01a17b84-5025-7622-a5c6-992131e4b7f8` reached state 3. omp `tasks.log` ledger: `hops 1, children 1` |
| S4a (0014-013) write approve | PASS | `a2a send omp "WRITE_FILE" --wait` returned INPUT_REQUIRED. Approval `perm-1793073600661953500-2` approved with allow_once. Task `01a17b85-01f6-7502-b07a-7a4a5f6f70b4` reached COMPLETED. `ws/local/omp/01a17b8501f6/written.txt` exists |
| S4b (0014-014) write deny | PASS | Approval `perm-1793073600661953500-3` denied with reject_once. Task `01a17b85-6582-70d6-9f4a-d6231447fe80` reached COMPLETED with no failure. `ws/local/omp/01a17b856582/` has no `written.txt`. Matches RFC 0014 "COMPLETED without the write if the turn succeeds" |
| S4c (0014-015) no decision | SUPERSEDED 2026-10-27 by the re-run below (was FAIL: BACKEND_CRASHED, detail null, 04:07:29) | Approval `perm-1793073600661953500-4` (expires 04:07:29.274). Task `01a17b86-1a76-758e-b236-897360332a52` ended state 4 at 04:07:29.281 with `failure.code = BACKEND_CRASHED`, detail null. Expected `APPROVAL_TIMEOUT` and `cancelled` to the backend. No `written.txt`. Cause not yet confirmed [INFERENCE]: the driver's own 300 s `permission_timeout` and the worker's approval expiry fire at the same instant, and the driver path wins with a null-detail crash. `commit_expired` only maps to `APPROVAL_TIMEOUT` when the worker's expiry runs first |
| S8 (0014-019) cancel LONG_STREAM | SUPERSEDED 2026-10-27 by the re-run below (earlier PASS had no process check) | Task `01a17b8d-b91c-7496-a2cc-6347f592feec`. `a2a cancel` at 04:10:53.52 returned WORKING. CANCELED (state 5) committed at `at_ms 1793074253688`, about 0.2 s after the request. "No child process remains" is NOT VERIFIED: omp is a persistent host, and no tree check was done |

Earlier S8 attempts were invalid. The first ran while S4c's approval still held omp's turn, so it failed with `BACKEND_START_FAILED: a turn is already open`. A later attempt cancelled after the stream had already completed (`TaskNotCancelable`).

## Re-run 2026-10-27: S4c and S8 (isolated stack `.spikes/e2e/s1debug.toml`)

Stack: bus 4252, health 9485, omp stub 18786, claude stub 18777. Supervisor PID 44628 (first run), 47864 (second run, used for S8). Started by this run: stubs, supervisor, nats-server, a2ad hosts, omp.exe and node.exe children. No config change; the approval expiry is the fixed worker value (300 s), so the full wait was used. Stop: `a2ad.exe stop --config .spikes/e2e/s1debug.toml` reported `supervisor 47864 stopped`.

| Case | Verdict | Evidence |
|---|---|---|
| S4c (0014-015) no decision | PASS | Task `01a17e1f-492c-7342-8985-e17eb60401c1` sent `A2A_SEND:claude:review README.md` (`send --wait` returned INPUT_REQUIRED at 17:09:03 local). Approval `perm-1793117316775074300-1` (tool `write`, tier EXEC, scope once) was never decided. `approvals list` showed `expires at 2026-10-27T16:14:03.136+00:00`. In `agents/omp/tasks.log` the approval is `state: Expired`, `expires_ms 1793117643136`. Task went to state 4 (FAILED) with `failure: {"code":"APPROVAL_TIMEOUT","detail":"APPROVAL_TIMEOUT: approval window expired"}` and `terminal_ms 1793117643145` (9 ms after expiry). Time to terminal: task created 1793117341996, terminal 1793117643145, = 301.1 s. Detail is non-null. The supervisor log has no BACKEND_CRASHED for this task |
| S8 (0014-019) cancel LONG_STREAM | PASS | Task `01a17e27-3a39-77e2-835b-f5edf48cc700`, sent `LONG_STREAM` to omp. `a2a cancel <id> --agent omp` issued 16:17:46.50 local, returned `TASK_STATE_WORKING` at 16:17:46.64 (exit 0). CANCELED (state 5) committed at `at_ms 1793117866747`, about 0.25 s after the request. The send exited with `TASK_STATE_CANCELED` at 16:17:46.96. Process check: omp.exe PID 4604 (child of this run's a2ad host 49864) and node.exe PID 768 (child of claude host 60756) were alive before stop and gone after `a2ad stop` (`Get-Process -Id`). The omp host keeps a persistent omp.exe between tasks, so the check is made at stop |

Anomaly (not a verdict change): after the S4c timeout, the omp backend stayed closed. The next task `01a17e24-ccae-748e-b090-2f71d292dbd1` (LONG_STREAM, sent 16:15:02 local, before the restart) failed in 1.7 s with `failure: {"code":"BACKEND_CRASHED","detail":"BACKEND_START_FAILED: backend is closed"}`. At that point the omp a2ad host (PID 40584) was alive but its omp.exe child (PID 34080) was gone, and no `agent process exited` line was logged. Cause [INFERENCE]: the approval-expiry path stops the ACP session and the omp host does not respawn the child. S8 ran only after a supervisor restart. Evidence: `.spikes/e2e/state-s1/logs/omp.log` has the `spawned program=omp.exe pid=34080` line at 16:08:36 and no later spawn or exit line; `tasks.log` holds the failure record above.

Invalid attempts in this re-run (not verdicts): (1) the first S8 cancel used an id with a trailing newline and was rejected with a usage error; (2) a later S8 attempt cancelled after the stream had completed (about 20 s), returning `TASK_STATE_COMPLETED`.

Test status: `cargo build -p a2ad -p a2a-cli -p a2a-mcp` passes. `cargo test -p driver-acp`: 67 passed, 0 failed, 2 ignored on the last run (AcpDriverAgent also fixed the `peer_tools_with_operator_servers_refused_0015_084` race and the silent refusal). Checks for Main: `cargo test -p a2ad -p a2a-worker -p a2a-cli`.



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
| 0014-015 (S4c) | no decision: `cancelled`, FAILED APPROVAL_TIMEOUT | PASS (re-run 2026-10-27) | Task `01a17e1f-492c-7342-8985-e17eb60401c1`: FAILED, `APPROVAL_TIMEOUT`, detail non-null, 301.1 s to terminal. See "Re-run 2026-10-27" |
| 0014-016 (S5) | loop: LOOP_DETECTED | NOT RUN | Not exercised |
| 0014-017 (S6) | kill bus mid-task | NOT RUN | Not exercised |
| 0014-018 (S7) | duplicate delivery | NOT RUN | Not exercised |
| 0014-019 (S8) | cancel within 15 s | PASS (re-run 2026-10-27) | CANCELED about 0.25 s after cancel; omp.exe and node.exe children gone after `a2ad stop`. See "Re-run 2026-10-27" |
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

## Re-run 2026-10-27b: backend respawn after expiry

Stack: `.spikes/e2e/s1debug.toml`, bus 4252, health 9485, omp stub 18786 (PID 42220), claude stub 18777 (PID 59516), supervisor PID 61032 (single run, no restart between steps). Started by this run: both stubs, supervisor, nats-server and the a2ad hosts. Stopped with `a2ad.exe stop` (`supervisor 61032 stopped`), then stubs killed by PID.

| Step | Verdict | Evidence |
|---|---|---|
| S4c approval expiry | PASS | Task `01a17e31-31f4-72b2-bacc-c0e9d2bc3d04` (`A2A_SEND:claude:review README.md`) reached INPUT_REQUIRED at 16:28:36 UTC; approval never decided. `tasks.log`: state 4 FAILED, `failure: {"code":"APPROVAL_TIMEOUT","detail":"APPROVAL_TIMEOUT: approval window expired"}`, `at_ms 1793118816534` (16:33:36.534 UTC). Detail non-null. |
| Respawn after expiry | PASS | `omp.log`: at 16:33:36.839 health `live=false ... reason=agent process killed by the worker`. At 16:33:57.921 `agent process spawned program=omp.exe pid=9132`, and at 16:33:58.199 health `live=true init=Ready`. |
| Plain turn on same supervisor | PASS | `a2a send omp "hello" --wait` at 16:33:59 UTC, no restart. Task `01a17e36-1a91-7572-a730-6f70d356bf6e`: `TASK_STATE_COMPLETED`, artifact `reply` = `OK`, no failure field (none present). Not BACKEND_CRASHED / BACKEND_START_FAILED. |

Process check (omp.exe PIDs not owned by this run, left untouched): 32988, 43184, 14360, 16684, present before and after; not killed.
This run's omp child 18152 (spawned 16:28:04) was absent before the second task. This run's respawned child 9132 (spawned 16:33:57) was alive after the second task (`tasklist`) and absent after `a2ad stop` (`tasklist /FI "PID eq 9132"`: no tasks). No `agent process exited` line was logged for either child.

Cleanup: no listeners on 4252, 9485, 18777, 18786 after stop and stub kill. Nothing committed; no source changed.
