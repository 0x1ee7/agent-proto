# Spike: omp-acp (omp `--mode acp`, ACP v1)

Status: Q1 to Q8 answered. Q3 to Q7 observed with a local scripted OpenAI-compatible stub, not a real model; every such result is labelled observed-with-stub. Real-model behaviour is `[UNOBSERVED]`.

## Scope and method

- Client: `spikes/omp-acp/probe.py` (stdlib only). Newline-delimited JSON-RPC 2.0 over stdio. Summaries: `spikes/omp-acp/summarize.py`. Repro: `spikes/omp-acp/README.md`.
- Isolation: `PI_CODING_AGENT_DIR=<repo>/.spikes/omp-acp/agent` so omp reads the spike's own `models.yml` and `config.yml`, not `~/.omp`. Env allowlist only (RFC-0009 0009-068); no provider keys forwarded. The stub provider uses a dummy literal key (`stub-local-key`), not a secret.
- Model route: `stub/stub-model` at `http://127.0.0.1:18765/v1` (`spikes/omp-acp/stub/stub_openai.py`). The stub answers by keyword in the last user message (`WRITE_FILE`, `READ_FILE`, `RUN_ECHO`, `RUN_PING`, `CODE_WORD`) and always emits `echo hi` for `RUN_ECHO`. It emits real tool-call shapes (`write`, `read`, `bash`).
- Launch: `omp --mode acp --cwd <work> --session-dir <sdir> --model stub/stub-model --thinking off --no-title --approval-mode <mode>`. Restart and resume variants omit `--session-dir` where noted.
- Client capabilities (RFC-0009 0009-018): `fs` false, `terminal` false, `auth.terminal` false. `elicitation.form` advertised only in the `elicit` scenario.
- Raw results: `.spikes/omp-acp/results/*.json` (scratch, gitignored). Transcripts: `.spikes/omp-acp/transcripts/`.

## Q1: initialize response

Observed (`results/init.json`, trimmed):

```
"result": {
  "protocolVersion": 1,
  "agentInfo": {"name": "omp", "title": "omp", "version": "18.12.6"},
  "agentCapabilities": {
    "loadSession": true,
    "mcpCapabilities": {"http": true, "sse": true},
    "sessionCapabilities": {"list": {}, "fork": {}, "resume": {}, "close": {}}},
  "_meta": {"steering": {"supported": true}}
}
```

Verdict: **PASS**. omp accepts a client with no `elicitation`, no fs and no terminal.

## Q2: session/new with mcpServers

- `mcpServers: []` returns `sessionId`, `configOptions`, `modes`.
- A valid stdio entry also succeeds. The MCP server log shows `start`, `recv initialize`, `recv notifications/initialized`, `recv tools/list`.
- A nonexistent command also succeeds. The server never starts and nothing reports it.

Verdict: **PASS**. A failing stdio MCP server is silent at `session/new`; the bus must verify tool availability itself.

## Q3: session/prompt, update kinds, usage, stopReason

Method: `probe.py prompt` (stub route).

Observed: `stopReason` `end_turn`. Update kinds: `agent_message_chunk` (text `OK`), `usage_update` (`size` 32000, `used` 123), `session_info_update`.

Verdict: **PASS (observed-with-stub)**. Real-model update kinds and usage fields: `[UNOBSERVED]`.

## Q4: approval gate, elicitation shapes, response effects, cache keying

Method: `probe.py elicit --mode <m> --answer <a> [--value <label>] [--perm <allow|reject|cancel>]`. Each run does: write1 (`WRITE_FILE`), read1 (`READ_FILE`), bash1 (`RUN_ECHO`), write2, read2. The client advertises `elicitation.form`; the policy answers `elicitation/create` per `--answer`. `session/request_permission` answers per `--perm` (default `allow_once`).

### Which tools prompt, and by which channel

| `--approval-mode` | write | read | bash (`echo hi`) |
| --- | --- | --- | --- |
| always-ask | `elicitation/create` (every call, unless "for this session" granted) | no prompt, runs | `session/request_permission` |
| always-ask-cached | `elicitation/create` (until cached) | `elicitation/create` (until cached) | `session/request_permission` |
| write | no prompt, runs | no prompt, runs | `session/request_permission` |
| yolo | no prompt | no prompt | no prompt |

Observed: `results/elicit-write-accept-Approve.json` (`write` mode: no elicitation, bash permission); `results/elicit-yolo-accept-Approve.json` (no elicitation, no permission). So `session/request_permission` is sent for bash in every mode except `yolo`.

