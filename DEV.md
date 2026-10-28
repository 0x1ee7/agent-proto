# Developer commands

Every command needed to build, test, run and inspect each component. Paths are relative to the repository root. Native commands are shown for Windows PowerShell or cmd; the Docker dev environment works on Linux, including Arch Linux under WSL.

## Prerequisites

| Target | Needs |
|---|---|
| Native (Windows or Linux) | Rust 1.98.1 (the Docker images pin `rust:1.98.1`; use the same version natively), `cargo`, `clippy`, `rustfmt`. Python 3 for scripts under `spikes/` and `tools/`. |
| Live tests | `nats-server` v2.15.2. Set `NATS_SERVER_BIN` to its path, or use `.spikes/`. Optional: omp, the Claude Code ACP adapter, Hermes (see the live section). |
| Docker | Docker Engine with Compose v2 and BuildKit. The Dockerfile uses `RUN --mount=type=cache`, so the buildx plugin must be installed. On Arch under WSL, install it as a user plugin at `~/.docker/cli-plugins/docker-buildx`. |
| Diagrams | Python 3 and Microsoft Edge or Google Chrome (headless PDF rendering). |

Never point these commands at your real `~/.omp`, `~/.claude` or `~/.hermes` directories. Live runs use throwaway profiles under `.spikes/` (gitignored).

## Native build, test and lint

```
cargo build --workspace
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all --check
```

Run one crate on its own:

```
cargo test -p <crate>
cargo clippy -p <crate> --all-targets -- -D warnings
```

Crates: `a2a-types`, `a2a-core`, `a2a-security`, `a2a-audit`, `a2a-workspace`, `a2a-worker`, `a2a-client`, `a2a-tui`, `a2a-cli` (binary `a2a`), `a2a-mcp` (binary `a2abus-mcp-shim`), `a2a-conformance`, `bus-api`, `bus-mem`, `bus-nats`, `driver-acp`, `driver-a2a-http`, `gateway-http`, `a2ad` (binary `a2ad`).

## Live tests

Live tests are `#[ignore]` and do not run by default. Run them with `--ignored`:

```
cargo test -p bus-nats --test live -- --ignored
cargo test -p driver-acp -- --ignored
cargo test -p driver-a2a-http -- --ignored
```

They need the tools described in `spikes/nats/README.md`, `spikes/omp-acp/README.md`, `spikes/claude-acp/README.md` and `spikes/hermes-a2a/README.md`. Stub model servers (no real model calls) are tracked in:

- `spikes/omp-acp/stub/stub_openai.py`
- `spikes/claude-acp/stub_anthropic.py`
- `spikes/hermes-a2a/stub_openai.py`

Run each with the arguments its folder's `README.md` documents. The isolated end-to-end run, its ports and its recorded results are in `docs/spikes/e2e-m1.md`.

## a2ad (supervisor)

The supervisor starts the bus, the gateway and the workers from a TOML config. Unknown config keys are rejected.

Native (Windows example; on Linux use `target/debug/a2ad`):

```
target\debug\a2ad.exe check-config --config .spikes\e2e\s1debug.toml
target\debug\a2ad.exe run --config .spikes\e2e\s1debug.toml
target\debug\a2ad.exe status --config .spikes\e2e\s1debug.toml
target\debug\a2ad.exe stop --config .spikes\e2e\s1debug.toml
```

`a2ad run` needs a `[bus]` table whose `sha256` is the digest of the `nats-server` binary. Agent tables are `[[agent]]` with kind `worker`, `gateway` or `external`. `permission_timeout` must exceed the 300 s worker approval expiry (default 330 s).

## a2a (operator CLI)

Subcommands: `discover`, `send`, `watch`, `cancel`, `approvals`, `start`, `stop`, `audit` (with `audit verify`). Run `a2a <command> --help` for each one's arguments.

```
a2a --help
a2a --config <path> audit verify --help
```

Global options: `--config`, `--bus-url`, `--tenant`, `--operator`, `--state-dir`, `--trust`, `--timeout`, `--json`, `--no-color`. They can also come from `A2A_CONFIG`, `A2A_BUS_URL`, `A2A_TENANT`, `A2A_OPERATOR` and `A2AB_STATE_DIR`.

## a2abus-mcp-shim (MCP peer-call server)

Binary built from `crates/a2a-mcp`, enabled with the `runtime` feature. It is an MCP stdio server that an MCP host (Claude Code, omp, Hermes) launches; do not run it by hand. The source is `crates/a2a-mcp/src/main.rs`.

## Docker dev environment

The `Dockerfile` has these stages:

