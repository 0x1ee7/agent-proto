# SP-4 hermes-a2a: how to reproduce

Findings and verdicts: `docs/spikes/hermes-a2a.md`. This README is the reproduction recipe. Everything runs on 127.0.0.1 with a throwaway Hermes profile, a local stub model and two spike tokens. No provider is called and no real Hermes home is touched.

## Prerequisites

- Windows 11, git-bash (commands below use bash syntax).
- `hermes` on PATH (v0.22.5 was used).
- Python 3.12 (scripts use the standard library only).
- Repo root as the working directory. Scripts resolve paths from it.

## Layout (all under `.spikes/`, gitignored)

```
.spikes/hermes/profiles/a2a-spike/config.yaml   throwaway Hermes profile
.spikes/hermes/secrets/peer.token               spike peer token, format <name>:<hex>
.spikes/hermes/secrets/peer2.token              second spike peer token
.spikes/hermes/logs/                            all probe output (tokens scrubbed)
```

## One-time setup

1. Create directories:

   ```
   mkdir -p .spikes/hermes/profiles/a2a-spike .spikes/hermes/secrets .spikes/hermes/logs
   ```

2. Write `.spikes/hermes/profiles/a2a-spike/config.yaml`:

   ```yaml
   model:
     provider: custom
     default: stub-model
     base_url: http://127.0.0.1:9912/v1
     api_mode: chat_completions
   gateway:
     standalone: true
   tools:
     tool_search:
       enabled: "off"
   a2a_agents:
     local-peer:
       url: http://127.0.0.1:9911/
       timeout: 20
       capabilities: [spike]
     card-peer:
       url: http://127.0.0.1:9913/
       timeout: 20
       capabilities: [spike]
   ```

   `gateway.standalone: true` is required: without it a non-default profile exits with code 78 while another gateway runs on the host. `tool_search` off keeps the `a2a_*` tools visible to the model. The config holds no tokens.

3. Create the two peer tokens. Never print them. [UNOBSERVED: this exact one-liner was not run; the token files were created in the same `<name>:<hex>` format.]

   ```
   python -c "import secrets; print('spike-peer:' + secrets.token_hex(16))" > .spikes/hermes/secrets/peer.token
   python -c "import secrets; print('spike-peer2:' + secrets.token_hex(16))" > .spikes/hermes/secrets/peer2.token
   ```

## Run order

Start the stub and the gateway in separate terminals (or as background jobs), then run the probes.

### 1. Stub model (port 9912)

```
python spikes/hermes-a2a/stub_openai.py 9912 .spikes/hermes/logs/stub.jsonl
```

Replies `STUB_REPLY: ok`, or `[INPUT_REQUIRED] Which city?` for `STUB_INPUT_REQUIRED`. `STUB_TOOL_DISCOVER <url>` and `STUB_TOOL_CALL <peer> <text>` make it emit `a2a_discover` / `a2a_call` tool calls. After a tool result it answers `STUB_AFTER_TOOL: <first 400 chars>`. Each request is one JSON line in the log file.

### 2. Gateway under test (Q1, port 9911)

```
env -u ANTHROPIC_API_KEY HERMES_HOME="C:/Users/0x1de/dev/agent-proto/.spikes/hermes/profiles/a2a-spike" OPENAI_API_KEY=spike-dummy-not-a-secret A2A_PORT=9911 A2A_PEER_TOKENS="$(tr -d '\n' < .spikes/hermes/secrets/peer.token),$(tr -d '\n' < .spikes/hermes/secrets/peer2.token)" hermes gateway run -v
```

Do not add `--replace`, and do not run `hermes gateway install`, `start` or `stop` against the real home. Adjust `HERMES_HOME` to the absolute path of your checkout if the repo is elsewhere.

### 3. Probes (need steps 1 and 2)

```
python spikes/hermes-a2a/probe.py
python spikes/hermes-a2a/probe_extra.py
python spikes/hermes-a2a/probe_q4.py
```

- `probe.py`: 34 tests T01-T32 (card, auth, SendMessage, SSE, error codes, INPUT_REQUIRED, contextId reuse, cross-peer GetTask/CancelTask/ListTasks, tenant, unknown fields, rate limit, metrics). Writes `probe-results.json` and `card-live.json` under `.spikes/hermes/logs/`.
- `probe_extra.py`: sends a body over 1 MiB on a raw socket (T14 retest). Writes `probe-extra-413.txt`. The client connection was reset (WinError 10053) before any status line was seen.
- `probe_q4.py`: INPUT_REQUIRED first turn plus three follow-ups (taskId, GetTask, contextId only). Imports `probe`. Writes `probe-q4.json`.

### 4. Card peer for Q7 (port 9913)

```
python spikes/hermes-a2a/card_server.py 9913 .spikes/hermes/logs/card-server.jsonl
```

Serves a card with unknown top-level, interface, capability, skill and provider fields. Answers SendMessage with a Task containing `futureTaskField`. Logs request bodies (Authorization omitted).

### 5. Hermes as client (Q7)

Run after steps 1, 2 and 4. Same env as step 2, with `-z` (one-shot) and `-t a2a` (A2A toolset):

```
env -u ANTHROPIC_API_KEY HERMES_HOME="C:/Users/0x1de/dev/agent-proto/.spikes/hermes/profiles/a2a-spike" OPENAI_API_KEY=spike-dummy-not-a-secret hermes -z "STUB_TOOL_DISCOVER http://127.0.0.1:9913/" -t a2a
env -u ANTHROPIC_API_KEY HERMES_HOME="C:/Users/0x1de/dev/agent-proto/.spikes/hermes/profiles/a2a-spike" OPENAI_API_KEY=spike-dummy-not-a-secret hermes -z "STUB_TOOL_CALL card-peer hello card" -t a2a
env -u ANTHROPIC_API_KEY HERMES_HOME="C:/Users/0x1de/dev/agent-proto/.spikes/hermes/profiles/a2a-spike" OPENAI_API_KEY=spike-dummy-not-a-secret hermes -z "STUB_TOOL_CALL local-peer hello local" -t a2a
```

Expected: the first prints the discovered card (`Agent: card-probe`, `Skills (1): echo`). The second ends with `CARD_PROBE_OK`. The third reports HTTP 401 for `local-peer`, because no token is configured for it. Configured-bearer success is not covered [UNOBSERVED].

## Scripts

| File | Role |
|---|---|
| `stub_openai.py` | Local OpenAI-compatible stub for the Hermes model (port 9912). |
| `probe.py` | Main HTTP/JSON-RPC probe against the gateway (port 9911). Shared helpers for `probe_q4.py`. |
| `probe_extra.py` | Oversize-body (>1 MiB) check on a raw socket. |
| `probe_q4.py` | INPUT_REQUIRED follow-up sequence. |
| `card_server.py` | Card peer with unknown fields (port 9913) for Q7. |

## Cleanup

1. Stop every process you started (Ctrl+C, or kill the background job). The gateway needs its whole process tree stopped, including the Python child.
2. Verify: each PID is gone (`Get-Process -Id <pid>` in PowerShell fails) and nothing listens on 9911-9913.
3. Do not delete `.spikes/hermes/logs/` until the note is final; it holds the raw evidence.

The user's real gateway (`hermes gateway run --replace`, separate home) is not part of this run and must not be stopped.
