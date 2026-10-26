# bus-mem

Owns: the deterministic in-process binding of `bus-api::Bus` and `BusControl`:
streams, consumers with redelivery, requests and replies, KV buckets with TTL and
watch, blobs, grants, and fault injection under a seed.

Provides:
- `MemBus` (implements `Bus`), `MemConsumer`, `MemRequests`, `MemWatch`.
- `MemControl` (implements `BusControl`): virtual clock, fault switches, restart,
  consumer crash, blob corruption.
- `MemConfig`, `StreamConfig`, `Grant`, `ConfigError`, `m1_capabilities()`.
- Every request carries `Request::principal`, the caller's principal. Every stored message
  records its publisher, and `Delivery::principal()` returns it. Headers and payload never
  change either value.

Uses: `bus-api` (seam, types, errors), `a2a-types` (`TenantId`), `bytes`, `sha2`
(blob digests), `tokio` (`sync::Notify` for waiting), and `a2a-conformance` as a
dev-dependency to run the suite against this binding.

## Behaviour that callers rely on

- Time is virtual. It moves only through `BusControl::advance`. Request timeouts and
  `ack_wait` are deadlines on that clock.
- `MemControl` supports every `BusControl` operation, so its methods always return `Ok`
  and never `Unsupported`. The suite runs it with `Clock::Virtual` (`BusFactory::clock`).
- `serve` grants match by exact equality with the pattern passed to `open_server`. A grant
  of `a2a.v1.t1.rpc.>` does not permit serving `rpc.send`; grant each served subject
  pattern exactly.
- Faults draw from one generator seeded by `MemConfig::seed`. A fault draws only when
  its switch is installed. `fault_trace()` records drop, duplicate, partition,
  reorder, delay, restart and crash decisions in order.
- Consumer filters are fixed at creation. Resuming an existing consumer ignores a new
  filter.
- Stream sequences start at 1. Retention drops the oldest messages and advances the
  first retained sequence.
- Dedupe is keyed by `(stream, msg_id)` and kept for `dedupe_window`.
- `term` settles like `ack`.
- Handles carry epochs. A bus restart fails stale handles with
  `Unavailable{RESTARTED}`. A consumer crash fails its handle with
  `Unavailable{CONSUMER_CRASHED}`; a new `consume` resumes and redelivers.
- Gate order per operation: closed, then anonymous, then partition, then capability,
  headers, grant, size.

## Tests and the requirements they guard

- 0011-025 `req_0011_025_same_subject_blocks_later_candidates` (state): same-subject
  order under reordering.
- 0011-061 `req_0011_061_same_seed_same_sequence` (rng): seeded generator is repeatable.
- 0011-071 `req_0011_071_pending_bound_stops_new_candidates` (state) and
  `req_0011_071_zero_max_pending_refused` (`tests/conformance.rs`).
- 0011-014 `req_0011_014_shared_handle_publishes_are_visible_to_other_user`
  (`tests/shared_handle.rs`): two users of one `Arc<MemBus>` see each other's publishes.
- `tests/conformance.rs` runs the full RFC-0011 suite from `a2a-conformance`:
  - 0011-080 `req_0011_080_suite_runs_offline_against_bus_mem`
  - 0011-081 `req_0011_081_report_reproducible_from_seed`
  - 0011-061 `req_0011_061_suite_holds_across_seeds`
  - 0011-082 `req_0011_082_failed_row_withholds_claim`

The suite's per-row checks are the binding's contract tests for every other row of
RFC-0011 that bus-mem declares.
