# bus-nats

Owns: the NATS JetStream binding of the `bus-api::Bus` seam (RFC-0010 and the bus contract of RFC-0011, M1 profile), and the server-side permission rendering and supervisor provisioning of RFC-0010 sections 4.5-4.9.
Provides: `NatsBus` (implements `bus_api::bus::Bus`), `connect`, `NatsConfig`, `provision`, `permissions`, and the subject and bucket `layout`.
Uses: `bus-api` (the seam, names, values, capabilities), `a2a-types` (`TenantId`, `AgentId`), `async-nats` 0.50.0 (core NATS and JetStream, KV, object store), `sha2` (blob digests), `bytes`.

## Rules

- Every `Bus` operation checks the caller's principal against the connection identity first (0010-078); a mismatch is `PermissionDenied`.
- Subject, bucket and stream names come only from `layout`. Nothing else formats them.
- Payloads move as `Bytes`; no copies on the send or receive path.
- KV reads use direct get (`direct_get_last_for_subject`); payloads are never base64-decoded.
- A KV entry is live unless it carries `Nats-Marker-Reason` or a `KV-Operation` other than `PUT`.
- KV watch snapshots are sorted by key before `SnapshotEnd` (0011-026).
- TTLs are whole seconds on the wire; a sub-second TTL is refused with `INVALID_TTL` rather than rounded (0010-031).
- Write headers for KV and events are built only in `kv.rs`, `stream.rs` and `wire.rs`.
- KV buckets and streams are not named by RFC constants: a bucket is resolved from its backing `KV_<bucket>` stream (existence and `max_message_size`), so caller-named buckets work.
- Principal of a stream delivery: the owner token of the subject (`a2a.v1.<tenant>.ev.<agent>.<task>` and `a2a.v1.<tenant>.audit.<identity>`), the only publisher the server lets write it (0010-060). No header sets or strips the principal; a client-stamped `a2a-principal` header is ignored (0011-040). Other subjects deliver with no principal.
- Permission denials reach the binding as server events (`Permissions Violation`). `denial::guard` turns them into `PermissionDenied` for publish, request, consumer creation and stream or bucket lookup. A missing bucket is `PermissionDenied`, indistinguishable from denial (0011-044; RFC feedback).
- A consumer closed by `Bus::close` fails `next` with `CONSUMER_CLOSED` (0011-019).
- Blob quota (0011-070): the artifact stream's `max_bytes` is the quota plus object-store overhead, and `put` refuses a blob that would exceed the stream's stored bytes with `Overloaded`, storing nothing. The retry hint is `NatsBus::with_overload_retry_after` (default `errors::OVERLOAD_RETRY_AFTER`); quota accounting is not defined by the RFC (RFC feedback).
- Provisioning runs only as identity `supervisor`; an object that already exists is left as provisioned (a repeat create re-reserves stream bytes on the server).
- `BusControl` (fault injection) is not implemented by this binding; it is for the conformance harness only.
- Edition 2024, `#![forbid(unsafe_code)]`, no `unwrap`/`expect` outside tests, Windows and Linux parity.
- Requirement IDs appear in test names (`req_0010_*`, `req_0011_*`). Live-server tests are `#[ignore]`.

## Requirement tests

- 0010-011, 0010-060, 0010-062..064: `permissions` rendering (roles and subjects).
- 0010-015: payload budget (`wire`, `settle`).
- 0010-024, 0010-025, 0010-027, 0010-030, 0010-031, 0010-035, 0010-054, 0010-055: stream, bucket and archiver configuration (`provision`).
- 0010-078: principal from inbox prefix and subject (`principal`).
- 0011-011, 0011-015: request outcomes and dispatch (`request`, `settle`). `request` passes its own `timeout` to the async-nats request; the connection-wide `request_timeout` is a default only and never caps a call.
- 0010-031: KV live/marker classification and TTL header (`kv`).
- 0010-031 live: `req_0010_031_ttl_expiry_then_create_recreates`; 0010-034 live: `req_0010_034_watch_snapshot_then_live_without_gap`; 0010-078 live: `req_0010_078_unlisted_nkey_refused`; 0010-024 live: `req_0010_024_provision_is_idempotent`; 0011-011 live: `req_0011_011_request_honors_call_timeout_over_connection_default`.

## Live tests

- `tests/live.rs` runs against a real `nats-server` (2.15.x). `tests/support/server.rs` starts one per test on a free port in a unique temp dir and removes it on drop; the binary comes from `NATS_SERVER_BIN`, else the local spike path.
- `tests/support/factory.rs` is the `a2a-conformance` `BusFactory` in real-clock mode: one connection per grant principal. `create` resets the suite's streams, `KV_<bucket>` buckets and `OBJ_<artifact>` stores to empty on every call (provisioning is not idempotent across suite runs), then connects as each principal with the suite's overload retry hint. A withheld `blob_store` is enforced by the wrapper (`Unsupported`), since startup refuses an unsupported profile.
- Run with `cargo test -p bus-nats -- --ignored`. The `req_0011_conformance_suite_real_clock` test runs the full suite.