### elicitation/create request shape (always-ask, write)

Observed verbatim (`results/elicit-always-ask-accept-Approve.json`):

```
{"mode": "form", "sessionId": "01a179d1-92d9-702a-91dd-2f58b6ae49a5",
 "message": "Allow tool: write\nPath: written.txt\nContent:\nhello",
 "requestedSchema": {"type": "object",
   "properties": {"value": {"type": "string", "enum": ["Approve", "Approve write for this session", "Deny"]}},
   "required": ["value"]}}
```

- No `toolCallId` and no tool name field. Correlation is by the `message` text (`Allow tool: <tool>` on the first line) and by the JSON-RPC request `id`.
- The message may also contain `Reason: target not yet approved` and a folder listing (seen for cached mode).

### elicitation/create request shape (always-ask-cached)

Observed (`results/elicit-always-ask-cached-accept-Approve_File.json`):

```
{"mode": "form", ..., "message": "Allow tool: write\nReason: target not yet approved\nPath: written.txt\nContent:\nhello\nFolder: ~/dev/agent-proto/.spikes/omp-acp/work/<name> - 1 entry (0 folders, 1 file)\n  - read-me.txt",
 "requestedSchema": {"type": "object", "properties": {"value": {"type": "string", "enum": [
   "Approve Directory: ~/dev/agent-proto/.spikes/omp-acp/work/<name>", "Approve File", "Always Approve Directory: ...", "Always Approve File", "Approve Once", "Deny"]}}, "required": ["value"]}}
```

- The read prompt uses `Allow tool: read\nReason: target not yet approved\nFolder: ... - 2 entries (0 folders, 2 files)`.
- Paths in the enum and message are shown with `~/` (home-relative display), not absolute paths.

### Response shapes and resulting tool output

| Elicitation answer | Result (observed) |
| --- | --- |
| `{"action":"accept","content":{"value":"Approve"}}` (always-ask) | write runs: `[written.txt#77F9] Successfully wrote 5 bytes to written.txt` (`completed`) |
| same, value `Approve write for this session` (always-ask) | one prompt; the later write runs with no prompt (`results/elicit-always-ask-accept-Approve_write_for_this_session.json`) |
| `{"action":"accept","content":{"value":"Approve"}}` (always-ask-cached) | NOT in enum: write and read `failed` with `Tool call denied by user: write` / `...: read` |
| `{"action":"accept","content":{"value":"Approve Once"}}` (cached) | every call prompts again; all tools run |
| `{"action":"accept","content":{"value":"Approve File"}}` (cached) | write and read each prompt once; the repeat calls run with no prompt |
| `{"action":"accept","content":{"value":"Always Approve File"}}` (cached) | same as Approve File in the observed run |
| `{"action":"decline"}` | `failed`, text `Tool call denied by user: write`; turn continues; later read runs |
| `{"action":"cancel"}` (no `session/cancel`) | `failed`, text `Tool call denied by user: write`; turn continues |
| `{"action":"cancel"}` after `session/cancel` (notify) | prompt `stopReason` `cancelled`; no tool output; session stays usable (later read runs) |
| no answer, `session/cancel` sent (notify-only) | prompt `stopReason` `cancelled`; NEXT prompt fails `ACP cancel cleanup timed out`, then `Unsupported ACP session` for every later call (see Q5) |

Probe-side note: the `accept` value must be an exact enum label. Any other label is treated as a denial, not an error.

### session/request_permission for bash

Observed shape (`results/elicit-always-ask-accept-Approve.json`, trimmed):

```
{"sessionId": "...", "toolCall": {"toolCallId": "call_stub_1", "title": "echo hi", "kind": "execute",
  "status": "pending", "rawInput": {"command": "echo hi"}, ...},
 "options": [{"optionId": "allow_once", "kind": "allow_once"}, {"optionId": "allow_always", "kind": "allow_always"},
             {"optionId": "reject_once", "kind": "reject_once"}, {"optionId": "reject_always", "kind": "reject_always"}]}
```

- `allow_once` (`selected`): bash runs; `tool_call_update` `completed`, text `$ echo hi`, then `hi`.
- `reject_once` (`selected`): bash `failed`, text `Tool call rejected by user (bash)` (`results/elicit-always-ask-accept-Approve-perm-reject.json`).
- `cancelled` outcome: bash `failed`, text `Permission request cancelled` (`results/elicit-always-ask-accept-Approve-perm-cancel.json`).

### Cached keying

