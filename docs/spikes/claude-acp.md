# Spike: claude-agent-acp on Windows (Q1-Q10)

Repro: `spikes/claude-acp/README.md`. Client: `spikes/claude-acp/acp_client.py` (stdlib, no dependencies). Model stub: `spikes/claude-acp/stub_anthropic.py`.
Raw frame logs: `.spikes/claude-acp/out/` (not committed; redacted).

Two evidence tiers appear below:
- **real**: the user's subscription through the real Anthropic API. Blocked by the weekly rate limit in this run.
- **observed-with-stub**: the adapter and `claude` CLI 2.1.310 talking to a local Anthropic Messages stub (fake key `sk-stub-not-real`, `ANTHROPIC_BASE_URL` to `127.0.0.1`, throwaway `CLAUDE_CONFIG_DIR`). This tests the protocol and client-side behavior. It does not test real model output, real token counts, or real billing.

## Environment

- Windows 11 x64. Node v24.22.1 portable (`.spikes/node/`). Launch as `node <abs>/dist/index.js`, not `cmd /c`.
- Adapter `@agentclientprotocol/claude-agent-acp` 0.97.0 (`npm install @latest`, isolated npm cache and userconfig).
- Adapter bundled CLI (`index.js --cli --version`): 2.1.310 (Claude Code). Host `claude` on PATH: 2.1.310.
- `ANTHROPIC_API_KEY` is set in the user environment. The probe strips it from the child env (RFC-0009 [0009-069]) for real runs. Stub runs set only the fake key in the child env.
- Subscription: `claude auth status --json` reports `loggedIn: true`, `authMethod: claude.ai`, `apiProvider: firstParty`. Identity fields not recorded.
- **Weekly subscription rate limit is active** (`api_error_status: 429`, `rateLimitType: seven_day`). Every real model call fails with it.

## Verdicts

| Q | Question | Real | Stub | Verdict |
|---|---|---|---|---|
| Q1 | Install and launch on Windows as `node <abs entry>` | observed | - | PASS |
| Q2 | `initialize` response shape, protocol v1 | observed | - | PASS |
| Q3 | Auth: `authMethods`, identity reporting, `session/new` without `authenticate` | observed (partial) | observed | PARTIAL |
| Q4 | Permission flow (`session/request_permission`, `defaultMode`) | BLOCKED | observed | PARTIAL (stub) |
| Q5 | `usage_update` and `stopReason` | BLOCKED | observed | PASS (stub; values are stub constants) |
| Q6 | `session/cancel` during a turn | BLOCKED | observed | PASS (stub) |
| Q7 | `session/load` / `session/resume` after restart | BLOCKED (unknown id only: -32002) | observed | PASS (stub) |
| Q8 | Declined fs/terminal capabilities | BLOCKED | observed | PASS (stub) |
| Q9 | Windows process cleanup (stdin close, kill, ctrl-break) | observed | - | PASS for the tested paths |
| Q10 | `claude -p --output-format stream-json` | BLOCKED (429 result) | observed | PASS (stub; shape only) |

Verdicts marked "(stub)" are protocol and client-behavior results. They do not confirm real-model behavior. Re-run `--stub`-less commands when the rate limit resets to confirm the real tier.

## Method and observations

### Q1 install and launch: PASS (real)
```
npm install @agentclientprotocol/claude-agent-acp@latest  (108 packages, isolated cache)
node.exe .../dist/index.js --version        -> 0.97.0
node.exe .../dist/index.js --cli --version  -> 2.1.310 (Claude Code)
```
Bin shim `claude-agent-acp.cmd` launches `node.exe "%dp0%\..\@agentclientprotocol\claude-agent-acp\dist\index.js"`, so the `node <abs entry>` form is what the shim does too. Engines: `node >=22`. Launch from Python `subprocess` with absolute paths works.

### Q2 initialize: PASS (real)
Request: `initialize` with `protocolVersion: 1`, `clientCapabilities: {fs: {readTextFile:false, writeTextFile:false}, terminal:false, auth:{terminal:false}}`.
```
protocolVersion: 1
agentCapabilities: { promptCapabilities:{image:true,embeddedContext:true}, mcpCapabilities:{http:true,sse:true},
  auth:{logout:{}}, providers:{}, loadSession:true,
  sessionCapabilities:{additionalDirectories:{},close:{},delete:{},fork:{},list:{},resume:{},systemPrompt:{...}},
  _meta:{claudeCode:{promptQueueing:true}, authStatus:{}} }
agentInfo: {name:"@agentclientprotocol/claude-agent-acp", title:"Claude Agent", version:"0.97.0"}
authMethods: []
```
The adapter answered with protocol v1. Stdin close exited with code 0.

