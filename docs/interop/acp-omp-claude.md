# ACP driver notes: omp (`--mode acp`) and Claude (`@agentclientprotocol/claude-agent-acp`)

Date context: 2026. Scope: a Rust ACP **client** driving two ACP **agents**. Every factual claim cites a source. `[UNVERIFIED]` = not confirmed from a fetched source. `[proposal]` = design suggestion by the author, not a spec statement.

Sources (short names):
- ACP-OV: https://agentclientprotocol.com/protocol/v1/overview
- ACP-INIT: https://agentclientprotocol.com/protocol/v1/initialization
- ACP-SESS: https://agentclientprotocol.com/protocol/v1/session-setup
- ACP-PROMPT: https://agentclientprotocol.com/protocol/v1/prompt-turn
- ACP-TOOL: https://agentclientprotocol.com/protocol/v1/tool-calls
- ACP-FS: https://agentclientprotocol.com/protocol/v1/file-system
- ACP-TERM: https://agentclientprotocol.com/protocol/v1/terminals
- ACP-SCHEMA: https://agentclientprotocol.com/protocol/v1/schema
- ACP-AUTH: https://agentclientprotocol.com/protocol/v1/authentication.md
- ACP-V2-OV: https://agentclientprotocol.com/protocol/v2/overview.md
- OMP-APPROVAL: omp://approval-mode.md (ACP sessions section)
- OMP-CLI: omp://cli-reference.md
- OMP-RPC: omp://rpc.md (custom JSONL, not ACP)
- OMP-SETTINGS: omp://settings.md
- OMP-EXT: omp://extensions.md (ACP UI context)
- CAA-README: https://github.com/agentclientprotocol/claude-agent-acp (README, raw main)
- CAA-MODES: https://raw.githubusercontent.com/agentclientprotocol/claude-agent-acp/main/src/permissions/modes.ts
- CAA-MODEL: https://raw.githubusercontent.com/agentclientprotocol/claude-agent-acp/main/docs/model-configuration.md
- CAA-AUTH: https://raw.githubusercontent.com/agentclientprotocol/claude-agent-acp/main/src/auth-status.ts
- CAA-TOOLS: https://raw.githubusercontent.com/agentclientprotocol/claude-agent-acp/main/src/permissions/options/tools.ts
- CC-HEADLESS: https://code.claude.com/docs/en/headless.md

---

## 1. Summary for the driver author

| Question | Answer | Source |
|---|---|---|
| Protocol version to send | integer `1` (`"protocolVersion":1`). v2 is a draft; claude-agent-acp serves v2 only if the client asks for >=2 | ACP-INIT, CAA-README |
| Framing | JSON-RPC 2.0 over stdio (agent is a subprocess of the client) | ACP-OV |
| omp launch | `omp --mode acp` or `omp acp` (same thing) | OMP-CLI |
| Claude launch | `claude-agent-acp` bin from npm package `@agentclientprotocol/claude-agent-acp` | CAA-README |
| Paths | MUST be absolute; line numbers 1-based | ACP-OV |
| Mandatory client handling | answer every `session/request_permission`; treat `session/update` as notification (no reply) | ACP-OV, ACP-TOOL |
| Decline fs/terminal | do not advertise `fs.*` / `terminal` in `clientCapabilities` (agent MUST NOT call them then) | ACP-FS, ACP-TERM |
| Cancel | `session/cancel` notification; wait for pending `session/prompt` to return `stopReason: "cancelled"` | ACP-PROMPT |

---

## 2. ACP core (v1 baseline)

### 2.1 Transport and envelope
- JSON-RPC 2.0: **methods** (request/response) and **notifications** (no response; never get a success or error reply). Source: ACP-OV.
- Errors: `error` object with `code`, `message`. Source: ACP-OV.
- Property keys camelCase; discriminator values snake_case. Source: ACP-OV.
- Extensibility: `_meta` fields; custom methods prefixed `_`; custom capabilities via `_meta`. Source: ACP-OV.
- Enums may have future/custom fallbacks: `_`-prefixed values reserved for implementation extensions; unknown non-underscore values reserved for future ACP variants (so a Rust client must not hard-fail on unknown enum strings). Source: ACP-V2-OV (Conventions), same rule referenced in v1 schema.

### 2.2 Method table (v1)

