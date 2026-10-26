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

## Uses
- `a2a-client` for Directory sync, event streams, `PendingApproval`, `decide`, and the
  envelope signing hook.
- `a2a-audit` (timestamp validity), `a2a-core` (clock), `a2a-security` (operator key and
  trust storage), `a2a-types`, `bus-api`. `bus-nats` in `main.rs` only.

## Static rules
- The library is generic over `bus_api::bus::Bus` and `AuditFeed`. Only `main.rs` may name
  `bus-nats`, and only `main.rs` may spell bus layout strings (audit stream, consumer names).
- Agent-supplied text passes through `redact` before display. No clipboard writes.
- Decisions are gated locally (dwell, connection, expiry, operator identity). A decision whose
  outcome is unknown after send shows `unknown` and is never resent. Only a local approval
  check failure shows `refused`.
- The console's bus principal is `tui`. Keys are loaded for `operator` and missing or rejected
  keys make the console read-only.
- No `unsafe`. No `unwrap`, `expect`, `panic`, or `todo!` outside tests.

## Requirements guarded
- req_0012_*: screens, keybindings, read-only mode, approval card, dwell, escaping, disconnect,
  dangerous-mode flag.
- req_0006_*: approval card fields, decision flow, expiry.
- req_0013_*: audit tail, timestamp validity, and redaction of audit reasons and card text.
- req_0004_*: presence labels and offline agents.
- req_0010_063: console connects as the `tui` bus identity.
