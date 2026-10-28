# a2a-tui

Layer L5 operator console (RFC-0001 crate table). Binary plus library.

## Owns
- Terminal UI state model, rendering, and keybindings (RFC-0012 sec 4.10).
- Approval card and decision flow on the console side (RFC-0006 sec 4.1-4.3).
- Audit tail view (RFC-0013) and presence view (RFC-0004).
- Display sanitisation (`redact`).

## Provides
- Library: pure state (`model`, `app`), `redact`, `render`, `ui` (terminal loop), the generic
  `driver` (directory sync, event streams, decisions), `audit_feed::AuditFeed`, and
  `operator::load` (console identity: signed or read-only).
- Binary `a2a-tui` (`main.rs`): the only place that names the concrete bus. It parses flags,
  connects two `bus-nats` connections as `tui`, supplies the audit stream, consumer, and filter
  names, and builds the envelope signing hook.
- Headless follow mode: `a2a-tui --follow --seed-file <path>` prints one plain line per change to stdout (agent presence, task state or status text, artifacts, approvals, audit records) and needs no terminal, operator key or trust file. `follow::Follower` does the change detection on the driver's messages; the driver runs unsigned and its command channel is never used, so this mode cannot decide approvals. It uses its own durable audit consumer (`tui-audit-follow`) so it never splits deliveries with an open console. Guarded by `req_0012_follow_*`, `req_0004_follow_*`, `req_0013_follow_*` and `req_0013_log_line_*`. The demo stack streams it into `docker logs` (DEV.md).

## Uses
- `a2a-client` for Directory sync, event streams, `PendingApproval`, `decide_signed` (operator
  decisions, RFC-0007 0007-095), and the envelope signing hook.
- `a2a-audit` (timestamp validity), `a2a-core` (clock), `a2a-security` (operator key and
  trust storage), `a2a-types`, `bus-api`. `bus-nats` in `main.rs` only.

## Static rules
- The library is generic over `bus_api::bus::Bus` and `AuditFeed`. Only `main.rs` may name
  `bus-nats`, and only `main.rs` may spell bus layout strings (audit stream, consumer names).
- Agent-supplied text passes through `redact` before display. No clipboard writes.
- Decisions are gated locally (dwell, connection, expiry, operator identity). A decision whose
  outcome is unknown after send shows `unknown` and is never resent. Only a local approval
  check failure shows `refused`.
- The console's bus principal is `tui`. Paths resolve from the state directory: `--state-dir`,
  then `A2AB_STATE_DIR`, then the platform default (matches a2ad `default_state_dir`). Trust is
  `<state>/trust.toml` (`--trust` overrides) and the operator key is
  `<state>/creds/operator-<K>.key`. Missing or rejected keys make the console read-only.
- The console does not read the a2ad config. An operator whose a2ad config sets
  `[security] trust_file` must pass `--trust` with the same path, or the console signs with a
  key a2ad does not trust.
- No `unsafe`. No `unwrap`, `expect`, `panic`, or `todo!` outside tests.

## Requirements guarded
- req_0012_*: screens, keybindings, read-only mode, approval card, dwell, escaping, disconnect,
  dangerous-mode flag.
- req_0006_*: approval card fields, decision flow, expiry.
- req_0013_*: audit tail, timestamp validity, and redaction of audit reasons and card text.
- req_0004_*: presence labels and offline agents.
- req_0010_063: console connects as the `tui` bus identity.
