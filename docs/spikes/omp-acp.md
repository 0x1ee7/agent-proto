# Spike: omp-acp (omp `--mode acp`, ACP v1)

Status: Q1 to Q6 and Q8 observed. Q3 to Q6 observed with a local scripted OpenAI-compatible stub, not a real model. Q7 FAIL (session load after restart). Real-model behaviour is `[UNOBSERVED]`.

## Scope and method

- Client: `spikes/omp-acp/probe.py` (stdlib only). Newline-delimited JSON-RPC 2.0 over stdio.
- Isolation: `PI_CODING_AGENT_DIR=<repo>/.spikes/omp-acp/agent` so omp reads the spike's own `models.yml` and `config.yml`, not `~/.omp`. Env allowlist only (RFC-0009 0009-068); no provider keys forwarded.
- Model route: `stub/stub-model` at `http://127.0.0.1:18765/v1`, served by `spikes/omp-acp/stub/stub_openai.py`. The stub answers by keyword in the last user message (`WRITE_FILE`, `READ_FILE`, `RUN_ECHO`, `RUN_PING`, `CODE_WORD`), emitting real tool-call shapes (`write`, `read`, `bash`). Results from this route are observed-with-stub.
- Launch: `omp --mode acp --cwd <work> --session-dir <sdir> --model stub/stub-model --thinking off --no-title --approval-mode <mode>` (or `--config <overlay>`).
- Client capabilities (RFC-0009 0009-018): `fs` false, `terminal` false, `auth.terminal` false, no `elicitation` unless `--elicit`.
- Repro commands: `spikes/omp-acp/README.md`. Raw results: `.spikes/omp-acp/results/*.json` (scratch, gitignored).

## Q1: initialize response

Observed (`results/init.json`, trimmed):

```
"result": {
  "protocolVersion": 1,
  "agentInfo": {"name": "omp", "title": "omp", "version": "18.12.6"},
  "authMethods": [{"id": "agent", "name": "Use existing local credentials", ...}],
  "agentCapabilities": {
    "loadSession": true,
    "mcpCapabilities": {"http": true, "sse": true},
    "promptCapabilities": {"embeddedContext": true, "image": true},
    "sessionCapabilities": {"list": {}, "fork": {}, "resume": {}, "close": {}}},
  "_meta": {"steering": {"supported": true}}
}
```

Verdict: **PASS**. omp accepts a client with no `elicitation`, no fs and no terminal.

## Q2: session/new with mcpServers

- `session/new` with `mcpServers: []` returns `sessionId`, `configOptions`, `modes`.
- `session/new` with a valid stdio entry also returns `sessionId`. The MCP server log shows `start`, `recv initialize`, `recv notifications/initialized`, `recv tools/list`.
- `session/new` with a nonexistent command also succeeds. The server never starts and nothing reports it.

Verdict: **PASS** for the accepted request shape. A failing stdio MCP server is silent at `session/new`; the bus must verify tool availability itself.

## Q3: session/prompt, update kinds, usage, stopReason

Method: `python spikes/omp-acp/probe.py prompt` (stub route, plain "say OK" prompt).

Observed: `stopReason` `end_turn`. Update kinds: `agent_message_chunk` (text `OK`), `usage_update` (`size` 32000, `used` 123), `session_info_update`. The message carries a `messageId`. Stub usage: `prompt_tokens` 123, `completion_tokens` 7.

Verdict: **PASS (observed-with-stub)**. Real-model update kinds and usage fields: `[UNOBSERVED]`.

## Q4: approval and session/request_permission

Method: `probe.py approval --mode <m> --decision <allow|reject|cancel> --via <flag|config> [--elicit]`. The stub writes `written.txt` (`WRITE_FILE`) and reads `read-me.txt` (`READ_FILE`).

Observed (`results/approval-*.json`):

| Run | Result |
| --- | --- |
| always-ask, allow, flag, no elicitation | write `failed`, text `Tool call denied by user: write`; `written.txt` not created; `permissions` 0 |
| always-ask, allow, flag, `--elicit` | probe received `elicitation/create` (unhandled, declined with -32601); write `failed` with the same denial; `permissions` 0 |
| always-ask, reject, config overlay | write denied as above |
| always-ask-cached, allow, flag | write denied; read `failed` (file read not granted) |

