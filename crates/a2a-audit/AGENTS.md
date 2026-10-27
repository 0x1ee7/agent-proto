# a2a-audit

Owns: RFC-0013 audit: event building and schema checks, redaction before hashing, the per-emitter hash chain, checkpoints and their signatures, the local spool with gap records, the publish pipeline, the JSONL sink with rotation and retention, the verifier, coalescing of peer rejections, metrics, and health bodies.
Provides: `Pipeline`, `PipelineConfig`, `Publisher`, `EventBuilder`, `Emitter`, `EmitterIdentity`, `AuditSink`, `JsonlSink`, `SinkOutcome`, `Spool`, `GapLog`, `AuditError`, plus the `verify`, `health`, `metrics`, `redact`, `schema`, `clock`, and `testkit` modules.
Uses: `a2a-types` (generated `a2abus` protos, ids, vocabulary), `chrono`, `serde_json`, `sha2`, `base64`, `uuid`, `regex`, `prost`, `pbjson-types`, `thiserror`. Checkpoint signing is injected through the `CheckpointSigner` trait (RFC-0007 seam). The NATS transport is injected through `Publisher`.

## Rules

- No `unsafe`. `unwrap`/`expect`/`panic`/`todo` are denied outside tests.
- Canonical JSON (RFC 8785 JCS) is the only input to hashes and signatures. Do not hash serde output directly.
- Redaction runs before hashing. Secrets never reach the spool, the publisher payload, or the sink.
- Spool and sink files are owner-only on both platforms. Windows uses `icacls`; Linux uses mode `0600`. Both branches must stay unit-tested.
- Integer details that can exceed 2^53 are decimal strings. Numbers and nulls in details are refused.
- Event type checks compare against `AuditEventType::*.as_str_name()`, not hand-typed strings.
- Sequence numbers are per emitter and start at 1. A discarded range is recorded as an `AUDIT_GAP`, never silently skipped.
- `reason` details are lowercase tokens `[a-z0-9_]{1,64}` (table 4.2). `excluded_count` is admitted only for `kind` `copy`, as a decimal string (RFC-0012 0012-074).

## Requirement IDs guarded

Tests are named `req_0013_NNN_*` inside the module that implements the rule. Integration tests live in `tests/req_0013.rs`.

- 0013-002, 003, 004, 005, 022, 026, 027, 031, 046: `emitter.rs`
- 0013-003, 014, 015, 019: `spool.rs` (including torn-tail repair)
- 0013-007, 008: `schema.rs` (reason tokens, `excluded_count` copy-only decimal string)
- 0013-013, 016, 017, 018, 019, 020, 021: `sink.rs` (duplicates, gaps, rotation, UTC day boundary, retention)
- 0013-020: `fsutil.rs` (`icacls` argument construction)
- 0013-022, 023, 024, 025: `redact.rs`
- 0013-026, 027, 028, 029: `verify.rs`
- 0013-030, 031: `correlation.rs`
- 0013-033, 036, 037: `health.rs`
- 0013-034, 035: `metrics.rs`
- 0013-047: `coalesce.rs` and `tests/req_0013.rs`
- 0013-050: `schema.rs` (SUBMISSION_DROPPED is worker-only)
- 0013-051: `gaplog.rs` and `sink.rs` (`JsonlSink::report_gaps` writes a `warn` record; consumers never write `AUDIT_GAP`)
- 0013-003, 013, 014: `tests/req_0013.rs` (publish payload, retained-on-failure, reopen continuity)

Not implemented here, owned by other crates or the listener/CLI: 0013-006 (bus fixture), 0013-009 to 0013-012 (bus transport), 0013-032 and 0013-038 to 0013-040 (logging; only the consumer gap warning in `gaplog.rs` exists, and log rotation under 0013-039 is not done), listener wiring for 0013-033/036/037, and the `audit verify` CLI.
