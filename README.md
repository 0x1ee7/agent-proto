# A2A-Bus

The open [A2A](https://a2a-protocol.org) v1.0 protocol carried over a message bus, with drivers that let Hermes, Claude Code and omp collaborate on one host.

A2A-Bus is a custom A2A protocol binding (`urn:a2abus:binding:bus:1`). It keeps the A2A data model (Task, Message, Artifact, AgentCard) and adds the pieces A2A leaves open as A2A extensions: human approval gates, budgets and loop control, scoped delegation, provenance and per-task sequencing. Reference bus: NATS with JetStream; the core is bus-agnostic. A gateway exposes bus agents to stock A2A HTTP clients, and an HTTP driver puts external A2A agents such as Hermes on the bus.

## Layout

| Path | Contents |
|---|---|
| `docs/rfcs/` | The specification: 15 RFCs with requirement IDs and conformance tables |
| `docs/interop/` | Sourced notes on A2A, Hermes, ACP, omp, Claude Code and NATS |
| `docs/spikes/` | Observed behavior on real tools and the live end-to-end run |
| `proto/` | Protobuf schemas for the A2A-Bus extensions |
| `crates/` | Rust implementation (types, bus API and bindings, core, security, audit, workspace isolation, worker, client, MCP peer-call shim, drivers, gateway, supervisor, TUI, CLI, conformance) |
| `spikes/` | Reproducible spike scripts and probes |

## Build and test

```
cargo build --workspace
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Live tests (real `nats-server`, real omp / Claude adapter / Hermes with local stub models) are ignored by default; see `docs/spikes/e2e-m1.md` and `spikes/*/README.md`.

## Status

See `docs/spikes/e2e-m1.md` for what has been verified live and what has not. The RFCs are `Proposed`; implementation requirement coverage is tracked by test names that carry the requirement ID.
