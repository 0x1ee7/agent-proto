# crates

Owns: the Rust workspace members listed in RFC-0001 sec 4.2.
Provides: libraries and executables for A2A-Bus.
Uses: the RFCs as the specification; `proto/` via `a2a-types`.

## Rules

- Layer and dependency rules are RFC-0001 sec 4.1-4.2 and rules `[0001-001]` to `[0001-006]`. A new crate needs its own `AGENTS.md`, its shim and an RFC-0001 table row in the same change as its `Cargo.toml`.
- A crate's `AGENTS.md` states owns, provides, uses, and which requirement IDs its tests guard.
- Implement requirements by ID; name the ID in the test that guards it.
- Edition 2024, `#![forbid(unsafe_code)]` in every crate. The one exception is a `platform` module in `a2ad` (Windows Job Object FFI) under `#![deny(unsafe_code)]` elsewhere in that crate (RFC-0012). No `unwrap`/`expect` outside tests.
- No allocation or copy in hot paths that can be avoided; envelope handling passes `Bytes`, does not clone payloads.
- Windows and Linux CI parity; no POSIX-only assumptions.
- Interface changes across crates are the integrator's task: new signature, all callers, conformance in one commit.