| Direction | Method | Kind | Gate | Source |
|---|---|---|---|---|
| C→A | `initialize` | baseline | - | ACP-OV |
| C→A | `authenticate` | baseline (when `authMethods` non-empty) | - | ACP-OV, ACP-AUTH |
| C→A | `session/new` | baseline | - | ACP-OV |
| C→A | `session/prompt` | baseline | - | ACP-OV |
| C→A | `session/load` | optional | `agentCapabilities.loadSession` | ACP-OV, ACP-SESS |
| C→A | `session/set_mode` | optional | agent-defined | ACP-OV |
| C→A | `logout` | optional | `agentCapabilities.auth.logout` | ACP-OV, ACP-AUTH |
| C→A | `session/resume`, `session/close`, `session/delete`, `session/list` | optional | `sessionCapabilities.*` | ACP-SESS |
| C→A (notif) | `session/cancel` | baseline | - | ACP-OV |
| A→C | `session/request_permission` | baseline | - | ACP-OV, ACP-TOOL |
| A→C | `fs/read_text_file`, `fs/write_text_file` | optional | `clientCapabilities.fs.*` | ACP-OV, ACP-FS |
| A→C | `terminal/create`, `output`, `release`, `wait_for_exit`, `kill` | optional | `clientCapabilities.terminal` | ACP-OV, ACP-TERM |
| A→C | `elicitation/create` | optional | elicitation capability | ACP-OV |
| A→C (notif) | `session/update` | baseline | - | ACP-OV |
| A→C (notif) | `elicitation/complete` | optional | - | ACP-OV |

### 2.3 initialize

Request (client):
```json
{"jsonrpc":"2.0","id":0,"method":"initialize","params":{
  "protocolVersion":1,
  "clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false,"auth":{"terminal":false}},
  "clientInfo":{"name":"agent-proto","title":"agent-proto","version":"0.1.0"}}}
```
Source for field names and shape: ACP-INIT, ACP-SCHEMA. Note: the schema default for `clientCapabilities` when omitted is `{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false,"auth":{"terminal":false}}` (ACP-SCHEMA). Omitted capability = unsupported.

Response (agent) fields: `protocolVersion`, `agentCapabilities`, `agentInfo` {`name`,`title`,`version`}, `authMethods` []. Source: ACP-INIT.

If the agent returns a `protocolVersion` you do not support, the client SHOULD disconnect. Source: ACP-INIT.

`agentCapabilities` (defaults from schema, ACP-SCHEMA/ACP-INIT):

| Field | Default if absent | Meaning |
|---|---|---|
| `loadSession` | `false` | `session/load` allowed |
| `promptCapabilities.image` / `.audio` / `.embeddedContext` | `false` | prompt content types beyond text |
| `mcpCapabilities.http` / `.sse` | `false` | MCP transports accepted in `mcpServers` |
| `sessionCapabilities` | `{}` | `resume`, `close`, `delete`, `additionalDirectories` objects |
| `auth.logout` | absent | `logout` method allowed |

Rule: text and `resource_link` prompt blocks MUST always be supported. Source: ACP-INIT.

### 2.4 Session setup

- `session/new` params: `cwd` (absolute; base for relative paths, MUST be used regardless of spawn dir), `mcpServers` (required array), optional `additionalDirectories` when `sessionCapabilities.additionalDirectories`. Response: `{"sessionId":"..."}`. Source: ACP-SESS.
- `session/load` (only if `loadSession`): params `sessionId`, `cwd`, `mcpServers`. Agent MUST replay the full history as `session/update` notifications (`user_message_chunk`, `agent_message_chunk`) **before** responding `{}`. Notices are not replayed; compaction is replayed. Source: ACP-SESS.
- `session/resume` (only if `sessionCapabilities.resume`): params `sessionId`, `cwd`, `mcpServers`; **no** history replay; response `{}` (may include mode/model/config state). Source: ACP-SESS.
- `session/close` (only if `sessionCapabilities.close`): params `sessionId`; agent cancels active work, then frees; response `{}`; may error if session missing. Source: ACP-SESS.
- `session/list`, `session/delete`: `[UNVERIFIED]` field details (session-list page not fetched). claude-agent-acp v2 serves `session/list` and `session/delete` (CAA-README).
- `additionalDirectories`: entries MUST be absolute; effective root set is `[cwd, ...additionalDirectories]`. Source: ACP-SESS.

### 2.5 session/prompt and turn lifecycle

Request:
```json
{"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{"sessionId":"S","prompt":[{"type":"text","text":"hello"}]}}
```
Content blocks: `text` `{type,text}`; `resource` `{type:"resource",resource:{uri,mimeType,text}}`; `image`, `audio`, `resource_link` per prompt capabilities. Client MUST restrict content types to those advertised. Source: ACP-PROMPT, ACP-SESS.

