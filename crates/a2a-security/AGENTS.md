# a2a-security

Layer L2 (RFC-0001). Depends only on `a2a-types`. Implements RFC-0007 in full, plus the signing headers of RFC-0002, approval decision signing of RFC-0006, and card JWS of RFC-0003.

## Owns
- Ed25519 keys, kids, public-key text encoding (`identity`).
- JCS canonicalisation, base64url, SHA-256 (`canon`, private).
- Trust file parsing and validation, key lifecycle, per-agent signing requirement (`trust`).
- Key storage and file-mode/ACL checks, Unix and Windows (`storage`).
- Envelope signing input, signature block, replay cache, verification order (`envelope`, `replay`).
- Approval book and decision verification (`decision`).
- Capability tokens: issuance, chain verification, use ledger, credentials (`delegation`).
- Card signing and verification (`card`).
- Provenance, untrusted-content framing, redaction (`provenance`, `frame`, `redact`).
- Signing and enforcement modes (`mode`), clock abstraction (`clock`), bus error reasons and gRPC mapping (`error`).

## Provides
- `Signer` and `Verifier` traits; `Ed25519Signer`; `Kid`.
- `TrustStore`, `Role`, `KeyStatus`, `TrustEntry`.
- `Envelope`, `sign_envelope`, `verify_request`, `VerifyContext`, `VerifiedSender`, `MessageClass`.
- `ApprovalBook`, `DecisionOrigin`, `VerifiedDecision`, `DecisionOutcome`, `verify_decision`, `request_digest`.
- `verify_grant`, `VerifiedGrant`, `issue_root`, `issue_child`, `TokenCache`, `resolve_reference`, `UseLedger`, `GrantCredential`, `check_loopback_bind`.
- `sign_card`, `verify_card`, `CardOutcome`, `VerifiedCard`.
- `originate`, `append_hop`, `verify_provenance`, `frame_untrusted`, `redact`.
- `ReplayCache`, `SigningMode`, `Policy`, `McpMode`, `Clock`, `SystemClock`, `ManualClock`, `BusError`, `Reason`, `SecurityError`.

## Uses
- `a2a-types`: generated protobuf types (approval, delegation), identifiers, lifecycle and failure vocabulary.
- Crates: `ed25519-dalek`, `sha2`, `base64`, `serde_json`, `serde_json_canonicalizer`, `toml`, `time`, `regex`, `getrandom`, `prost`, `pbjson-types`, `serde`, `thiserror`.

## Rules
- `#![forbid(unsafe_code)]`. No `unwrap`, `expect`, `panic`, `todo!`, `dbg!`, or `unimplemented!` outside tests.
- Verification order is fixed. Envelopes: status and validity before cryptography; identity before timestamp and signature. Tokens: signature, then chain, then audience and task, then time, then revocation last. Decisions: origin before any approval-book check.
- A partial signature block (1 to 3 of 4 headers) is `SIGNATURE_INVALID`, never unsigned.
- Keys and secrets never appear in `Debug`, errors, logs, or examples. Grant tokens never appear in `Debug` output.
- Test modules are `#[cfg(test)]` followed by `#[allow(clippy::unwrap_used, clippy::expect_used)]`.
- Platform-specific helpers carry `#[cfg_attr(not(unix|windows), allow(dead_code))]`.
- Windows and Linux parity. Each platform's file-permission logic is unit-tested on every platform through pure parsers.
- Every test name carries the requirement ID it guards (`req_<rfc>_<id>_*`).

## Tests guarding requirement IDs
- `canon`: 0007-035, 0007-092.
- `clock`: 0007-022, 0007-034.
- `identity`: 0007-001, 0007-002, 0007-003.
- `trust`: 0007-021, 0007-022, 0007-023, 0007-031, 0007-039.
- `storage`: 0007-010, 0007-011, 0007-012, 0007-013, 0007-014.
- `envelope`: 0007-030, 0007-031, 0007-032, 0007-033, 0007-034, 0007-035, 0007-036, 0007-038, 0007-039, 0007-040, 0007-041, 0007-046.
- `replay`: 0007-041.
- `decision`: 0006-009, 0006-013, 0006-014, 0006-015, 0006-051, 0007-095, 0007-096, 0007-098.
- `delegation`: 0007-090, 0007-092, 0007-093, 0007-094, 0007-100, 0007-101, 0007-102, 0007-103, 0007-104, 0007-105, 0007-106, 0007-107, 0007-108, 0007-109, 0007-110, 0007-111, 0007-112, 0007-113, 0007-114, 0007-116, 0007-118, 0007-119.
- `card`: 0003-040, 0003-041, 0003-042, 0004-039, 0007-050, 0007-051, 0007-052, 0007-053, 0007-054, 0007-055, 0007-057.
- `provenance`: 0007-120, 0007-121, 0007-122, 0007-123, 0007-124.
- `frame`: 0007-124, 0007-125, 0007-128, 0007-132.
- `redact`: 0007-130, 0007-131, 0007-133.
- `error`: 0007-140.
- `mode`: 0007-031, 0007-111.
