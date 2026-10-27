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

## Re-run 2026-10-27c: LiveGatewayRestart (S2, S3, S9, N3, N8, N9)

Stack: `.spikes/e2e/m1/m1.toml` (bus 4262 with sha256, health 127.0.0.1:9495, gateway 127.0.0.1:9930 with `loopback_anonymous = false` unless noted, omp stub 18796, Hermes UUID stub 9922). Hermes home `.spikes/e2e/m1/hermes-home`. Peer bearer read from owner-only file `.spikes/e2e/m1/secrets/peer.token` (dummy value, not a secret). Real Hermes gateway and the `a2a-spike` profile were not touched. Task store: `.spikes/e2e/m1/state/agents/omp/tasks.log` (length-prefixed frames: 4-byte big-endian length, then JSON `{"upsert":{...}}`).

Summary: S2 PASS with a stub workaround (defect D1). S3 NOT RUN. S9 PASS. N3 PASS with strict config; default config observed (not a failure). N8 PASS. N9 PASS for the in-flight task and the restart; backoff doubling NOT RUN. Superseded by Re-run 2026-10-27d: S2 (stock stub), S3, N9 backoff and 503, O5, D4, and D4 and O5 defect rows below.

| Case | Scenario | Verdict | Evidence / reason |
|---|---|---|---|
| 0014-011 (S2) | Hermes `a2a_call` to gateway, WORKING before COMPLETED | PASS (with workaround) | Command (PowerShell): `$env:HERMES_HOME=".spikes/e2e/m1/hermes-home"`, dummy `OPENAI_API_KEY` and peer env set inline, `ANTHROPIC_API_KEY` removed, then `hermes.exe -z "STUB_TOOL_CALL omp-gw s2 check ping" -t a2a`. Output: `STUB_AFTER_TOOL:` line naming omp-gw, context `4fdf0a75-cf73-4914-9195-e8916b67878c`, completed; then `OK`. Task `01a17e48-81af-73ee-9c09-58f56a009955`: state 1 at seq 1, state 2 (WORKING) seq 2-5, state 3 (COMPLETED) seq 6. WORKING-before-COMPLETED is read from tasks.log; the SSE wire was not captured separately. Workaround: `.spikes/e2e/m1/stub_hermes_uuid.py`, a copy of the shared stub that adds a UUID `context_id` to `a2a_call`. With the shared stub, tasks `01a17e46-5b70-7462-aa8d-36484b4577e7` and `01a17e46-b645-7102-bcf0-de681231deba` FAILED with `WORKSPACE_FAILED`, detail `{"token":"config"}` (D1). |
| 0014-012 (S3) | claude to Hermes via driver-a2a-http, `A2A-Version: 1.0` | NOT RUN | `spikes/claude-acp/stub_anthropic.py` has no A2A send trigger (only WRITE, READ, BASH, LONG, RECALL, REMEMBER), so no claude turn could call Hermes. Hermes server on 9921 was not started. Not a Hermes-side failure. |
| 0014-020 (S9) | a2a-sdk Python client via gateway | PASS | `.spikes/e2e/venv/Scripts/python.exe .spikes/e2e/m1/s9_sdk_client.py` (a2a-sdk 1.0.3, bearer header, `streaming=False`, `accepted_output_modes=["text/plain"]`). One event: task `01a17e49-1e90-727e-b565-1135c89f8f61`, `TASK_STATE_COMPLETED`, artifacts `reply` (OK) and `workspace-summary`, budget metadata present. Exit 0. Agent card at `/agents/omp/.well-known/agent-card.json` contains no `nats://`, `4262` or `a2abus.bus` strings. |
| 0014-023 (N3) | gateway without creds gives 401 | PASS (strict config) | `loopback_anonymous = false`. `curl -X POST http://127.0.0.1:9930/agents/omp` with no Authorization: HTTP 401, `WWW-Authenticate: Bearer`, JSON-RPC `-32050`, reason UNAUTHORIZED. Wrong bearer: same 401. tasks.log size 32284 bytes before and after both requests; omp.log and bus.log line counts unchanged (30, 151). Headers and bodies: `.spikes/e2e/m1/logs/n3/`. |
| 0008-022 (default config) | anonymous loopback, default `loopback_anonymous` | OBSERVED (not a failure) | Setting omitted (default enabled). No-header POST without `A2A-Version`: HTTP 200, JSON-RPC `-32009 VERSION_NOT_SUPPORTED`, so auth was not enforced and the request reached the protocol check. With `A2A-Version: 1.0`: HTTP 200, task `01a17e4a-0ba5-7076-87ac-3bef4a9a0915` created with `origin: loopback`, COMPLETED with artifact `reply` = OK. Matches RFC 0008-022. Logs: `.spikes/e2e/m1/logs/n3/default-noauth*`. |
| 0014-028 (N8) | supervisor killed | PASS | `taskkill /PID 59836 /F` at 16:59:04 UTC; PID 59836 verified as `a2ad.exe run --config .spikes/e2e/m1/m1.toml`. Children before the kill: nats-server 59124, gateway host 19424, omp host 18916, omp 58152. Sampled at +3 s, +10 s and +20 s: all gone. No listeners on 9930, 9495 or 4262 afterwards. Outcome observed; the containment mechanism (job object on Windows) was not inspected. |
| 0014-029 (N9) | worker crash while ready | PASS for in-flight task and restart; backoff doubling NOT RUN | `taskkill /PID 23720 /F` at 16:57:52 UTC (omp child of host 18916) while task `01a17e4b-c45b-752a-b883-4a4c17126cff` (context `66666666-7777-4888-8999-aaaaaaaaaac1`) was WORKING (state 2, seq 64). tasks.log seq 65: state 4 FAILED at 16:57:53.084 UTC, `failure.code` `BACKEND_CRASHED`, detail `BACKEND_NOT_LIVE: backend not live: agent stdout closed (exit code: 1)`. Only one task ID exists for that context: not re-run (0012-013). omp.log: `agent process ended unexpectedly` at 16:57:53.10; respawn `pid=58152` at 16:57:54.11 (about 1.0 s); health `live=true init=Ready restarts=1` at 16:57:54.37. Earlier idle kill (no task) of omp at 16:56:44 UTC: respawn at 16:56:45.9 (about 1.2 s). After that kill omp.log shows `previous presence record still live; retrying claim agent=omp` once per second from 16:56:45.99 to 16:57:10.82, then no further retries (consistent with the 30 s TTL of 0004-017; the expiry event itself is not logged). NOT verified: gateway 503 `AGENT_UNAVAILABLE` with `Retry-After` for a request during the outage (0008-049). Backoff: both restarts were 1.0 s; the doubling toward 60 s (0012-011) was not exercised. |