Response: `{"stopReason": "<StopReason>"}`. Source: ACP-PROMPT.

`StopReason` values (ACP-SCHEMA, ACP-PROMPT): `end_turn`, `max_tokens`, `max_turn_requests`, `refusal`, `cancelled`. `cancelled` MUST be returned when the client sent `session/cancel`, even if underlying operations throw.

Turn flow (ACP-PROMPT): client `session/prompt` → agent emits `session/update` notifications and optionally `session/request_permission` requests → final response with `stopReason`.

### 2.6 session/update variants (ACP-PROMPT)

`params` = `{"sessionId":"...","update":{"sessionUpdate":"<kind>", ...}}`

| `sessionUpdate` | Notes | Gate |
|---|---|---|
| `user_message_chunk` | replayed on load; optional `messageId` | - |
| `agent_message_chunk` | text/content; optional `messageId` | - |
| `agent_thought_chunk` | reasoning | - |
| `tool_call` | new tool call | - |
| `tool_call_update` | all fields except `toolCallId` optional | - |
| `plan` | `entries[]` each `{content, priority: high\|medium\|low, status}` | - |
| `available_commands_update` | slash commands | - |
| `current_mode_update` | mode change | - |
| `config_option_update` | config options | `session.configOptions` (client) |
| `session_info_update` | session metadata | - |
| `usage_update` | see 2.8 | - |
| `notice` | `{severity: info\|warning\|error, title, description?, _meta?}` | client `session.notices` |
| `compaction_update` | `{compactionId, status: in_progress\|completed\|failed\|cancelled, summary?, error?}` | client `session.compaction` |
| `compaction_summary_chunk` | `{compactionId, content}` | client `session.compaction` |

Sources: ACP-PROMPT, ACP-SCHEMA. Note: the `notice` and `compaction_*` fields are from the v1 prompt-turn page as summarized in our notes; exact `session` capability keys (`session.notices`, `session.compaction`, `session.configOptions.boolean`) are from ACP-INIT (client-side capabilities).

### 2.7 Tool calls (ACP-TOOL)

`tool_call` fields: `toolCallId` (required), `title` (required), `name` (optional, opaque), `kind` (`read`,`edit`,`delete`,`move`,`search`,`execute`,`think`,`fetch`,`switch_mode`,`other`), `status` (`pending`,`in_progress`,`completed`,`failed`; default `pending`), `content` (ToolCallContent), `locations`, `rawInput`, `rawOutput`.

ToolCallContent variants: `{"type":"content","content":{...}}`, `{"type":"diff","path","oldText","newText"}`, `{"type":"terminal","terminalId"}`.

### 2.8 usage_update

`usage_update` fields: `used` (uint64, tokens in context), `size` (uint64, context window), optional `cost: {amount, currency}` (currency ISO 4217). Source: ACP-PROMPT. Note: this is context usage, not billing; per-turn token counts are not in v1 `StopReason` response.

### 2.9 session/cancel (ACP-PROMPT)

Notification: `{"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"S"}}`.

Client obligations: SHOULD mark non-finished tool calls `cancelled`; MUST answer all pending `session/request_permission` with outcome `cancelled`; SHOULD still accept tool updates after cancel.
Agent obligations: SHOULD stop LLM and tools; MUST respond to the pending `session/prompt` with `stopReason: "cancelled"` after aborting; MUST catch abort exceptions.

### 2.10 session/request_permission (ACP-TOOL)

Agent→client request:
```json
{"jsonrpc":"2.0","id":7,"method":"session/request_permission","params":{
  "sessionId":"S",
  "toolCall":{"toolCallId":"t1"},
  "options":[{"optionId":"allow","name":"Allow","kind":"allow_once"},{"optionId":"deny","name":"Deny","kind":"reject_once"}]}}
```
`kind` values: `allow_once`, `allow_always`, `reject_once`, `reject_always`.

Client response (selected):
```json
{"jsonrpc":"2.0","id":7,"result":{"outcome":{"outcome":"selected","optionId":"allow"}}}
```
Cancelled (required on `session/cancel`):
```json
{"jsonrpc":"2.0","id":7,"result":{"outcome":{"outcome":"cancelled"}}}
```
Clients MAY auto-allow or auto-reject by user settings. Source: ACP-TOOL.

### 2.11 File system and terminal methods

**fs/read_text_file** (ACP-FS): agent MUST NOT call unless `clientCapabilities.fs.readTextFile: true`. Params `sessionId`, `path` (absolute), optional `line` (1-based), optional `limit`. Result `{"content":"..."}`. Reads unsaved editor state.