### Q3 auth: PARTIAL
- With `auth.terminal: false`, `authMethods` is `[]`. Terminal login methods (`claude-ai-login`, `console-login`) appear only when the client declares `auth.terminal: true` (acp-agent.js ~L1325-1383). `[UNOBSERVED]`: the terminal-true response.
- The `_auth/status_update` push (advertised by `agentCapabilities._meta.authStatus`):
  - Real, key inherited: `{"kind":"api_key","label":"Anthropic API key","detail":"ANTHROPIC_API_KEY"}`. The adapter reports whatever the child environment contains. Hosts MUST strip keys (RFC-0009 [0009-069]).
  - Real, key stripped: `{"kind":"none","label":"Not logged in"}`, although `claude auth status --json` reports `loggedIn: true, authMethod: claude.ai`. **Discrepancy**. Cause `[UNOBSERVED]`.
  - Stub, fake key in env: `{"kind":"api_key","label":"Anthropic API key","detail":"ANTHROPIC_API_KEY"}`. Consistent with the env-reporting behavior above.
- `session/new` without `authenticate`: works (real and stub). `modes.currentModeId "default"`, available `default, acceptEdits, plan, auto, bypassPermissions`.
- Verdict PARTIAL: authMethods shape verified. Identity report disagrees with the CLI on the real account (unexplained).

### Q4 permission flow: PARTIAL (stub; real BLOCKED)
Stub commands and results:

```
write --stub --perm allow_once
  probe.txt exists: True  content: hello
  permission requests: [{"toolCall":{"title":"Write probe.txt","kind":"edit"},
    "options":[{"optionId":"allow-once","kind":"allow_once","name":"Yes"},
               {"optionId":"allow-with-updates","kind":"allow_always","name":"Yes, allow all edits during this session"},
               {"optionId":"reject","kind":"reject_once","name":"No"}]}]
  tool updates: pending -> in_progress (Write probe.txt) -> completed
write --stub --perm reject_once
  probe.txt exists: False
  tool updates: ... -> status "failed"
write --stub --perm allow_once --disk-mode acceptEdits      (settings.json in throwaway CLAUDE_CONFIG_DIR, empty _meta)
  currentModeId: acceptEdits   permission requests: []   probe.txt exists: True
write --stub --perm allow_once --default-mode acceptEdits   (_meta.claudeCode.options.settings.permissions.defaultMode)
  currentModeId: default   permission requests: [one Write request]   probe.txt exists: True
```
Findings:
- The adapter sends `session/request_permission` with three options: kinds `allow_once`, `allow_always`, `reject_once`. Option ids are `allow-once`, `allow-with-updates`, `reject`.
- `allow_once` runs the tool. `reject_once` blocks it (tool status `failed`).
- **`_meta.claudeCode.options.settings` is ignored** in 0.97.0 for `defaultMode`: `currentModeId` stays `default`, and a permission prompt is still sent under requested `acceptEdits`.
- **The disk setting is honored**: `CLAUDE_CONFIG_DIR/settings.json` with `permissions.defaultMode: "acceptEdits"` gives `currentModeId: acceptEdits` and no prompt.
- Real: `session/prompt` failed on the rate limit before any permission request: `{"code": -32603, "message": "Internal error: You've hit your weekly limit · resets Oct 28, 5pm (Europe/Stockholm)", "data": {"errorKind": "rate_limit"}}`. `[UNOBSERVED]` in real mode.

### Q5 usage and stopReason: PASS (stub; shape only)
```
prompt result (write --stub, 2 model calls): usage {inputTokens: 246, outputTokens: 34, totalTokens: 280}
usage_update: {used: 124, size: 1000000, cost: null} ... {used: 140, size: 1000000, cost: {amount: 0.003328, currency: "USD"}}
stopReason: end_turn
```
- Turn-level usage is the sum over model calls (2 calls x 123 in / 17 out = 246 / 34). The per-call stub usage is 123 in / 17 out.
- `usage_update` carries `used`, `size`, and `cost` (`cost` appears only on the later updates, as `{amount, currency}`).
- `_meta.quota.token_count` and `model_usage` are also present on the prompt result.
- Values are stub constants. They prove the field shape and the summation, not real token accounting. Real: BLOCKED.