Observations and defects (not fixed; owners decide):

- D1: Hermes `_send_task` uses `ctx-<16 hex>` context IDs, which are not UUIDs. The workspace key parse (`crates/a2a-workspace/src/ids.rs`, `WorkKey::parse`) requires a canonical UUID, so workspace-backed workers return `WORKSPACE_FAILED` with detail token `config` (tasks `01a17e46-5b70-7462-aa8d-36484b4577e7`, `01a17e46-b645-7102-bcf0-de681231deba`). Either Hermes mints UUIDs or the gateway rejects with a clear error before submission.
- D2: a gateway peer bearer cannot come from an environment variable. The gateway child runs with `env_clear` (`crates/a2ad/src/supervisor.rs`, about line 553) and the gateway config refuses env passthrough, so the error `environment variable ... named by a secret reference is not set` is raised. Workaround used: owner-only file reference.
- D3: the owner-only credential check rejects the default inherited Windows ACL, which grants NT AUTHORITY\SYSTEM. The file had to be tightened with `icacls` to the current user only. Decide whether SYSTEM should be allowed.
- D4: the blocking `SendMessage` in the N9 run returned JSON-RPC `-32603` `DEADLINE_EXCEEDED` while the task was still WORKING. The return time was not measured, so this cannot be attributed to `blocking_wait_ms` (100000 in config).
- O5: `/readyz` on 9495 reported `"agents":{}` and `"status":"unavailable"` from 16:57:54 to 16:58:18 UTC, while driver health reported omp live at 16:57:54.37 and `a2ad.exe status` reported omp `ready`. Not diagnosed.

Commands used (all relative to the repo root):

