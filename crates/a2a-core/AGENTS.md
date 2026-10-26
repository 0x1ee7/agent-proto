# a2a-core

Owns: the A2A-Bus wire layer (RFC-0002): subject grammar, binding headers, request and
reply envelopes, the payload codec and blob references, and the mapping between
`google.rpc.Status` and the closed error taxonomy. Also the task state, budget and
approval logic owned by the CoreTask modules (RFC-0005, RFC-0006).

Provides:
- `subject::{Op, Family, Address, SubjectError, parse, rpc, event, audit, events_of}`:
  subject grammar `a2a.v1.<tenant>.{rpc.<agent>.<op> | ev.<agent>.<task> | audit.<agent>}`.
- `headers::*`: binding header names, value grammar, `validate_all`, `required`,
  `optional`, `parse_ts`, `parse_ttl_ms`, `check_version`, service-parameter mapping.
- `envelope::{Clock, SystemClock, Config, MAX_INLINE_BYTES, inline_limit, WireMessage,
  RequestBody, RequestHead, Inbound, Rejection, Answer, ReplyOptions, ReplyError,
  receive, RequestParts, Outgoing, EncodeError, encode_request, EventParts,
  encode_event, InboundEvent, receive_event, decode_reply, ReplyDecodeError, BlobRef,
  BLOB_SCHEME, STATUS_TYPE}`.
- `errors::{WireError, LocalError, StatusExtras, FieldViolation, error_info, classify,
  retry_after}`: `WireError` is the wire taxonomy (RFC-0002 section 4.8). `LocalError`
  never reaches the wire.
- `task`, `budget`, `approval`: CoreTask modules, documented in their own sources.

Uses: `a2a-types` (ids, `proto::a2a` and `proto::rpc` messages, extension activation),
`bus-api` (`Headers`, `Subject`, `BusError`, reply handles), `bytes`, `prost`,
`pbjson-types`, `serde_json` (JSON encoding only when enabled), `sha2` (blob digests),
`thiserror`. Dev-dependencies: `bus-mem`, `a2a-conformance`.

## Rules

- Layer rule [0001-001]: depends only on `a2a-types` and `bus-api`; testable on `bus-mem`.
- Protobuf is the payload encoding ([0002-035]). JSON is accepted only when
  `Config::accept_json` is set ([0002-037]).
- Every error answer is one `google.rpc.Status` with one `ErrorInfo`, in domain
  `a2a-protocol.org` for A2A errors or `a2abus.v1` for bus errors ([0002-090]).
  Replies carry no `A2AB-Msg-Id`.
- Header values are printable ASCII, at most 1024 bytes ([0002-033]). Every header
  failure maps to `MALFORMED_MESSAGE`.
- Payloads are passed as `Bytes` and never copied on the receive path.
- No `unwrap`/`expect` outside tests. No `unsafe`.
- A `None` clock reading fails closed to `REQUEST_EXPIRED` (see RFC feedback).

## Tests and the requirements they guard

Test names carry the requirement ID (`req_0002_NNN_*`).

- 0002-001, 0002-002: `subject` (`req_0002_001_*`, `req_0002_002_foreign_subjects_are_dropped`).
- 0002-061: `envelope` surfaces `A2AB-After-Seq` as `RequestHead::after_seq` and refuses a malformed value (`req_0002_061_*`).
- 0002-010, 0002-011, 0002-013, 0002-014: `envelope` receive pipeline.
- 0002-020, 0002-021, 0002-023: `subject` op table, `envelope` body matching.
- 0002-030, 0002-031, 0002-032, 0002-033, 0002-034: `headers`.
- 0002-035, 0002-036, 0002-037: `envelope` codec.
- 0002-040, 0002-041, 0002-044: `envelope` replies and events.
- 0002-051, 0002-061: `envelope` reply headers and sequence headers.
- 0002-070, 0002-071, 0002-072: `headers` timestamps and TTL; `envelope` deadline and staleness.
- 0002-080, 0002-081, 0002-082: `headers` version, service parameters; `envelope` extensions.
- 0002-090, 0002-091, 0002-092, 0002-093, 0002-094, 0002-095, 0002-096: `errors`.
- 0002-110, 0002-111, 0002-112, 0002-113: `envelope` inline limit and blob references.

0002-011 is enforced here: `envelope::receive` refuses a request unless its transport principal
(`Request::principal`) names the `A2AB-Sender` agent; `envelope::receive_event` refuses an event
unless its principal is the owning agent of the event subject (0002-006). A missing or anonymous
principal is refused with `PERMISSION_DENIED`. Signatures (RFC-0007) are not implemented here.

## Deviations from RFC text

- An unknown operation token is `WireError::UnknownOperation` (code 12, reason
  `UNSUPPORTED_OPERATION`), following [0002-023], not the `FAILED_PRECONDITION` row of
  the RFC table.
- Service-parameter keys are lowercased on read; original case is not recoverable.
- `decode_reply` accepts JSON when the reply declares it. `receive_event` honours
  `Config::accept_json`.