- `nats`: official nats-server v2.15.2 tarball, SHA-256 checked.
- `dev`: Rust 1.98.1 with clippy and rustfmt, python3, and `nats-server` on PATH (`NATS_SERVER_BIN` is set).
- `build`: release binaries `a2ad`, `a2a`, `a2a-tui` and `a2abus-mcp-shim`.
- `runtime`: Debian slim, user `a2a`, entrypoint `a2ad`.
- `fetch` and `demo`: the demo stack, see below.

The source is bind-mounted at `/work`. Build output goes to the `cargo-cache` volume, so Windows `target/` and Linux builds stay separate.

```
docker compose build dev
docker compose run --rm dev cargo build --workspace
docker compose run --rm dev cargo test --workspace --all-features
docker compose run --rm dev cargo clippy --workspace --all-targets --all-features -- -D warnings
docker compose run --rm dev cargo test -p bus-nats --test live -- --ignored
docker compose run --rm dev bash
```

Runtime image with the bus-only config in `deploy/compose/a2ad.toml` (no agents):

```
docker compose --profile stack build a2ad
docker compose --profile stack up a2ad
docker compose --profile stack down
docker compose --profile stack run --rm --entrypoint a2a a2ad --help
docker compose --profile stack run --rm --entrypoint sha256sum a2ad /usr/local/bin/nats-server
```

The last command prints the digest that must match `sha256` in `deploy/compose/a2ad.toml`. To run workers in the container, mount a copy of the config that adds `[[agent]]` tables, and mount each agent's credentials. Credentials are never baked into the image.

## Demo stack (two agents, no credentials)

Runs the whole system in Docker: the bus, two real coding agents (omp 18.12.6 and Claude Code through `@agentclientprotocol/claude-agent-acp` 0.97.0) and the stub model servers they talk to. The agents are downloaded into the image at build time (omp by SHA-256, Node by SHA-256, the adapter by pinned npm version). Their profiles point at local stubs with fake keys, so no account, login or provider key is needed and no real model is called. Works on Linux, including Arch Linux under WSL.

```
docker compose --profile demo build
docker compose up -d demo demo-logs
docker compose logs -f demo-logs
```

Name the services in `up`: a bare `docker compose --profile demo up` also starts the `dev` shell container.

`docker compose logs -f demo-logs` is the system view. Every line is prefixed with its source: `[bus]`, `[supervisor]`, `[omp]`, `[claude]`, `[omp-model]`, `[claude-model]` (component logs under `/state/logs`) and `[events]` (`a2a-tui --follow`: agent presence, task state changes, artifacts, approvals and audit records read from the bus).

Operator console and CLI (both connect as the bus identity `tui`; run them in a second terminal):

```
docker compose exec demo demo-tui
docker compose exec demo demo-a2a send omp "hello" --wait
docker compose exec demo demo-a2a send omp "A2A_SEND:claude:review README.txt" --wait
docker compose exec demo demo-a2a approvals list --pending
docker compose exec demo demo-a2a approvals approve <request-id> --scope once --yes
```

`demo-tui` is `a2a-tui` with the demo paths; `demo-a2a` is `a2a` with the demo config and seed. Approvals can be decided in the console or with the CLI. The stub models act on prompt keywords: `hello` is a plain turn, `WRITE_FILE` makes omp write a file (approval card), and `A2A_SEND:<agent>:<text>` makes the agent call the peer through the delegation tool. Claude's stub uses `STUB:A2A:<agent>:<text>`, `STUB:WRITE:<name>` and `STUB:LONG`; see `spikes/claude-acp/README.md`.

Stop with `docker compose down`; `docker compose down -v` also deletes the state volume (and every other named volume of the project, including the build cache `cargo-cache`).

Files: `deploy/compose/a2ad.demo.toml` (supervisor config), `deploy/compose/demo/` (omp and Claude profiles, adapter manifest, wrapper and log-follower scripts), `spikes/claude-acp/serve_stub.py` (Claude stub server). The console and CLI are one identity because `[supervisor] operator_id` allows only `tui` or `cli`, and the console always connects as `tui`.

## Architecture diagrams

```
python tools/architecture_diagrams.py
```

Regenerates the pages and PDFs under `docs/architecture/`. Needs Edge or Chrome.

## RFC checks

RFC requirement IDs (`[NNNN-xxx]`) are stable and never renumbered. Every MUST or SHOULD needs an ID and a Conformance row. The mechanical checker described in `docs/rfcs/0000-process.md` must report 0 issues. No checker script is committed; it has been run inline.

## Spike scripts

Probes live in `spikes/<name>/` and are not production code. Each folder's `README.md` documents its arguments and expected output. Examples:

```
python spikes/omp-acp/probe.py
python spikes/hermes-a2a/probe.py
python spikes/claude-acp/acp_client.py
```