- `target/debug/a2ad.exe run --config .spikes/e2e/m1/m1.toml` (supervisor, ready on 9930)
- `target/debug/a2ad.exe stop --config .spikes/e2e/m1/m1.toml` (stopped supervisor 54772, not hard-killed)
- `target/debug/a2ad.exe status --config .spikes/e2e/m1/m1.toml`
- `curl -s -D - -X POST http://127.0.0.1:9930/agents/omp -H "Content-Type: application/json" [-H "A2A-Version: 1.0"] [-H "Authorization: Bearer <dummy>"] -d <SendMessage JSON-RPC>`
- `.spikes/e2e/venv/Scripts/python.exe .spikes/e2e/m1/s9_sdk_client.py`
- `hermes.exe -z "STUB_TOOL_CALL omp-gw s2 check ping" -t a2a` with `HERMES_HOME=.spikes/e2e/m1/hermes-home`
- `python .spikes/e2e/m1/stub_hermes_uuid.py 9922 .spikes/e2e/m1/logs/hermes-stub.jsonl`
- `python spikes/omp-acp/stub/stub_openai.py 18796 .spikes/e2e/m1/logs`
- `taskkill /PID <pid> /F` on the specific PIDs listed above (each verified by command line first)

Pass counts: no cargo tests or clippy were run (no source changed). Scenario verdicts: PASS 4 (S2 with workaround, S9, N3 strict, N8), PASS with one sub-item NOT RUN (N9), NOT RUN 1 (S3), plus the default-config N3 observation.

Processes: started and stopped by this run: both supervisors, the omp stub (pid 57456), the Hermes UUID stub (pid 47780), the Hermes one-shot runs and the S9 client (exited on their own). Not touched: omp.exe PIDs 32988, 43184, 14360, 16684; the AuditGatewayE2E stack under `target-audit-e2e` (a2ad 12532, 23176, nats 49260, omp 57888, stub 2676); the user's Hermes gateway (pid 60820) and its hermes.exe processes. Listeners on 4262, 9495, 9930, 18796, 9922, 9921 and 18797 were checked after cleanup: none.

Nothing committed; no source changed.

## Re-run 2026-10-27d: LiveGatewayRestart Round 2 (S2, S3, N9 backoff, 503, O5, D4)

Build: `CARGO_INCREMENTAL=0 cargo build -p a2ad -p a2a-cli -p a2a-mcp` at HEAD cd76dea, exit 0 (`.spikes/e2e/m1/logs/build-r2.out`). Stack as in 27c, plus a `claude` agent in `m1.toml` (`ANTHROPIC_BASE_URL` to the claude stub on 18797) and `expose = ["omp", "claude"]` in `gateway.toml`. Stubs: stock Hermes stub `spikes/hermes-a2a/stub_openai.py` (port 9922), omp stub (18796), claude stub (18797). All times are UTC (the log `Z`). Peer bearer read from `.spikes/e2e/m1/secrets/peer.token` (not logged).

Summary: S2 PASS with the stock stub, no UUID workaround. S3 BLOCKED: the driver refuses the provider-key route, so the claude stub was never reached. N9 backoff 1, 2, 4, 8 s measured; the fifth exit marks the driver unhealthy (0009-061). The 503 `AGENT_UNAVAILABLE` (0008-049) was captured when the omp host was killed, not when only the child was killed. O5 FAIL: `/readyz` reports `omp: ready` during child restarts and after the driver is unhealthy. D4 FAIL: `blocking_wait_ms = 100000` returns `DEADLINE_EXCEEDED` at about 10 s; `5000` returns HTTP 200 WORKING at 5.04 s, as 0008-052 requires.