### Q6 cancel: PASS (stub)
```
cancel --stub                 (permission held, then session/cancel, then answer "cancelled")
  prompt returned after 0.028 s  stopReason: cancelled   c.txt exists: False
cancel --stub --no-answer     (permission held, cancel, no answer to the permission request)
  prompt returned after 0.027 s  stopReason: cancelled   adapter sent $/cancel_request
cancel --stub-long            (STUB:LONG streams 200 chunks at 50 ms; cancel after 11 chunks)
  prompt returned after 0.012 s  stopReason: cancelled
```
- The prompt returns `stopReason: "cancelled"` within about 30 ms in all three variants, even when the permission request is never answered.
- Streaming cancel: 11 `agent_message_chunk` updates arrived before cancel. Usage on that result was zero (the turn was cut before the stub's usage frame).
- Real: BLOCKED.

### Q7 load / resume: PASS (stub)
```
resume-stub --method session/load     (adapter A completes turn "remember banana"; exits; adapter B reloads)
  B loadSession: True  sessionCapabilities.resume: {}
  session/load result keys: [configOptions, modes, sessionId]
  update kinds during session/load: [agent_message_chunk, user_message_chunk]   (history replay)
  B prompt: agent text contains banana: True

resume-stub --method session/resume
  session/resume result keys: [configOptions, modes, sessionId]
  update kinds during session/resume: []                                          (no replay)
  B prompt: agent text contains banana: True
```
- Both methods reload the session after the adapter exits. `session/load` replays the history as `user_message_chunk` and `agent_message_chunk`. `session/resume` replays nothing.
- The stub recalls the word only from the message history it receives. The recall therefore shows the adapter sends the prior transcript on the second process. It does not show the model's memory.
- Unknown session id (real): both methods return `-32002 Resource not found`.
- Real live resume: BLOCKED.

### Q8 declined fs/terminal: PASS (stub)
```
fs --fs-mode decline     requests_in from agent: []   permission requests: []
fs --fs-mode advertise   requests_in from agent: []   permission requests: []
tool updates: Read File (kind read) -> completed;  Terminal (kind execute, title "echo hi") -> completed
```
- With fs advertised or declined, the adapter sent no `fs/*` or `terminal/*` request to the client. Its Read and Bash tools ran internally.
- Bash `echo hi` under default mode ran with no permission request in this run. Read-only tool approval rules were not isolated, so this is observed for `echo` only.
- Verdict PASS for "the adapter does not call the client for fs/terminal when they are declined or advertised-but-unanswered". Note that the adapter never needed them.

### Q9 Windows process cleanup: PASS for the tested paths (real)
A session created with `session/new` immediately spawns the SDK child (`claude.exe`) plus `conhost.exe` (PowerShell `Win32_Process` parent-tree snapshot, no prompt needed):
```
children before action: [{"pid": 46692, "name": "claude.exe"}, {"pid": 35092, "name": "conhost.exe"}]
```
| Action on node adapter | node exit code | exit time | children alive 3 s later |
|---|---|---|---|
| close stdin | 0 | 0.02 s | none |
| `Popen.terminate()` (TerminateProcess) | 1 | 0.02 s | none |
| `Popen.kill()` | 1 | 0.02 s | none |
| `taskkill /PID <node> /T /F` | 1 | 0.25 s | none |
| `CTRL_BREAK_EVENT` to new process group | 3221225786 (0xC000013A) | 0.02 s | none |

In every case the `claude.exe` child was gone 3 s after node exited, including TerminateProcess where only node was killed. `[UNOBSERVED]`: the mechanism (probably the child exits when its stdio pipe breaks, not an adapter-side tree kill).

### Q10 headless `claude -p`: PASS for shape (stub); real BLOCKED
Command (stub): `claude -p "STUB:PLAIN Reply with the single word OK" --output-format stream-json --verbose --no-session-persistence` with the stub env.
```
exit: 0   stderr_bytes: 0
is_error: False  subtype: success  stop_reason: end_turn
result text: "OK"
usage: input_tokens 123, output_tokens 17, cache_creation_input_tokens 0, cache_read_input_tokens 0, service_tier "standard", ...
modelUsage keys: ['claude-opus-5-5']
total_cost_usd: 0.000832
```
- `result` keys (stub): `api_error_status`, `duration_api_ms`, `duration_ms`, `fast_mode_disabled_reason`, `fast_mode_state`, `first_content_frame_ms`, `is_error`, `modelUsage`, `model_usage_since_last_result`, `num_turns`, `permission_denials`, `queued_turn_count`, `result`, `result_index`, `safety_stops`, `session_id`, `stop_reason`, `subagent_stats`, `subtype`, `terminal_reason`, `time_to_request_ms`, and more.
- Real run (rate limit): exit 1, events `system/init`, `assistant` (`model "<synthetic>"`), `rate_limit_event` (`status: rejected`, `rateLimitType: seven_day`), `result` with `api_error_status: 429`, all usage zeros, `modelUsage` empty.
- The `usage` values and `modelUsage` keys in stub mode are stub constants. Real values remain `[UNOBSERVED]`.

## Hygiene

- Stub runs used `CLAUDE_CONFIG_DIR` under `.spikes/claude-acp/cfg/<name>`. No entries were added to `~/.claude/projects` by these runs (checked: no `spikes-claude-acp` entries present).
- Real runs earlier: `~/.claude/projects/*spikes-claude-acp-work*` transcripts were deleted. `~/.claude.json` and `~/.claude/history.jsonl` may have received project/history entries from real `session/new` and were not changed by hand `[UNOBSERVED]`.
- Spawned processes: all exited (see process check in the final report). Only the Claude desktop app `claude.exe` under `C:\Program Files\WindowsApps\` remains, which is not ours.
- No user credentials were read or printed. The fake key `sk-stub-not-real` was set only in child environments.

## RFC changes required

Exact text proposals. RFCs are not edited here.

1. **RFC-0009 [0009-069]** (no `ANTHROPIC_API_KEY` for child): add evidence. Proposed addition: "Verified: with the key inherited, the adapter reports `{kind:"api_key"}` at initialize (claude-acp spike, 0.97.0, real)."
2. **RFC-0009 [0009-038]** (Windows launch `node <abs path>`): confirm. Proposed: "Verified on Windows 11, Node v24.22.1, claude-agent-acp 0.97.0: `node <abs>/dist/index.js` works from `subprocess` with no shell."
3. **RFC-0009 O5** (Windows stop semantics): propose resolution for the tested paths. Proposed: "Closing stdin exits the adapter with code 0 in about 0.02 s and no child survives. TerminateProcess/kill exits with code 1 and no child survives (observed). The mechanism is `[UNOBSERVED]`."
4. **RFC-0009 O2** (`session/resume` in the pinned version): resolve for the stub tier. Proposed: "`loadSession` and `sessionCapabilities.resume` are advertised in 0.97.0. After adapter restart, both `session/load` (with history replay) and `session/resume` (no replay) restore the session (observed-with-stub). Unknown id returns -32002. Real-model live resume is `[UNOBSERVED]`."
5. **RFC-0006 [0006-026]** (`defaultMode` must be `default`): replace with the observed mechanism. Proposed: "The default permission mode must be set through the adapter's settings file (`CLAUDE_CONFIG_DIR/settings.json`, `permissions.defaultMode`). In 0.97.0 the `_meta.claudeCode.options.settings` override is ignored for `defaultMode` (observed-with-stub: `currentModeId` stayed `default` under requested `acceptEdits`, and a permission prompt was still sent). Settings-file `acceptEdits` is honored (observed-with-stub)."
6. **RFC-0009 auth (new open item)**: the initialize-time `authStatus` push can disagree with `claude auth status --json` (`none` vs `loggedIn: true`, real). Proposed: track as an open issue. The host must not treat the initialize-time `_auth/status_update` as authoritative until a later push is observed.
7. **RFC-0009 permissions (new)**: the adapter's permission options are `allow_once`, `allow_always`, `reject_once` (ids `allow-once`, `allow-with-updates`, `reject`), observed-with-stub. Proposed: "Hosts answering `session/request_permission` MUST select a `kind` option. Cancelling the prompt returns `stopReason: cancelled` within about 30 ms, even with an unanswered permission request (observed-with-stub)."

Real-tier items to re-run when the weekly limit resets, without `--stub`: Q4 (`write --perm allow_once` and `reject_once`), Q5 (usage values), Q6 (`cancel` and `cancel --no-answer`), Q7 (`resume-a` then `resume-b --method session/load|session/resume`), Q8 (`fs --fs-mode advertise`), Q10 (`claude-p`). Stub-mode commands are listed in `spikes/claude-acp/README.md`.