- Observed: the grant is per tool name (write and read each prompt once). Repeats of the same write and read in the same cwd run without a prompt after `Approve File` or `Always Approve File`.
- Not observed: whether a grant keys on the exact file path, the folder (`Approve Directory`), or only the tool. Not tested: a different file path in the same folder, and `Approve Directory`. `[UNOBSERVED]`.
- `Approve Once` is the only option that prompts every time.

Verdict: **PASS (observed-with-stub)** for the approval gate, elicitation shapes, response effects, and bash permission. Real-model prompt behaviour and cache keying beyond tool name: `[UNOBSERVED]`.

## Q5: cancel while an elicitation is pending

Method: `probe.py elicit --answer <a>` with `--mode always-ask`.

Observed (`results/elicit-always-ask-notify-only.json`, `results/elicit-always-ask-cancel-notify.json`):

- Cancel with the elicitation answered `{"action":"cancel"}` after `session/cancel`: write `stopReason` `cancelled`. Read and later calls run normally (`end_turn`). This is the expected answer: the client MUST answer the pending elicitation (cancel) and send `session/cancel`.
- Cancel with the elicitation left UNANSWERED after `session/cancel`: write `stopReason` `cancelled`, but the next prompt fails with `ACP cancel cleanup timed out` and then `Unsupported ACP session` for every later prompt. The session is unusable.
- Cancel with the elicitation UNANSWERED and no `session/cancel` (cancel-notify-silent): no reply in 40 s; the probe timed out. What omp does after 40 s is `[UNOBSERVED]`.

Verdict: **PASS (observed-with-stub)** for the cancel answer shape and final `stopReason` `cancelled`. An unanswered pending elicitation at cancel breaks the session (FAIL for that case).

## Q6: session/cancel mid-turn (bash)

Method: `probe.py cancel` (`yolo`). Prompt `RUN_PING` (`bash ping -n 60 127.0.0.1 > nul`); cancel after the tool call starts.

Observed:

- `tool_call_seen_before_cancel`: true. Process tree before cancel: `PING.EXE`, `conhost.exe`.
- Prompt result after cancel: `stopReason` `cancelled`.
- Tree after cancel and response: no live PIDs.
- Follow-up `session/prompt` on the same session returned `end_turn`.

Verdict: **PASS (observed-with-stub)** for cancel during a bash tool. Cancel during model streaming (`LONG_STREAM`) was not run.

## Q7: session/load and session/resume after restart

Method: `probe.py resume` (restart variants; one omp process A, then process B). Prompt in A: "Remember this code word for later: PINEAPPLE". B: `session/list` (optional), `session/load`, `session/resume`, prompt "what code word".

Results (`results/resume-*.json`, `summarize.py`):

| Variant | Load | Resume | Prompt after |
| --- | --- | --- | --- |
| same process, no restart (`resume --same-process`) | ok | ok | `end_turn` |
| restart, `--session-dir`, same cwd, no close (`persist`/`resume`) | `ACP session not found` | `ACP session not found` | `Unsupported ACP session` |
| restart, `--session-dir`, same cwd, `session/close` in A | `ACP session not found` | `ACP session not found` | `Unsupported ACP session` |
| restart, `--session-dir`, same cwd, `session/list` in B | `list` returns `{"sessions": []}`; load `ACP session not found` | same | `Unsupported ACP session` |
| restart, `--session-dir`, DIFFERENT cwd in B | `ACP session not found` (persist, other cwd) | same | `Unsupported ACP session` |
| restart, default session dir (no `--session-dir`), same cwd, no close | ok | ok | `end_turn` |
| restart, default session dir, same cwd, `session/close` in A | ok | ok | `end_turn` |
| restart, default session dir, same cwd, `session/list` in B | list shows the session; load ok | ok | `end_turn` |
| restart, default session dir, DIFFERENT cwd in B | `ACP session load was cancelled` | same | `Unsupported ACP session` |
| restart, different `PI_CODING_AGENT_DIR` (`--agent-dir-b`), default dir | `ACP session not found` | same | `Unsupported ACP session` |

Observed details:

- With `--session-dir`, omp writes the file flat in that directory (`sessions/resume-.../<timestamp>_<id>.jsonl`). `session/list` and `session/load` never find it.
- With the default session dir, omp writes `<PI_CODING_AGENT_DIR>/sessions/<encoded-cwd>/<timestamp>_<id>.jsonl`, where `<encoded-cwd>` is e.g. `-dev-agent-proto-.spikes-omp-acp-work-<name>`. Load and list find it.
- The session header records the cwd as given, for example `"cwd":"C:\\Users\\0x1de\\dev\\agent-proto\\.spikes\\omp-acp\\work\\<name>"`.
- Load with a different cwd is cancelled: `ACP session load was cancelled: <id>`. Cause `[UNOBSERVED]` (the transcript shows no other message).

