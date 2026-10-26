# A2A-Bus

Protocol that carries the open A2A v1.0 protocol over a message bus (reference bus: NATS with JetStream), plus the drivers that let Hermes, Claude Code and omp collaborate on one host. The repository is spec-first: RFCs and interop notes define behavior; code follows them.

## Map

| Path | Contents | Owner |
|---|---|---|
| `docs/requirements.md` | Requirements, decisions D1-D15, threat model | integrator |
| `docs/rfcs/` | RFCs; start with `0000-process.md` and `0001-architecture.md` | RFC authors (one RFC per author at a time) |
| `docs/interop/` | Researched facts about A2A, Hermes, ACP, omp, Claude Code, NATS | research agents |
| `proto/` | Protobuf for A2A-Bus extensions | integrator, with the owning RFC |
| `crates/` | Rust workspace (layers and rules in RFC-0001) | crate owners |

## Commands

- Docs and protos only so far; no build. Rust workspace commands are added here when `crates/` exists: `cargo fmt --check`, `cargo clippy --workspace -- -D warnings`, `cargo test --workspace`.

## Rules

- RFC-0001 sec 4.4 (shared vocabulary) and RFC-0000 (format, requirement IDs) are binding on every RFC and crate. Define a name once; reference it elsewhere.
- Build on A2A v1.0 (`lf.a2a.v1`). Never invent a message for something A2A already models. Protocol additions are A2A extensions.
- Third-party facts come from `docs/interop/`. Keep its `[UNVERIFIED]` markers; do not upgrade a claim without a source.
- Work only inside the files you were assigned. Another agent's RFC changes go through a note to the integrator, not an edit.
- Write ASCII punctuation. No emoji. Wrap nothing by hand; one sentence per line is fine in docs.
- Windows and Linux are both first-class. Never assume POSIX paths, signals or permissions.
- Never put secrets, tokens or provider keys in any file, example or log.
- Do not quote attack strings verbatim in docs or agent files; describe the attack.
- Pending work is tracked in the issue tracker or the integrator's plan, not in agent files. This file holds only static rules.

## Protocol for parallel agents

1. Read `docs/rfcs/0000-process.md`, `docs/rfcs/0001-architecture.md`, then your assigned inputs.
2. Edit only your owned paths. Do not run builds or formatters on shared files.
3. Cross-RFC inconsistencies: pick the RFC that owns the item (RFC-0001 sec 4.5), follow it, and list the inconsistency in your final message.
4. The integrator reconciles, sets statuses and commits.