**fs/write_text_file** (ACP-FS): requires `fs.writeTextFile`. Params `sessionId`, `path`, `content`. Client MUST create the file if missing. Result `{}`.

**terminal/create** (ACP-TERM): requires `clientCapabilities.terminal: true`. Params `sessionId`, `command`, `args`, `env` [{name,value}], `cwd` (absolute), `outputByteLimit`. Result `{"terminalId":"..."}` immediately.
- `terminal/output` → `{output, truncated, exitStatus?:{exitCode,signal}}`
- `terminal/wait_for_exit` → `{exitCode, signal}`
- `terminal/kill` keeps id valid
- `terminal/release` kills and invalidates id; agent MUST call it when done.

**Declining fs/terminal**: `[UNVERIFIED]` exact error code when a client receives a call it did not advertise. The documented rule is that the agent MUST NOT call the method when the capability is false/absent (ACP-FS, ACP-TERM). Driver behavior: keep `fs.*=false` and `terminal=false` in `initialize`; if an agent calls anyway, reply with a JSON-RPC `error` (`code`, `message`). Exact code: `[UNVERIFIED]`.

### 2.12 mcpServers (ACP-SESS)

Used in `session/new`, `session/load`, `session/resume`.
- stdio (MUST be supported by agents): `{"name","command"(absolute path),"args":[],"env":[{name,value}]}`.
- http (needs `mcpCapabilities.http`): `{"type":"http","name","url","headers":[{name,value}]}`.
- sse (needs `mcpCapabilities.sse`, deprecated): `{"type":"sse",...}`.
- Agents SHOULD connect to all listed servers.

### 2.13 Authentication (ACP-AUTH, ACP-INIT)

- `authMethods[]` returned in `initialize`. Method `type` absent or `agent` → client calls `authenticate` with `{"methodId":"..."}`; result `{}`.
- `terminal` method (`args`, `env`) → client MUST NOT call `authenticate`; it launches the same agent binary with appended args/env in an interactive terminal; exit code 0 = success; then re-initialize. Requires client `clientCapabilities.auth.terminal: true`.
- `logout` requires `agentCapabilities.auth.logout`; result `{}`.
- Auth-gated requests fail with an `auth_required` error `[UNVERIFIED]` exact code string.

### 2.14 Protocol version and draft v2 (ACP-V2-OV, CAA-README)

v2 overview differences observed (ACP-V2-OV):
- Baseline methods: `auth/login` and `auth/logout` (replacing v1 `authenticate`/`logout`). MUST implement both if `authMethods` non-empty.
- `session/list`, `session/resume`, `session/close` are in the v2 baseline.
- `session/prompt` response = acknowledgement "once the prompt is accepted"; turn end is signaled by an idle `state_update` with a stop reason (ACP-V2-OV message flow). This is a semantic change from v1 (v1 response carries `stopReason`).
- Notifications listed: `session/update` covers `state_update`, `display-only terminal` output, `compaction`, `notices`.
- `elicitation/create` and `elicitation/complete` listed.

Driver implication: Support v1 first. Negotiate `protocolVersion:1`. If you ever send `2`, claude-agent-acp serves v2 only when the client asks for >=2 (CAA-README). omp's v2 support is `[UNVERIFIED]`.

---

## 3. omp `--mode acp`

### 3.1 Launch

| Item | Value | Source |
|---|---|---|
| Command | `omp --mode acp` or `omp acp` | OMP-CLI |
| Transport | ACP over stdio | OMP-CLI |
| Other modes | `--mode rpc` is custom JSONL, not JSON-RPC 2.0; do not confuse | OMP-CLI, OMP-RPC |
| Config overlay | `omp acp --config <file>` accepted | OMP-SETTINGS line 304 |
| Working dir | `--cwd <dir>` overrides launch cwd; ACP `session/new` `cwd` is the session root | OMP-CLI, ACP-SESS |
| Extra roots | `--add-dir <dir>` (repeatable) | OMP-CLI |
| Session dir | `--session-dir <dir>` | OMP-CLI |
| No persistence | `--no-session` | OMP-CLI |
| Resume flags | `--continue`/`-c`, `--resume [id]`/`-r`, `--session [id]`, `--fork` (CLI launch) | OMP-CLI |

Env vars (omp): `PI_NO_TITLE=1` disables title generation; RPC/ACP pin protocol defaults (OMP-RPC line 30). Every `bash` tool command (including ACP client terminal routes) receives `PI_SESSION_ID` and session identity (OMP-ENV). `PI_CONFIG_FILES` = platform-delimited list (`;` on Windows) of config overlays (OMP-SETTINGS line 306).