| Case | Scenario | Verdict | Evidence / reason |
|---|---|---|---|
| 0014-011 (S2) | Hermes `a2a_call` to gateway, stock stub | PASS | `hermes.exe -z "STUB_TOOL_CALL omp-gw s2 check ping" -t a2a` with `HERMES_HOME=.spikes/e2e/m1/hermes-home`. Output `logs/r2-s2-stock.out`: `STUB_AFTER_TOOL: [omp-gw - context ctx-ed193fe3ec034908 - completed]`, then `OK`. The gateway accepted the `ctx-<hex>` context id. No WORKSPACE_FAILED or BACKEND_CRASHED for it in state. Task id not extracted. D1 (round 1) not reproduced in this run. |
| 0014-012 (S3) | claude reached through gateway via Hermes | BLOCKED | Three attempts; the claude stub received no request (`logs/claude-stub.jsonl` is 0 bytes). (1) No provider key: task `01a17e90-d93b-71fe-8d36-551db5890f27` FAILED `BACKEND_ERROR`, detail `Authentication required`. (2) Dummy key in the supervisor process environment only: task `01a17e91-e9b7-777a-a6ad-e170e608be3a` FAILED `BACKEND_ERROR`, same detail. (3) `env_allow = ["ANTHROPIC_API_KEY"]`: driver log `agent claude: env name ANTHROPIC_API_KEY is a provider credential variable and cannot be set by the driver`; Hermes received HTTP 503 (`Error: call to 'claude-gw' failed - HTTP 503`). Config reverted to baseline. See D5. |
| 0014-029 (N9) backoff | Kill the omp child five times, measure respawn | PASS for 1, 2, 4, 8 s; fifth exit makes the driver unhealthy (0009-061) | `omp.log` kill to spawn: 18:16:27.88 to 18:16:28.90 (1.0 s); 18:16:33.42 to 18:16:35.43 (2.0 s); 18:16:39.70 to 18:16:43.71 (4.0 s); 18:16:48.25 to 18:16:56.27 (8.0 s). Fifth exit 18:17:00.67: `agent exited too often; driver unhealthy` (5 exits in 600 s; `UNHEALTHY_EXITS` in `driver-acp` core.rs). No respawn after that. The 60 s cap was not reached because the unhealthy limit stops restarts first. Script `backoff.ps1`, log `logs/backoff-r2.log`; its own spawn detection (2.2, 3.0, 5.3, 9.2 s) and its no-respawn report for kill 5 are coarse and not used as evidence. |
| 0014-029 (N9) requests during child outage | SendMessage while the child respawns | NOT AS SPECIFIED (D8) | 10 SendMessage calls in the respawn windows returned HTTP 200. Each task ended FAILED: 4 `BACKEND_CRASHED` (`BACKEND_NOT_LIVE: agent is starting`), 4 `BACKEND_ERROR` (`BACKEND_START_FAILED: a turn is already open`), 2 `BACKEND_CRASHED` (`BACKEND_START_FAILED: backend is closed`, after unhealthy). Per-task states: `logs/omp-tasks-r2.txt`. No 503 for child-only outages. |
| 0008-049 (503) | Host outage: no bus responder | PASS | Killed omp host pid 49424 at 18:20:50.927 (verified as `a2ad.exe host omp` with the m1 config, parent supervisor). t+0.3 s: `/readyz` `omp: backoff`; SendMessage HTTP 503, `Retry-After: 1`, JSON-RPC `-32603`, reason `AGENT_UNAVAILABLE`, message `agent is not available`. t+1 s: readyz `starting`, SendMessage HTTP 200 with reply `OK`. t+5 s: readyz `omp: ready`. Log `logs/host-kill-r2.log`. |
| O5 | `/readyz` during outage | FAIL | `/readyz` reported `"omp":"ready"` with `status ok` during every child-kill window, including 18:17:01.689 (driver unhealthy, no omp child running). The `backoff` state appeared only for the host kill. Round 1 `"agents":{}` not reproduced. Presence did not expire within 30 s (0014-029). |
| D4 | Blocking SendMessage vs `blocking_wait_ms` | FAIL at 100000; PASS at 5000 | Test prompt `LONG_STREAM` (80 chunks, 0.25 s apart, about 20 s). `blocking_wait_ms = 100000`: HTTP 200 with JSON-RPC `-32603` `DEADLINE_EXCEEDED` at 10.05 s; task `01a17e99-84ef-76f6-9b60-194fd46975ce` still WORKING at seq 62. `blocking_wait_ms = 5000` (supervisor restarted): HTTP 200 at 5.04 s, `TASK_STATE_WORKING` with partial text, as 0008-052 requires. Responses `logs/d4-base-resp.json`, `logs/d4-5s-resp.json`. Cause of the 10 s cap not verified; a probable one is the 10 s `REQUEST_TIMEOUT` in `crates/a2ad/src/hosts/mod.rs` (inference). Config restored to 100000. |

Defects (not fixed; owners decide):

- D5: The driver refuses `env_allow` for `ANTHROPIC_API_KEY` ("provider credential variable and cannot be set by the driver"). Round 1 read RFC 0009-069 and 0012-062 as allowing an operator to name a provider key. Without a key, claude returns `Authentication required`. This blocks S3. Which rule governs needs an owner decision.
- D6 (O5): `/readyz` and presence report omp `ready` while the driver is unhealthy with no child running, and during child restarts. This conflicts with 0014-029 (presence expires within 30 s). Sends are admitted and then fail.
- D7 (D4): A blocking wait above about 10 s returns `DEADLINE_EXCEEDED` early. This conflicts with 0008-052. Cause not verified.
- D8: During a child respawn, requests are admitted (HTTP 200) and then fail with `BACKEND_CRASHED` or `BACKEND_START_FAILED`. 0014-029 expects a refusal with the unavailable error. The 503 appears only for a host outage. Tasks sent during respawn also hit `a turn is already open`.

