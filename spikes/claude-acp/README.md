# claude-acp spike: reproduce

Findings: `docs/spikes/claude-acp.md`. Scratch state lives in `.spikes/claude-acp/` (gitignored).

## Setup (already done once)

Node 24.22.1 portable zip, from `https://nodejs.org/dist/v24.22.1/node-v24.22.1-win-x64.zip`
(SHA256 checked against `SHASUMS256.txt`), extracted to `.spikes/node/node-v24.22.1-win-x64/`.

```
cd .spikes/claude-acp
../node/node-v24.22.1-win-x64/node.exe ../node/node-v24.22.1-win-x64/node_modules/npm/bin/npm-cli.js install @agentclientprotocol/claude-agent-acp@latest
```

npm is isolated with `npm_config_cache=.spikes/npm-cache` and `npm_config_userconfig=.spikes/npmrc-empty`
so the user's npm config is neither read nor written. Result: 0.97.0.

Verify:

```
C:/Users/0x1de/dev/agent-proto/.spikes/node/node-v24.22.1-win-x64/node.exe C:/Users/0x1de/dev/agent-proto/.spikes/claude-acp/node_modules/@agentclientprotocol/claude-agent-acp/dist/index.js --version
```

## Probes

All probes run from the repo root with `python spikes/claude-acp/acp_client.py <scenario>`.
The client strips `ANTHROPIC_API_KEY` and `ANTHROPIC_AUTH_TOKEN` from the child environment (RFC-0009 [0009-069]).

Without `--stub` the probes use the real subscription and need the weekly limit to be clear.
With `--stub` the adapter (and `claude -p`) talk to `stub_anthropic.py`, a local Messages API stub in a thread of the
harness process. The child env gets the fake key `sk-stub-not-real`, `ANTHROPIC_BASE_URL` pointing at the stub, and
`CLAUDE_CONFIG_DIR=.spikes/claude-acp/cfg/<name>` (recreated per run). `~/.claude` is not read or written.
Stub results are observed-with-stub, not real-model results.

| Scenario | Command | Tier | Notes |
|---|---|---|---|
| Q2/Q3 initialize | `init` / `init --stub` | real / stub | `--auth-terminal` declares `auth.terminal: true` |
| Q4 permission | `write --stub --perm allow_once` / `--perm reject_once` | stub | prompt `STUB:WRITE:probe.txt` |
| Q4 default mode, via `_meta` | `write --stub --default-mode acceptEdits` | stub | `_meta` override, expect `currentModeId` still `default` |
| Q4 default mode, via disk | `write --stub --perm allow_once --disk-mode acceptEdits` | stub | writes `cfg/write-stub/settings.json`, empty `_meta` |
| Q4 real | `write --perm allow_once` / `--perm reject_once` | real | BLOCKED by rate limit |
| Q6 cancel, held permission | `cancel --stub` / `cancel --stub --no-answer` | stub | `cancel` real variant without `--stub` |
| Q6 cancel, streaming | `cancel --stub-long` | stub | `STUB:LONG` streams 200 chunks, cancel after first chunks |
| Q7 resume, restart | `resume-stub --method session/load` / `session/resume` | stub | adapter A completes turn, exits; adapter B reloads and recalls |
| Q7 resume, real | `resume-a` then `resume-b --method session/load` (or `session/resume`) | real | BLOCKED by rate limit |
| Q8 fs/terminal | `fs --fs-mode decline` / `fs --fs-mode advertise` | stub | reads `probe.txt`, runs `echo hi`; expect `requests_in` empty |
| Q8 real | `write --fs-mode advertise` | real | BLOCKED by rate limit |
| Q9 process cleanup | `tree --how stdin|terminate|kill|taskkill|ctrlbreak` | real | spawns session, measures `claude.exe` children |
| Q10 headless | `claude-p --stub` / `claude-p` | stub / real | `claude -p ... --output-format stream-json --verbose --no-session-persistence` |

Prompt directives understood by the stub (in a user message containing `STUB:`):
`STUB:WRITE:<name>` (Write tool), `STUB:READ:<name>` (Read), `STUB:BASH:<cmd>` (Bash),
`STUB:LONG` (200 text chunks at 50 ms), `STUB:REMEMBER:<word>` (replies OK), `STUB:RECALL` (echoes the remembered word),
`STUB:PLAIN` (plain "OK"). After a tool result the stub replies `DONE`. Requests without tools get `OK`.

Outputs: `.spikes/claude-acp/out/<scenario>.jsonl` (frame log, redacted), `*.stderr.log`, and `stub-<name>.jsonl` (stub request log).
Work dirs: `.spikes/claude-acp/work/`. Config dirs for stub runs: `.spikes/claude-acp/cfg/`.

Reproduce the stub set in order:

```
python spikes/claude-acp/acp_client.py init --stub
python spikes/claude-acp/acp_client.py write --stub --perm allow_once
python spikes/claude-acp/acp_client.py write --stub --perm reject_once
python spikes/claude-acp/acp_client.py write --stub --perm allow_once --default-mode acceptEdits
python spikes/claude-acp/acp_client.py write --stub --perm allow_once --disk-mode acceptEdits
python spikes/claude-acp/acp_client.py cancel --stub
python spikes/claude-acp/acp_client.py cancel --stub --no-answer
python spikes/claude-acp/acp_client.py cancel --stub-long
python spikes/claude-acp/acp_client.py resume-stub --method session/load
python spikes/claude-acp/acp_client.py resume-stub --method session/resume
python spikes/claude-acp/acp_client.py fs --fs-mode decline
python spikes/claude-acp/acp_client.py fs --fs-mode advertise
python spikes/claude-acp/acp_client.py claude-p --stub
```

## Cleanup

Real probes that create sessions write transcripts under `~/.claude/projects/*spikes-claude-acp-work*`.
Remove only those directories after a real run:

```
rm -rf ~/.claude/projects/*spikes-claude-acp-work*
```

Stub runs write nothing under `~/.claude`.

## Known limits seen

- Subscription rate limit (`errorKind: rate_limit`, HTTP 429) blocks every real model call. The real-tier scenarios
  (`write`, `cancel`, `resume-a`, `claude-p`) cannot complete while it is active.
- The stub is not the real model. Its usage values (123 in / 17 out per call) and its replies are fixed by design.