### 3.2 Settings precedence for ACP (OMP-SETTINGS, OMP-APPROVAL)

1. runtime overrides (`--approval-mode`, `--auto-approve`/`--yolo`, protocol-mode defaults)
2. `--config` overlays on the `omp acp` process
3. project: `<cwd>/.omp/settings.json`, `<cwd>/.omp/config.yml` (cwd = ACP session cwd)
4. global: active agent dir `config.yml`

`ACP does not currently define a session/new, session/load, or session/resume approval-policy field`: per-session yolo requires a separate `omp acp` process with `--yolo`, `--auto-approve`, `--approval-mode yolo`, or a `--config` file with `tools.approvalMode: yolo` (OMP-APPROVAL).

### 3.3 Approval and permission (OMP-APPROVAL)

- Tool tiers: `read`, `write`, `exec`. Unknown/custom tools default to `exec`; MCP tools declare `write`.
- Modes (`tools.approvalMode`): `always-ask` (auto-approve `read`, prompt `write`/`exec`); `always-ask-cached` (auto-approve read without file targets and granted files); `write` (auto-approve `read`,`write`; prompt `exec`); `yolo` (default; auto-approve all).
- Without explicit yolo opt-in, **unmatched bash commands and destructive file ops still require an ACP client grant** (OMP-APPROVAL line 193). Schema default is `yolo`, but explicit opt-in is needed to skip the ACP gate.
- `bash`: ACP resolves `bash.patterns`, critical overrides, per-tool policies and mode before asking. `allow` = no card; `deny` = blocked, no card; `prompt` = `session/request_permission`. "Always allow" covers later ordinary bash calls in session; critical commands prompt every call.
- `delete`, `move`, and `edit` with deletion/rename operations: extra editor-side gate. Ordinary edits do not use it.
- Generic approval prompts use **form elicitation** when the client advertises `elicitation.form` (OMP-APPROVAL line 197). A rejected/cancelled/unsupported prompt rejects or cancels the tool call. Driver must therefore either handle `elicitation/create` or decline the capability; `[UNVERIFIED]` whether omp falls back to `session/request_permission` when `elicitation.form` is absent (OMP-EXT says defaults are returned when the client lacks `elicitation.form`, for extension UI).
- omp extension UI in ACP mode: `select`/`confirm`/`input`/`editor` round-trip as ACP form elicitations; defaults returned when client lacks `elicitation.form` (OMP-EXT, "ACP mode").

Driver decision (proposal): advertise no `elicitation` capability in v1 unless implementing `elicitation/create`; expect omp to use its defaults. Test `[UNVERIFIED]`.

### 3.4 Filesystem and terminal

- omp ACP `writeTextFile` "hands the write to a remote client" (OMP-EXT line 813): omp can route writes through the client's `fs/write_text_file`. Whether omp requires that capability: `[UNVERIFIED]`.
- ACP client terminal: omp's `bash` tool runs through `ACP-terminal` routes when the client offers terminals (OMP-ENV line 598; OMP-BASH). `[UNVERIFIED]` which branch runs when `terminal` capability is false.
- ACP-MCP: ACP sessions disable on-disk MCP discovery; MCP definitions come from the ACP client (`mcpServers`) (OMP-MCP line 36).

### 3.5 Cancel, usage, persistence

- Cancel: omp is interrupted by ACP cancel; `[UNVERIFIED]` exact path. OMP-ADVISOR (line 165) says cancel from ACP is treated as a deliberate interrupt.
- Usage: omp `get_state` (RPC) exposes `contextUsage {tokens, contextWindow, percent}`; this is RPC, not ACP. ACP `usage_update` expected to carry `used`/`size` `[UNVERIFIED]` for omp.
- Persistence/resume: session files under session dir; `--session-dir` and `--no-session` apply to ACP process (OMP-CLI). ACP `session/load`/`resume` support in omp `[UNVERIFIED]` (not stated in fetched pages; check `initialize` response `agentCapabilities`).
- Slash commands in ACP: `available_commands_update` advertises commands; ACP dispatch runs built-ins before prompt; TUI-only built-ins hidden (OMP-SLASH).
- Session export/share in ACP: default flow only (OMP-SESSION).

### 3.6 Windows notes

- `omp` config path, `PI_CONFIG_FILES` uses `;` on Windows (OMP-SETTINGS line 306).
- `bash` on Windows: `[UNVERIFIED]`. Not checked.