Commands used (repo root): `target/debug/a2ad.exe run|stop|status --config .spikes/e2e/m1/m1.toml`; `curl` against 9930 and 9495; `.spikes/e2e/m1/backoff.ps1 -Kills 7`; `.spikes/e2e/m1/host-kill.ps1 -HostPid 49424`; `.spikes/e2e/venv/Scripts/python.exe .spikes/e2e/m1/parse_tasks.py <tasks.log>`.

Pass counts: no cargo tests or clippy were run (no source changed). Live verdicts: S2 PASS; S3 BLOCKED; N9 backoff 1, 2, 4, 8 s PASS; 503 PASS (host outage only); O5 FAIL; D4 FAIL at 100000 and PASS at 5000.

Processes: started and stopped by this run: both supervisor generations, the omp, Hermes and claude stubs (service IDs m1-omp-stub, m1-hermes-stub, m1-claude-stub, all stopped), and the omp host pid 49424 killed deliberately and restored by the supervisor. Not touched: omp.exe 32988, 43184, 14360, 16684; the AuditGatewayE2E stack (a2ad 12532, 23176, nats 49260, omp 57888, stub 2676); Hermes gateway pid 60820 and the `a2a-spike` profile. Listeners on 4262, 9495, 9930, 18796, 9922, 9921 and 18797 were checked after cleanup: none.

Nothing committed; no source changed.

## Re-run 2026-10-27e: GatewayTimeouts (D7 fix, D5 route, S3 on new ports)

Build: `CARGO_INCREMENTAL=0 cargo build -p a2ad -p a2a-cli -p a2a-mcp`, exit 0 (16.6 s), after the bus-nats fix. Stack `.spikes/e2e/s3/` (gitignored scratch): bus 4272, health 9505, gateway 9940, claude stub 18806, Hermes-side endpoint `spikes/hermes-a2a/card_server.py` on 9931. Peer bearer from `.spikes/e2e/s3/secrets/peer.token` (copied from m1, owner-only ACL, not logged).

