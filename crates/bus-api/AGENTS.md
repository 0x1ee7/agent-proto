# bus-api

Owns: the transport-neutral `Bus` seam (RFC-0001 [0001-006]) and its value types,
the error taxonomy, capability declarations with the `a2a-bus-m1` profile check,
and the `BusControl` trait used by simulators and tests.

Provides:
- `bus::{Bus, RequestStream, Consumer, Watch}`: the seam. Methods take the caller's
  `Principal` and return `impl Future + Send`.
- `name::{StreamName, ConsumerName, Key, KeyPrefix, MsgId, Subject, SubjectPattern,
  Bucket, Principal, InvalidName}`: validated names.
- `value::{StreamSeq, Revision, Digest, InvalidDigest}`; `headers::Headers`.
- `message::{PublishAck, Request, Reply, ReplyHandle, Delivery, StartPosition, Entry,
  WatchEvent}`.
- `Request::principal` (public field) and `Delivery::principal()`: the sender principal the
  binding authenticated (RFC-0002 [0002-011]). Headers and payload never set it; `None`
  means the binding cannot attribute the message.
- `error::BusError`: the closed taxonomy. Reason strings are binding-chosen constants.
- `capabilities::{Capabilities, Persistence, Profile, Requirement, A2A_BUS_M1,
  PROFILE_REGISTRY, ProfileError, M1_MIN_DEDUPE_WINDOW, M1_MIN_MESSAGE_SIZE}`.
- `control::{BusControl, Fault, DelayDist, Probability}`: fault and clock control, each
  method fallible (see the rule below).
- `helpers::{fold_duplicate, Append, append_stored}`: caller-side append semantics.

Uses: `a2a-types` (`TenantId`), `bytes` (payloads), `thiserror`.

## Deviations from RFC text

- `BusControl` lives here, not in a simulator crate, because the simulator and the
  conformance driver both need it and the orphan rule forbids implementing a foreign
  trait for a foreign type. Its methods are synchronous.
- Control methods return `Result<_, BusError>`. A binding without a capability reports
  `BusError::Unsupported { capability }` naming the operation; the conformance suite
  marks such rows unexercised, not failed. `seed` is infallible.
- `Delivery::in_progress` extends the invisibility window to `now + ack_wait`. RFC
  Appendix A does not list it.
- `Bus::append` to a stream that does not capture the subject returns `Absent`; no
  dedicated error variant exists for this case.

## Tests and the requirements they guard

- 0011-003 `req_0011_003_header_round_trip_is_exact`
- 0011-031, 0011-032, 0011-034, 0011-035, 0011-037: profile boundaries and refusal
  naming in `capabilities` (`req_0011_031_*`, `req_0011_032_*`, `req_0011_034_*`,
  `req_0011_035_*`, `req_0011_037_*`).
- 0011-051 `req_0011_051_*` in `helpers`: duplicate folds to success; other failures
  stay failures.
- 0010-019, 0010-020, 0010-028 in `name`: subject token, length and stream grammar.

Requirements enforced by types rather than tests: 0011-012 (single-use `ReplyHandle`,
consumed by `reply`), 0011-015 (`Delivery` settled by consuming `ack`, `nak` or `term`),
0011-043 (`BusError` carries no caller data), 0011-044 (single `PermissionDenied`
variant for absent and forbidden resources). Their runtime behaviour is checked by
`a2a-conformance` through `bus-mem`.