### 3.7 MCP tools as the model sees them (live E2E)

Source: live run S1Debug, reported by Main; not yet recorded in `docs/spikes/e2e-m1.md`. That file still records S1 (0014-010) as FAIL with `BACKEND_CRASHED` before the tools reached the model, so S1 has not passed end to end.

- omp does NOT expose client-provided MCP tools as function tools. It lists them in the system prompt under `## MCP Tool Routes` as `xd://mcp__<server>_<tool>` (for example `xd://mcp__a2abus_a2a_send`).
- The model invokes such a tool by calling omp's `write` tool with `path` set to the route and `content` set to the JSON arguments. `tools.xdev` (default true) enables the route surface.
- The settings `tools.discoveryMode` and `mcp.discoveryMode` had no observed effect on the route list.
- Approval consequence: under `always-ask-cached` the call is a `write`, so it arrives as a `write` form elicitation (`Allow tool: write`, RFC-0006 [0006-052]). The route text is in the untrusted `Path:` line (RFC-0006 [0006-060]).
- claude's expected tool name form `mcp__<server>__<tool>` is `[UNVERIFIED]`; not observed.

---

## 4. claude-agent-acp (`@agentclientprotocol/claude-agent-acp`)

### 4.1 Install and run

- Package: `@agentclientprotocol/claude-agent-acp` (npm). Preview channel: `npm install @agentclientprotocol/claude-agent-acp@preview` (CAA-README). Built on the Claude Agent SDK (CAA-README).
- Run as an ACP stdio agent: `node dist/index.js` in the repo; the README example uses `node dist/index.js` (CAA-MODEL). The npm bin name is `claude-agent-acp` (npm package metadata, v0.97.0, fetched 2026-10-26).
- Windows: `[UNVERIFIED]` specific install steps. Spawn with `cmd /c npx ...` or direct `node.exe path\to\index.js` from Rust (`std::process::Command`). Note env var names use `CLAUDE_*` (see below).
- Prerequisite: Claude Code CLI login or API key (see 4.2). `claude auth status --json` is used by the adapter (CAA-AUTH).

### 4.2 Auth

- Adapter reads identity from `claude auth status --json`, with fields `loggedIn`, `authMethod`, `apiProvider`, `apiKeySource`, `email`, `orgId`, `orgName`, `subscriptionType` (CAA-AUTH).
- Kinds reported by the adapter via `_auth/status_update`: `account` (claude.ai subscription, label "Claude Max" etc.), `api_key` (`apiKeySource` = `apiKeyHelper`, `env`, `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`), `gateway`, `external` (Bedrock, Vertex, Foundry), `none` (CAA-AUTH).
- Precedence: `apiKeyHelper` in settings outranks claude.ai OAuth login (CAA-AUTH comment).
- Notification `_auth/status_update` → `{authStatus}` (CAA-AUTH). Advertised via `agentCapabilities._meta.authStatus` `{}`.
- `CLAUDE_MODEL_CONFIG` env JSON: `modelOverrides` and `availableModels` (CAA-MODEL). Invalid JSON fails session creation.
- Bedrock: `CLAUDE_CODE_USE_BEDROCK=1`, `AWS_REGION` (CAA-MODEL example).
- Claude Code `-p` path (non-ACP) reads `ANTHROPIC_API_KEY`; in `--bare` mode OAuth/keychain are never read (CC-HEADLESS). Adapter auth path itself is `[UNVERIFIED]` beyond CAA-AUTH.

Driver recommendation: the driver never sets provider key variables for the child, including `ANTHROPIC_API_KEY` (0009-069, M1-10). Auth comes from the agent's own login or configuration. `[UNVERIFIED]` whether the adapter advertises ACP `authMethods` for login.

### 4.3 Permission modes

- Setting `permissions.defaultMode` (settings) is mapped (CAA-MODES) with case-insensitive aliases:

| Setting value (any case) | SDK `PermissionMode` |
|---|---|
| `auto` | `auto` |
| `default`, `manual` | `default` |
| `acceptEdits`, `acceptedits` | `acceptEdits` |
| `dontAsk`, `dontask` | `dontAsk` |
| `plan` | `plan` |
| `bypassPermissions`, `bypass` | `bypassPermissions` (clamped to `default` if root unless `IS_SANDBOX`) |

Unknown value → `default` (logged). Source: CAA-MODES.