Root cause (observed, not from source): load and list read only omp's default sessions location under `PI_CODING_AGENT_DIR`. `--session-dir` is not used for lookup, so files written there cannot be loaded after a restart. Same-process load works because the session is held in memory.

Verdict: **PARTIAL**. Load and resume after a process restart WORK, under these conditions: (1) no `--session-dir` (default session location), (2) the same `PI_CODING_AGENT_DIR`, (3) the same cwd. With `--session-dir` (the original Q7 setup) they FAIL with `ACP session not found`. Closing the session first is not required.

## Q8: Windows process behaviour

Observed (`close` field of each run):

```
init:    exit_code 0, "exited after stdin EOF", 0.05 s, descendants before close: python.exe (MCP), conhost.exe; alive after exit: []
persist: exit_code 0, "exited after stdin EOF", 0.03 s, no descendants
cancel:  exit_code 0, "exited after stdin EOF", 0.05 s, no descendants
```

- Closing stdin exits omp with code 0 in under 0.1 s. The stdio MCP child exits with it. No leftovers.
- Pre-existing user `omp.exe` processes were not touched. The probe kills only its own descendant PIDs.

Verdict: **PASS** for stdin-EOF shutdown, including with a bash child during cancel.

## RFC changes required

Proposals only (RFCs not edited):

1. RFC-0009, 0009-016 to 0009-018 (session lifecycle): add "The bus MUST NOT pass `--session-dir` to omp when it needs to load or resume sessions. The bus MUST give each omp instance its own `PI_CODING_AGENT_DIR` and MUST reuse the same agent dir and the same cwd to load a session after restart." Basis: Q7.
2. RFC-0009, new requirement (ID to be assigned by the RFC owner): "The bus MUST treat `ACP session not found` or `ACP session load was cancelled` from `session/load` or `session/resume` as a fresh-session case." Basis: Q7.
3. RFC-0009, 0009-018 (client capabilities): "A client that needs `write` or `exec` tools to run under `always-ask` or `always-ask-cached` MUST advertise `elicitation.form` and answer `elicitation/create` with an exact enum label (`accept` with `content.value`), or run omp in `write` or `yolo` mode." Basis: Q4.
4. RFC-0009, 0009-039 (approval): record that bash gates through `session/request_permission` (`allow_once`, `reject_once`, `cancelled`) in every mode except `yolo`, and write/read gate through `elicitation/create` in always-ask modes. Keep `[UNOBSERVED]` for real models.
5. RFC-0009, cancel requirement (near 0009-019 or the cancel section): "On `session/cancel` while an `elicitation/create` is pending, the client MUST answer it with `{"action":"cancel"}`. An unanswered pending elicitation leaves the session unusable (`ACP cancel cleanup timed out`, then `Unsupported ACP session`)." Basis: Q5.
6. RFC-0009, 0009-021 (MCP): "The bus MUST verify MCP tool availability (for example by a probe call), because omp returns success from `session/new` for an MCP server whose command does not exist." Basis: Q2.
7. RFC-0009, 0009-019: undeclared-method error code: `-32601` in the probe's own answer only. omp's error for an undeclared client call was not observed. Keep `[UNVERIFIED]`.

## Blockers

- B1: omp's only real credential (anthropic oauth, `disabled_cause: deleted by user`) stays disabled. Not re-enabled, copied or mocked. Real-model behaviour is `[UNOBSERVED]`. To run it, the user must re-enable a credential or provide an isolated login.
- B2: the `--profile` route was not tested. Only the `PI_CODING_AGENT_DIR` route was used.
- B3: the different-cwd load cancellation cause is open (`[UNOBSERVED]` reason).

## Unobserved items

- [UNOBSERVED] Real-model Q3 update kinds, usage fields and stopReason values, and real-model tool-call behaviour.
- [UNOBSERVED] Whether a grant keys on file path or folder; `Approve Directory` option.
- [UNOBSERVED] omp's behaviour after 40 s of an unanswered elicitation (Q5 silent case).
- [UNOBSERVED] Why `session/load` is cancelled for a different cwd.
- [UNOBSERVED] Q6 cancel during model streaming (`LONG_STREAM` not run).
- [UNOBSERVED] Q7 replay updates during a successful load; MCP reconnect on load.
- [UNOBSERVED] Error code omp returns for an undeclared client method (0009-019).
