# A2A-Bus

Protocol that carries the open A2A v1.0 protocol over a message bus (reference bus: NATS with JetStream), plus the drivers that let Hermes, Claude Code and omp collaborate on one host. The repository is spec-first: RFCs and interop notes define behavior; code follows them.

## Map

| Path | Contents | Owner |
|---|---|---|
| `docs/requirements.md` | Requirements, decisions D1-D23, threat model (historical rationale) | integrator |
| `docs/rfcs/` | RFCs 0000-0015 (normative); start with `0000-process.md` and `0001-architecture.md` | RFC authors (one RFC per author at a time) |
| `docs/interop/` | Researched facts about A2A, Hermes, ACP, omp, Claude Code, NATS | research agents |
| `docs/spikes/` | Observed results of spikes and the live end-to-end run | spike owners |
| `proto/` | Protobuf for A2A-Bus extensions | integrator, with the owning RFC |
| `crates/` | Rust workspace (layers and rules in RFC-0001; each crate has its own AGENTS.md) | crate owners |
| `spikes/` | Reproducible spike scripts and probes (not part of the workspace, not production code) | spike owners |

## Commands

- Build and check: `cargo build --workspace`, `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace --all-features`.
- Live tests are `#[ignore]` and need tools under `.spikes/` (nats-server, stub servers, Node): see `spikes/*/README.md` and `docs/spikes/e2e-m1.md`; run with `-- --ignored`.
- Run a crate's checks on its own while other agents work: `cargo test -p <crate>`, `cargo clippy -p <crate> --all-targets -- -D warnings`.

## Rules

- RFC-0001 sec 4.4 (shared vocabulary) and RFC-0000 (format, requirement IDs) are binding on every RFC and crate. Define a name once; reference it elsewhere.
- Build on A2A v1.0 (`lf.a2a.v1`). Never invent a message for something A2A already models. Protocol additions are A2A extensions.
- Third-party facts come from `docs/interop/`. Keep its `[UNVERIFIED]` markers; do not upgrade a claim without a source.
- Work only inside the files you were assigned. Another agent's RFC changes go through a note to the integrator, not an edit.
- Write ASCII punctuation. No emoji. Wrap nothing by hand; one sentence per line is fine in docs.
- Exception: `tools/diagram_kit.py` may use non-ASCII glyphs (bullets, arrows, dashes) because it draws the PDFs in `docs/architecture/`. Every other file stays ASCII.
- Windows and Linux are both first-class. Never assume POSIX paths, signals or permissions.
- Never put secrets, tokens or provider keys in any file, example or log.
- Do not quote attack strings verbatim in docs or agent files; describe the attack.
- Pending work is tracked in the issue tracker or the integrator's plan, not in agent files. This file holds only static rules.

## Protocol for parallel agents

1. Read `docs/rfcs/0000-process.md`, `docs/rfcs/0001-architecture.md`, then your assigned inputs.
2. Edit only your owned paths. Do not run builds or formatters on shared files.
3. Cross-RFC inconsistencies: pick the RFC that owns the item (RFC-0001 sec 4.5), follow it, and list the inconsistency in your final message.
4. The integrator reconciles, sets statuses and commits.
