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

```
python spikes/omp-acp/probe.py init
python spikes/omp-acp/probe.py prompt
python spikes/omp-acp/probe.py approval --mode always-ask --decision allow --via flag
python spikes/omp-acp/probe.py approval --mode always-ask --decision allow --via flag --elicit
python spikes/omp-acp/probe.py approval --mode always-ask --decision reject --via config
python spikes/omp-acp/probe.py approval --mode always-ask-cached --decision allow --via flag
python spikes/omp-acp/probe.py nofs
python spikes/omp-acp/probe.py cancel
python spikes/omp-acp/probe.py persist
python spikes/omp-acp/probe.py exit
```

Each writes `.spikes/omp-acp/results/<name>.json` and `transcripts/*.jsonl`. `--via config` uses `.spikes/omp-acp/overlays/approval-<mode>.yml`. `--elicit` advertises `elicitation.form` and declines `elicitation/create`.

## 4. Stop

Stop the stub. The probe kills only its own descendant processes. Verify no leftover `omp` or `stub_openai.py` processes that the spike started.

## Notes

- Earlier `init`/`session` runs before the isolation change read the real `~/.omp`; their results are the same for Q1, Q2 and Q8.
- Real-model runs need an omp credential, which is not available (see the findings file).