Summary: D7 FIXED, PASS live (a blocking call returns the worker's COMPLETED task at 11.64 s). D5: the driver refusal stands (0009-069); the route is the agent's own `settings.json`. S3 NOT PASS: the outbound `a2a_send` never reached a peer, because claude's session was never offered the a2abus MCP tools. That attach path is outside this worker's ownership.

| Case | Scenario | Verdict | Evidence / reason |
|---|---|---|---|
| D7 (0011-011, 0008-052) | Blocking SendMessage above 10 s | PASS live; fix in bus-nats | Before: `req_0011_011_request_honors_call_timeout_over_connection_default` (bus-nats `tests/live.rs`, connection `request_timeout` 1 s, call timeout 5 s, reply after 2 s) panics `Timeout` at `live.rs:136`, FAILED in 2.42 s. Cause: `bus.rs` `request()` used `request_with_headers`, which takes the connection-wide timeout and ignores the per-call one. Fix: `send_request` with `Request::timeout(Some(timeout))`. After: ok in 2.44 s. Round 2 (27d) before evidence: `blocking_wait_ms = 100000` returned `DEADLINE_EXCEEDED` at 10.05 s with the task WORKING. After (S3 stack, `blocking_wait_ms = 100000`, `STUB:LONG` on claude, about 10 s of output): HTTP 200 at 11.64 s, `TASK_STATE_COMPLETED`, 1690-char artifact, no `error`. Response `.spikes/e2e/s3/logs/d7-resp.json`. |

Gateway guard: `req_0008_052_blocking_wait_above_ten_seconds_returns_the_working_task` (gateway-http requirements) uses paused time, a 100 s wait and a 30 s worker. It passes both before and after, because bus-mem already honors per-call timeouts. It guards the gateway contract; it does not reproduce D7. The bus-nats live test is the failing-before test.

D5 (driver refuses `env_allow` for `ANTHROPIC_API_KEY`):

- Conflict: 0009-069 forbids the driver to read, copy, log, forward or set provider credentials; 0012-062 keeps provider keys out unless `env_allow` names them. Conservative reading: 0009-069 governs, so the driver refusal stands. Recorded as a static rule in `crates/a2ad/AGENTS.md`.
- Route: `CLAUDE_CONFIG_DIR/settings.json` = `{"permissions":{"defaultMode":"default"},"env":{"ANTHROPIC_API_KEY":"sk-stub-not-real"}}`. `s3.toml` has no `env_allow`. Same route as `crates/driver-acp/src/live.rs` lines 130-144. Claude completed turns against the stub (text replies at stub requests 1 and 2).

S3 (0014-012, claude reaches Hermes): NOT PASS.

- Trigger: `SendMessage` to `claude` through the gateway, text `STUB:A2A:hermes:S3 research question`. HTTP 200, task `01a17ea7-ee10-702e-89ae-99ed8a947f7f` WORKING, COMPLETED about 2 s later.
- Claude stub: requests 3 and 5 emitted `tool_use` named `a2a_send`. The offered `tools` in those requests (16 names, Agent through Write) contain no `mcp__a2abus__*`, so the name did not resolve to an offered tool.
- No child task in the claude store. The Hermes-side `card_server` log was never created (no POST arrived). The hermes worker `tasks.log` is 0 bytes. `approvals list --agent claude --pending` is empty.
- Shim standalone: `a2abus-mcp-shim.exe --grant ... --endpoint ... --slot <16-byte base64url>` answered `initialize` (server `a2abus`) and `tools/list` (`a2a_discover`, `a2a_send`, ...). The shim is fine. The break is attaching the a2abus MCP server to the claude session (driver-acp `acp.rs` `mcp_servers`, peer-tools path). Not edited: owned by HealthAdmission, who was messaged.

Commands used (repo root): `target/debug/a2ad.exe run|stop --config .spikes/e2e/s3/s3.toml`; `python spikes/hermes-a2a/card_server.py 9931 .spikes/e2e/s3/logs/hermes-endpoint.jsonl`; `python .spikes/e2e/m1/run_claude_stub.py 18806 .spikes/e2e/s3/stubws .spikes/e2e/s3/logs/claude-stub.jsonl`; `curl -X POST http://127.0.0.1:9940/agents/claude` with `A2A-Version: 1.0`; `target/debug/a2a.exe --bus-url nats://127.0.0.1:4272 --state-dir .spikes/e2e/s3/state --operator cli approvals list --agent claude --pending`.

Processes: started and stopped by this run: s3-supervisor (pid 14572, exit 0), s3-claude-stub, s3-hermes-endpoint (both stopped by service kill). Not touched: omp.exe 32988, 43184, 14360, 16684; AuditGatewayE2E stack (a2ad 12532, 23176; nats 49260; omp 57888; stub 2676); Hermes gateway pid 60820 and profile `a2a-spike`.

Not done: S3 PASS (blocked on the a2abus MCP attach, above). D6 and D8 are out of scope for this ticket.

Pass counts: `CARGO_INCREMENTAL=0 cargo test -p gateway-http -p a2ad -p bus-nats`: 242 passed, 0 failed, 14 ignored (11 bus-nats live tests; 3 others not run). Bus-nats live D7 test `req_0011_011_request_honors_call_timeout_over_connection_default` with `--ignored`: ok in 2.43 s. Clippy `-D warnings` on the three crates: finished with no errors. `cargo fmt -p` on the three crates: applied. Live verdicts: D7 PASS; D5 route verified only by S3 completing turns; S3 NOT PASS.

## Re-run 2026-10-27f: HealthAdmission (D9 root cause, driver-a2a-http can_start, S3 live)

Build: `CARGO_INCREMENTAL=0 cargo build -p a2ad -p a2a-cli -p a2a-mcp`, exit 0 (15.1 s), with the `can_start` change linked in. Stack `.spikes/e2e/s3/` (gitignored scratch): bus 4272, health 9505, gateway 9940, claude stub 18806, Hermes-side endpoint 9931.

Summary: D9 is NOT a driver-acp defect, and the 27e diagnosis ("claude's session was never offered the a2abus MCP tools") was wrong. The session has the tools. Claude Code defers MCP tools, so they are absent from the Messages `tools` array, and the stub model could not call a deferred tool. The defect was in the spike stub and in the evidence field `tools_offered`, which cannot show deferred MCP tools. S3 PASS live after the stub fix, with a scratch Hermes stand-in that echoes the contextId (see qualifiers).

D9 root cause (evidence):

- Repro `.spikes/e2e/s3/d9_repro.py driver`: the real `claude-agent-acp` adapter, the stub model, and the exact entry `peer_tools_entry` builds (shim path, `--grant/--endpoint/--slot`, env `A2ABUS_MCP_GRANT`). The captured `session/new` params (`.spikes/claude-acp/out/d9-driver.jsonl`) match RFC-0015 [0015-084] exactly, and the adapter returned a `sessionId`. Adapter code `acp-agent.js` lines 8698-8706 maps a stdio entry to `{type:"stdio", command, args, env}` (env array to object). The shim answered `initialize` and `tools/list` with the same args and env.
- Request 2 `tools` held 15 names, none `mcp__*` (same as 27e). Its system reminder lists `mcp__a2abus__a2a_cancel`, `a2a_discover`, `a2a_get`, `a2a_send` as deferred tools: "Their schemas are NOT loaded - calling them directly will fail with InputValidationError. Use ToolSearch with query select:<name>". `ToolSearch` and `DeferredToolPlaceholder` are in `tools`.
- With `ENABLE_TOOL_SEARCH=false` in the child env, `tools` held 32 names including the four `mcp__a2abus__*`, the stub called `mcp__a2abus__a2a_send`, and the adapter raised `session/request_permission` for that name.
- The old stub emitted the bare name `a2a_send` and resolved it by suffix against `tools` only, so deferral made the call unresolvable.

Fix: `spikes/claude-acp/stub_anthropic.py` now finds the deferred `mcp__*` names in the system reminder, calls `ToolSearch select:<full name>` first, then the full name (or calls directly when the tool is already in `tools`), continues the turn after a `ToolSearch` result, and logs `deferred` next to `tools_offered`. No driver-acp change: the entry already matches 0015-084 and is guarded by `peer_tools_entry_reaches_session_new_0015_084` and `peer_tools_entry_reaches_session_load_0015_084`; adding another driver test would not fail before any fix. Test: `spikes/claude-acp/test_stub_anthropic.py` (6 behavior tests, `python spikes/claude-acp/test_stub_anthropic.py`). Before (HEAD stub copy): `FAILED (failures=1, errors=3)` of 6 (ToolSearch-first, call-after-search, call-without-schema, log names). After: `OK`, 6 tests. The other two (tool already in `tools`; turn ends after the call result) pass before and after as guards. Repro after the fix: request 2 `ToolSearch`, request 3 `mcp__a2abus__a2a_send` with 16 tools (the schema was loaded into `tools`).

driver-a2a-http `can_start`: `HttpBackend::can_start` is now `!closed`. Before: `req_0014_029_closed_backend_cannot_start` failed at `tests/backend.rs:735` ("can_start must refuse as start does"), because the trait default `true` kept the Worker serving after `shutdown` while every `start` answers `Closed`. After: passes. `req_0014_029_unreachable_peer_leaves_backend_startable` passes before and after and guards the other side. Conservative reading, recorded in `crates/driver-a2a-http/AGENTS.md`: "unhealthy means not startable" (driver-acp) has no counterpart here. `live` goes false on an unreachable peer and returns only when a call reaches the peer, and only a turn makes that call, so mapping `!live` to "cannot start" would withdraw the RPC subscription on the first transient outage with nothing left to flip `live` back.

S3 (0014-012, claude reaches Hermes via driver-a2a-http, `A2A-Version: 1.0`): PASS live, with qualifiers.

- `/readyz` after start: claude, gateway and hermes `ready`. After a worker kill, `/readyz` showed `claude: starting` while its driver was down (D6 live).
- Gateway trigger (peer bearer, fresh `messageId`): task `01a17ec5-ea97-7616-803b-36501cf40b62` reached INPUT_REQUIRED with an approval whose summary and tool are `mcp__a2abus__a2a_send`. Stub requests: 2 `ToolSearch` (4 deferred names), 4 `mcp__a2abus__a2a_send` (schema loaded, 17 tools). Cancelled through the gateway (CANCELED). A first POST with the 27e `messageId s3-msg-1` returned the stored 27e task (a replay, not a new run); a POST without the bearer returned 401.
- That approval could not be decided from the CLI: `a2a approvals list --agent claude --pending` printed nothing and `approve` answered `not found: no open approval request`. Cause: the worker's `ListTasks` is scoped to the caller's origin (`rpc.rs` `list_tasks` calls `tasks.list(&head.origin, ...)`), so operator `cli` cannot see a task created for the gateway peer. Not fixed (see below).
- Rerun as operator `cli` (`a2a send claude "STUB:A2A:hermes:S3 research question: summarise the gateway contract"`): the CLI listed the request (tool `mcp__a2abus__a2a_send`, tier EXEC, scope once) and `approve --scope once` moved the task to WORKING.
- With the card probe `spikes/hermes-a2a/card_server.py` on 9931: one `SendMessage` arrived (`a2a-version: 1.0`, path `/`, contextId `ctx-3ff6...`, text framed `<<<A2ABUS-UNTRUSTED-BEGIN ... trust_class=TRUST_CLASS_REMOTE_PEER origin=claude>>>`). The Hermes-side child task failed `BACKEND_ERROR` `Internal: peer contextId mismatch`, because the probe answers with a random contextId and the driver refuses a foreign one (0009-046). The claude task then went WORKING to FAILED `CHILD_FAILED` with `child_code BACKEND_ERROR` 0.36 s after the send. That is exactly [0015-070], so it is conformant.
- With the scratch echo endpoint `.spikes/e2e/s3/hermes_echo_endpoint.py` (the probe's card; `SendMessage` echoes the request contextId and returns COMPLETED with artifact text `S3_PEER_REPLY`): claude task `01a17eca-0956-74a2-b24f-95086fcee81a` history SUBMITTED, WORKING, INPUT_REQUIRED, WORKING (after approve), COMPLETED (seq 8, no failure). Hermes store: child (origin `claude`) COMPLETED 109 ms after creation. `S3_PEER_REPLY` is in the JetStream stream `A2AB_EV_local`. Endpoint log `.spikes/e2e/s3/logs/hermes-endpoint.jsonl` has exactly one request.
- Stub requests 13 and 14 (another `mcp__a2abus__a2a_send` then text) arrived after the call returned. They produced no second `SendMessage` (endpoint count stayed 1), no pending approval, and no state change. Cause not established [INFERENCE: a post-turn side request that replays the conversation].

Qualifiers: the model is the stub, which obeys the ToolSearch rule by construction; real-model use of `ToolSearch` for the a2abus tools is UNVERIFIED. The peer is a scratch stand-in (card probe, then the echo endpoint), not a real Hermes server. Approval was manual (`auto_allow_peer_tools` unchanged, default false).

Findings not fixed (outside the ticket):

- `a2a approvals list|approve` cannot see or decide approvals for tasks created by a gateway peer (origin-scoped `ListTasks`). Decisions themselves resolve the owner from the task id (`rpc.rs` 447-451); only CLI discovery is origin-bound.
- `HttpBackend::shutdown` is terminal (closed flag, runtime dropped), while the Worker's `kill` calls `shutdown(Duration::ZERO)` expecting the next start to respawn (0009-036). After a worker kill an HTTP-backed agent stays closed. With `can_start` now false the Worker withdraws (requests get AGENT_UNAVAILABLE) instead of accepting turns that fail, but it never recovers without a restart.
- In the `ENABLE_TOOL_SEARCH=false` repro the script stalled after the permission allow (no endpoint existed for the shim to dial); the cause was not established.

Commands used (repo root): `python .spikes/e2e/s3/d9_repro.py driver|nosearch`; `python spikes/claude-acp/test_stub_anthropic.py`; `target/debug/a2ad.exe run|stop --config .spikes/e2e/s3/s3.toml`; `python spikes/hermes-a2a/card_server.py 9931 ...`; `python .spikes/e2e/s3/hermes_echo_endpoint.py 9931 ...`; `python .spikes/e2e/m1/run_claude_stub.py 18806 ...`; `curl -X POST http://127.0.0.1:9940/agents/claude` with `A2A-Version: 1.0`; `target/debug/a2a.exe --config .spikes/e2e/s3/s3.toml --bus-url nats://127.0.0.1:4272 --state-dir .spikes/e2e/s3/state --operator cli send|approvals list|approvals approve`.

Processes: started and stopped by this run: s3-hermes-endpoint (pid 13380), s3-claude-stub (27316), s3-supervisor (service pid 58536; `a2ad stop` reported `supervisor 8292 stopped`, exit 0), s3-hermes-echo (21440). No orphan adapter, shim or nats process; listeners on 4272, 9505, 9940, 18806, 9931 were checked after cleanup: none (TIME_WAIT sockets only). No omp.exe or other a2ad.exe was touched.

Pass counts: `cargo fmt -p driver-acp -p driver-a2a-http -p a2ad -p a2a-worker` exit 0. `CARGO_INCREMENTAL=0 cargo test -p driver-acp -p driver-a2a-http -p a2ad -p a2a-worker` exit 0: a2a-worker lib 139, a2ad lib 133 (1 ignored), a2ad `tests/e2e` 7 (2 ignored), driver-a2a-http lib 49 and `tests/backend` 24 (`tests/live` 2 ignored), driver-acp lib 71 (2 ignored): 423 passed, 0 failed, 7 ignored (live). `cargo clippy ... --all-targets -- -D warnings` exit 0. `python spikes/claude-acp/test_stub_anthropic.py`: 6 OK.