- Tool permission prompts are sent as ACP `session/request_permission`. Options are built per tool type (CAA-TOOLS): WebFetch → `allow_once` plus `"Yes, and don't ask again for <host>"` (`allow_always`) plus reject `"No"`; Skill → allow once/always variants; ExitPlanMode → `Yes, clear context and use auto mode`, `Yes, and bypass permissions`, `Yes, manually approve edits` (`allow_once`), `No, keep planning`; fallback → `Yes, and don't ask again for <tool> commands`. Option kinds use the ACP `allow_once`/`allow_always`/`reject_once` values.
- Mapping to ACP: permission requests use `session/request_permission`; the response `outcome` is `selected` with an `optionId`, or `cancelled` on cancel (ACP-TOOL).
- `[UNVERIFIED]` exact mapping of each Claude mode to ACP options beyond CAA-TOOLS.

### 4.4 Sessions, resume, MCP, usage

- Sessions: `session/new`, `session/load`, `session/resume`, `session/list`, `session/close`, `session/delete`, `session/fork`, `additionalDirectories` available on v2 (CAA-README). v1 supports `session/load` and `session/new`; `session/resume` on v1 `[UNVERIFIED]`.
- `session/cancel` on parent interrupts Claude Code; background children keep running (CAA-README).
- MCP: client MCP servers are supported (CAA-README). `/mcp` in chat runs reconnect/enable/disable (CAA-README).
- Subagents: child sessions when `clientCapabilities.subagents` is declared (not AIR). `subagent_update` announces child with states `running`, `requires_action`, `idle`; stop reasons `end_turn`, `cancelled`, `error` (CAA-README). v2 gets this without declaring.
- `_meta.claudeCode.options.settings` in `session/new`/`session/load` merges settings; caller keys win (CAA-MODEL).
- Usage: ACP `usage_update` expected (CAA-README lists streamed tokens and costs only indirectly) `[UNVERIFIED]` exact fields for Claude; treat as `used`/`size` per ACP-PROMPT.
- Limitations: Steering and AIR extensions are v1-only for v2 (CAA-README). Bypass permissions unavailable as root (CAA-MODES).

### 4.5 Fallback: `claude -p --output-format stream-json`

Source: CC-HEADLESS.

- Command: `claude -p "<prompt>" --output-format stream-json --verbose --include-partial-messages` (CC-HEADLESS).
- Output: newline-delimited JSON; last line is a `result` message with final text, cost, session metadata (CC-HEADLESS).
- Event types seen in CC-HEADLESS:
  - `system` with `subtype: "init"` (first event): model, tools, MCP servers, loaded plugins. Fields `plugins`, `plugin_errors`, `mcp_servers`, `mcp_server_errors`, optional `capabilities` array (CC-HEADLESS).
  - `system` with `subtype: "api_retry"`: `attempt`, `max_retries`, `retry_delay_ms`, `error_status`, `error`, `session_id` (CC-HEADLESS table).
  - `system` with `subtype: "plugin_install"`: `status` (`started`,`installed`,`failed`,`completed`) (CC-HEADLESS).
  - `assistant` and `user` messages with `parent_tool_use_id` (CC-HEADLESS, subagent section).
  - `stream_event` with `event.delta.type == "text_delta"` (partial messages) (CC-HEADLESS example).
  - `result` final (CC-HEADLESS).
- `result` fields named in CC-HEADLESS: `total_cost_usd`, per-model cost breakdown, session ID, usage metadata, `structured_output` when `--json-schema` is used. Exact `usage` sub-fields (`input_tokens`, `output_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens`) and `subtype`/`is_error` values: `[UNVERIFIED]` (not present in fetched text; confirm via Agent SDK TypeScript docs or a live run).
- Exit codes: 0 success, non-zero failure; failures inside a run are printed as the result on stdout (CC-HEADLESS).
- SIGTERM → exit 143, turn left unfinished; send SIGINT to end the turn (CC-HEADLESS).
- `--bare`: skips hooks, MCP from project, CLAUDE.md; requires `ANTHROPIC_API_KEY` (CC-HEADLESS).
- Background work: `-p` waits up to 10 minutes idle by default (`CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS`) (CC-HEADLESS).

---

## 5. Differences that matter for one Rust driver