- Under `always-ask`, `read` ran with no prompt (`read` `completed`, `[read-me.txt#A730] 1:secret-42`) in the same mode.
- omp sent no `session/request_permission` for write or read. The generic approval prompt uses form elicitation when the client advertises `elicitation.form` (omp://approval-mode.md, ACP sessions). Without it, the call is rejected as unsupported, which matches the instant denial.
- Not observed: a successful `allow` for a write (the probe declines elicitation, so no grant could be given), `request_permission` for bash `prompt` policies, and cancel of a pending prompt.

Verdict: **PARTIAL**. The approval gate works and denies by default, but an allow was never reached. Generic write and exec under `always-ask` need either `elicitation.form` support or a `yolo` mode. Observed-with-stub.

## Q5: fs/terminal declined, omp's own tools

Method: `probe.py nofs` (`yolo`, fs and terminal declined). Prompt 1: `WRITE_FILE`. Prompt 2: `RUN_ECHO` (`bash` `echo shell-ok`).

Observed: write `completed` (`Successfully wrote 5 bytes to written.txt`); `written.txt` exists on disk; bash `$ echo hi` ran; `unhandled_client_calls` is empty, so omp never called `fs/*` or `terminal/*`. Both prompts `end_turn`.

Verdict: **PASS (observed-with-stub)**. omp uses its own file and shell tools and does not call the declined client capabilities.

## Q6: session/cancel mid-turn

Method: `probe.py cancel` (`yolo`). Prompt `RUN_PING` (`bash ping -n 60 127.0.0.1 > nul`); cancel after the tool call starts.

Observed:

- `tool_call_seen_before_cancel`: true. Process tree before cancel: `PING.EXE`, `conhost.exe`.
- Prompt result after cancel: `stopReason` `cancelled`.
- Tree after cancel and response: no live PIDs (`tree_pids_alive_after_cancel_and_response` empty).
- Follow-up `session/prompt` on the same session returned `end_turn`.

Verdict: **PASS (observed-with-stub)** for cancel during a bash tool. Cancel during model streaming (`LONG_STREAM`) was not run.

## Q7: session/load and resume after restart

Method: `probe.py persist` (one invocation, two omp processes sharing `--session-dir`). Run A: `session/new` with the MCP entry, prompt "Remember this code word for later: PINEAPPLE", `end_turn`. Run B: new process, `session/load`, `session/resume`, then prompt "CODE_WORD: what code word...".

Observed:

- Run A: `end_turn`. Session file written: `sessions/persist/2026-10-26T19-23-12-036Z_01a179aa-....jsonl` (listed before run A closed).
- Run B: `loadSession` true. `session/load` returns `-32603 "ACP session not found: 01a179aa-b124-720d-8136-02a7a5d75f94"`. `session/resume` returns the same error. Prompt after that returns `Unsupported ACP session`.
- Earlier run (no completed turn) gave the same `not found` result.

Verdict: **FAIL**. Observed: a completed turn writes a session file, and omp still reports `ACP session not found` in a new process with the same `--session-dir` and cwd. Cause `[UNVERIFIED]`. Not tested: whether the file name or path format, `--session-dir` handling, or the cwd/session index is the cause, and whether load works after a clean shutdown (run A was closed with stdin EOF; no `session/close` was sent).

## Q8: Windows process behaviour

Observed (`close` field of each run):

```
init:    exit_code 0, "exited after stdin EOF", 0.05 s, descendants before close: python.exe (MCP), conhost.exe; alive after exit: []
persist: exit_code 0, "exited after stdin EOF", 0.03 s, no descendants
cancel:  exit_code 0, "exited after stdin EOF", 0.05 s, no descendants
```

- Closing stdin exits omp with code 0 in under 0.1 s. The stdio MCP child exits with it. No leftovers in any run.
- Pre-existing user `omp.exe` processes (for example PIDs 40616 and 52820) were not touched. The probe kills only its own descendant PIDs.

Verdict: **PASS** for stdin-EOF shutdown, including with a bash child during cancel.

## RFC changes required

Proposals only (RFCs not edited):

1. RFC-0009, new requirement (ID to be assigned by the RFC owner, near 0009-016 to 0009-018): "The bus MUST NOT assume a session created by `session/new` can be loaded after a process restart. The bus MUST treat `ACP session not found` from `session/load` or `session/resume` as a fresh-session case." Basis: Q7.
2. RFC-0009, 0009-018 (client capabilities): add "A client that wants `write` or `exec` tools to run under `always-ask` MUST advertise `elicitation.form` and answer `elicitation/create`, or run omp with `yolo`." Basis: Q4 (`elicitation/create` denial).
3. RFC-0009, 0009-021 (MCP): "The bus MUST verify MCP tool availability (for example by a probe call), because omp returns success from `session/new` for an MCP server whose command does not exist." Basis: Q2.
4. RFC-0009, 0009-039 (approval mode): record the observed rule that `session/request_permission` is used only for bash `prompt` policies, and generic approvals use elicitation. Keep `[UNVERIFIED]` for the bash path.
5. RFC-0009, 0009-019: undeclared-method error code: observed `-32601` sent by the probe (the probe's answer, not omp's). omp's own error for an undeclared client call was not observed. Keep `[UNVERIFIED]`.

## Blockers

- B1 (resolved for the spike): omp's only real credential (anthropic oauth, `disabled_cause: deleted by user`) stays disabled. I did not re-enable, copy or mock it. Real-model behaviour therefore remains `[UNOBSERVED]`. To run it, the user must re-enable a credential or provide an isolated login.
- B2 (resolved for the spike route): with `PI_CODING_AGENT_DIR` set, omp reads only the spike agent dir. The default profile (`~/.omp/agent`) is not used by these runs. The `--profile` path was not tested.
- Q7 root cause open (see Q7).

## Unobserved items

- [UNOBSERVED] Real-model Q3 update kinds, usage fields and stopReason values.
- [UNOBSERVED] Q4 successful allow through `elicitation/create` (the probe declines it); bash `prompt` policy through `session/request_permission`; `always-ask-cached` read grant.
- [UNOBSERVED] Q6 cancel during model streaming (`LONG_STREAM` not run).
- [UNOBSERVED] Q7 replay updates and MCP reconnect on a successful load; load after a clean `session/close`.
- [UNOBSERVED] Error code omp returns for an undeclared client method (0009-019).
- [UNOBSERVED] Real-model tool-call behaviour (any tool calls other than the stub's scripted ones).
