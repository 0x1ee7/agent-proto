# omp-acp spike

Reproduces `docs/spikes/omp-acp.md`. Run from the repo root on Windows with `omp` 18.12.6 on PATH and Python 3.12.

Isolation: all omp runs set `PI_CODING_AGENT_DIR=<repo>/.spikes/omp-acp/agent`, so omp reads the spike's own config and not `~/.omp`. The probe sets this itself. Scratch data goes to `.spikes/omp-acp/` (gitignored).

## 1. Agent dir (one-time, scratch)

Create `.spikes/omp-acp/agent/models.yml` (dummy local key, not a secret):

```yaml
providers:
  stub:
    baseUrl: http://127.0.0.1:18765/v1
    apiKey: stub-local-key
    api: openai-completions
    models:
      - id: stub-model
        name: stub-model
        contextWindow: 32000
        maxTokens: 4096
        input: [text]
```

Create `.spikes/omp-acp/agent/config.yml` with `modelRoles` default, smol, slow, tiny, memory, task, plan and commit all set to `stub/stub-model`.

## 2. Stub server (keep running during the probes)

```
python spikes/omp-acp/stub/stub_openai.py 18765 .spikes/omp-acp/stub/logs
```

Serves `POST /v1/chat/completions` and `GET /v1/models`, logs request bodies to `.spikes/omp-acp/stub/logs/requests.jsonl`. Keywords in the last user message: `WRITE_FILE`, `READ_FILE`, `RUN_ECHO`, `RUN_PING`, `LONG_STREAM`, `CODE_WORD`.

## 3. Scenarios

Basic:

```
python spikes/omp-acp/probe.py init
python spikes/omp-acp/probe.py prompt
python spikes/omp-acp/probe.py nofs
python spikes/omp-acp/probe.py cancel
python spikes/omp-acp/probe.py persist
python spikes/omp-acp/probe.py exit
```

Approval (`--decision` answers `session/request_permission`; `--via config` uses `.spikes/omp-acp/overlays/approval-<mode>.yml`; `--elicit` advertises `elicitation.form` and declines `elicitation/create`):

```
python spikes/omp-acp/probe.py approval --mode always-ask --decision allow --via flag
python spikes/omp-acp/probe.py approval --mode always-ask --decision allow --via flag --elicit
python spikes/omp-acp/probe.py approval --mode always-ask --decision reject --via config
python spikes/omp-acp/probe.py approval --mode always-ask-cached --decision allow --via flag
```

Elicitation (advertises `elicitation.form`; runs write1, read1, bash1, write2, read2):

```
python spikes/omp-acp/probe.py elicit --mode always-ask --answer accept --value Approve
python spikes/omp-acp/probe.py elicit --mode always-ask-cached --answer accept --value "Approve File"
python spikes/omp-acp/probe.py elicit --mode always-ask --answer cancel-notify
python spikes/omp-acp/probe.py elicit --mode always-ask --answer notify-only
python spikes/omp-acp/probe.py elicit --mode always-ask --answer accept --value Approve --perm reject
python spikes/omp-acp/probe.py elicit --mode write
python spikes/omp-acp/probe.py elicit --mode yolo
```

- `--mode {always-ask,always-ask-cached,write,yolo}`
- `--answer {accept,decline,cancel,cancel-notify,cancel-notify-silent,notify-only}`. `cancel-notify` sends `session/cancel` then answers cancel. `notify-only` sends `session/cancel` and leaves the elicitation unanswered. `cancel-notify-silent` leaves it unanswered with no cancel; the probe times out after 40 s.
- `--value` is the enum label sent on accept (default `Approve`).
- `--perm {allow,reject,cancel}` answers the bash `session/request_permission` (default `allow`).
- `--config-yaml` passes an overlay YAML with `--config`.

Resume after restart (Q7). Each run starts omp process A, then process B:

```
python spikes/omp-acp/probe.py resume
python spikes/omp-acp/probe.py resume --same-process
python spikes/omp-acp/probe.py resume --default-session-dir
python spikes/omp-acp/probe.py resume --default-session-dir --close-before
python spikes/omp-acp/probe.py resume --default-session-dir --list-first
python spikes/omp-acp/probe.py resume --default-session-dir --load-cwd other
python spikes/omp-acp/probe.py resume --default-session-dir --agent-dir-b
```

- `--same-process` loads in the same process (no restart).
- `--default-session-dir` omits `--session-dir` (omp default sessions location under `PI_CODING_AGENT_DIR`). Without it the session dir is passed explicitly and load fails after restart (observed).
- `--close-before` calls `session/close` in A before restart.
- `--list-first` calls `session/list` in B before load.
- `--load-cwd {same,other}` picks the cwd used in B.
- `--agent-dir-b` runs B with a second agent dir `.spikes/omp-acp/agent-b` (copies `models.yml` and `config.yml`).

Each run writes `.spikes/omp-acp/results/<name>.json` and `transcripts/*.jsonl`.

## 4. Summarize results

```
python spikes/omp-acp/summarize.py "elicit-*.json"
python spikes/omp-acp/summarize.py "resume-*.json"
```

Prints one block per result file: elicitations and answers, permission requests and answers, per-tool outcome, and load/resume/prompt errors.

## 5. Stop

Stop the stub. The probe kills only its own descendant processes. Verify no leftover `omp` or `stub_openai.py` processes that the spike started.

## Notes

- Earlier `init`/`session` runs before the isolation change read the real `~/.omp`; their results are the same for Q1, Q2 and Q8.
- Real-model runs need an omp credential, which is not available (see the findings file).