| Area | omp `--mode acp` | claude-agent-acp | Source |
|---|---|---|---|
| Launch | `omp --mode acp` | `node` / npm bin of `@agentclientprotocol/claude-agent-acp` | OMP-CLI, CAA-README |
| Protocol | v1 `[UNVERIFIED]` for omp version pin | v1 default; v2 on request | CAA-README |
| Approval | per-tool tier + bash patterns; ACP client gate; elicitation when `elicitation.form` | `request_permission` with per-tool option lists | OMP-APPROVAL, CAA-TOOLS |
| Per-session yolo | not in ACP; separate process with `--yolo` or config | permission mode via settings `permissions.defaultMode` | OMP-APPROVAL, CAA-MODES |
| MCP | from client `mcpServers` only (on-disk discovery disabled) | client MCP servers + `/mcp` | OMP-MCP, CAA-README |
| Persistence | session dir / `--no-session` | Claude session index `src/session-index/*` `[UNVERIFIED]` | OMP-CLI, CAA-README |
| Usage | ACP `usage_update` `[UNVERIFIED]` | ACP `usage_update` `[UNVERIFIED]` | ACP-PROMPT |
| Auth | omp config/keys `[UNVERIFIED]` | `claude auth status`; `ANTHROPIC_API_KEY`, Bedrock/Vertex | CAA-AUTH, CAA-MODEL |

---

## 6. Mapping proposal: ACP → A2A v1.0 (`[proposal]`)

A2A state names and Part/Artifact types are not verified against a fetched A2A v1.0 page in this document. Mark all A2A names `[UNVERIFIED]` until checked against https://a2a-protocol.org/latest/specification/ and proto `lf.a2a.v1`.

| ACP | A2A (proposal) | Status |
|---|---|---|
| `initialize` | Agent Card + capability negotiation at bus registration | `[proposal]` |
| `session/new` | Task creation with new `contextId` | `[proposal]` |
| `session/load` / `session/resume` | continue same `contextId` | `[proposal]` |
| `session/prompt` | `SendMessage` (user Message with Parts) | `[proposal]` |
| `session/prompt` response `stopReason: end_turn` | Task state `COMPLETED` | `[proposal]` |
| `stopReason: cancelled` | Task state `CANCELED` | `[proposal]` |
| `stopReason: refusal` | Task state `REJECTED` or `FAILED` | `[proposal]` |
| `stopReason: max_tokens` / `max_turn_requests` | Task state `FAILED` or `COMPLETED` + notice | `[proposal]` |
| `session/update` `agent_message_chunk` | status Message or Artifact TextPart | `[proposal]` |
| `tool_call` / `tool_call_update` | Task status updates or DataPart | `[proposal]` |
| `plan` | DataPart | `[proposal]` |
| `usage_update` | metadata (`used`, `size`) | `[proposal]` |
| `session/request_permission` | `TASK_STATE_INPUT_REQUIRED` + Message with options | `[proposal]` |
| permission response `selected` / `optionId` | user Message selecting option | `[proposal]` |
| `session/cancel` | `CancelTask` | `[proposal]` |
| `notice` | status Message | `[proposal]` |
| `subagent_update` (claude) | child Task or metadata | `[proposal]` |

---

## 7. Driver checklist (for the implementer)

1. Spawn child with `cwd` set; set env (`ANTHROPIC_API_KEY` for Claude if using key auth; `PI_NO_TITLE=1` optional for omp).
2. Send `initialize` with `protocolVersion:1`; set `fs` and `terminal` to `false` unless implementing them.
3. Read `agentCapabilities.loadSession`, `sessionCapabilities`, `promptCapabilities`, `mcpCapabilities`, `authMethods`.
4. If `authMethods` non-empty and type `agent`, call `authenticate` with `methodId` before `session/new`. If `terminal`, do not call `authenticate`; run interactive login.
5. `session/new` with absolute `cwd` and `mcpServers`.
6. `session/prompt` with `text` blocks; read `session/update` until the response returns `stopReason`.
7. Answer every `session/request_permission` with `selected` or `cancelled`.
8. On cancel: send `session/cancel`, answer pending permissions with `cancelled`, await `stopReason: "cancelled"`.
9. Unknown `sessionUpdate` kinds and unknown enum strings: ignore, do not fail.
10. Implement `session/load` only if `loadSession` is true; never call otherwise.

---

## 8. Checks Main should run (not run by me)

- Spawn `omp --mode acp` and Claude adapter; capture the `initialize` response of each to confirm `agentCapabilities` and `authMethods`.
- Confirm omp `protocolVersion` and whether `elicitation.form` absence triggers `session/request_permission`.
- Confirm claude-agent-acp Windows install (`npx` vs `node dist/index.js`) and `claude auth status --json` output.
- Confirm Claude `stream-json` `result` usage sub-fields with a live `claude -p` run.
- Confirm A2A v1.0 state names against https://a2a-protocol.org/latest/specification/ before finalizing 6.
- Resolve every `[UNVERIFIED]` above by live test.
