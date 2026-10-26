# RFC-0007: Identity, Signing and Trust

| Field | Value |
|---|---|
| Status | Proposed |
| Depends on | RFC-0001; RFC-0002 (bus headers, error carriage); RFC-0003 (extension activation); RFC-0004 (card fields); RFC-0005 (task states, dedupe, deadlines); RFC-0006 (approval decision shape); RFC-0010 (broker identity mapping) |
| Owner area | Security (identity, signing, delegation, provenance) |
| Related interop notes | `docs/interop/a2a-spec-notes.md` sections 2, 6, 8.3, 11; `docs/interop/hermes.md` sections 5, 6.8, 7, 8.3; `docs/interop/acp-omp-claude.md` sections 2, 3, 5; `docs/interop/nats.md` section 5 |
| Companion protos | `proto/a2abus/v1/delegation.proto`, `proto/a2abus/v1/provenance.proto` |
| Normative language | BCP 14 (RFC 2119, RFC 8174) |

## 1. Summary

Each A2A-Bus participant has an Ed25519 identity whose public key is trusted only through a local, operator-edited allowlist. Envelopes carry an optional signature block on one host (required across hosts), and Agent Cards carry A2A JWS signatures. Delegation uses operator-signed, task-bound, attenuated capability tokens and MCP access through a proxy controlled by the delegator's supervisor, so no ambient authority and no raw credential crosses the bus. Content from anything other than the operator is provenance-marked untrusted data, with enforcement by capability scope and approvals rather than by text framing. Credential handling and redaction rules keep secrets out of envelopes, cards, logs and audit, and every verification failure fails closed.

## 2. Motivation and scope

Three agents (omp, claude, hermes) share one bus. A message from one agent can carry instructions for another (T1), a delegated agent can act with the delegator's authority (T2), a forged or fatigued approval can authorize a write (T7), and a peer can impersonate another agent (T6, T8). This RFC defines the mechanisms that answer "who sent this", "who authorized this", and "where did this content come from".

In scope:
- Agent keys, key storage, the trust allowlist, rotation and revocation.
- Envelope signature blocks and Agent Card JWS signatures.
- Signing rules for operator-approved decisions (the approval payload shape is RFC-0006's).
- Capability tokens for delegation, MCP access mediated by a proxy, and their revocation.
- Provenance metadata, the untrusted-content rule and model-input hygiene.
- Credential handling, redaction, and the error reasons for trust failures.

Non-goals:
- PKI, certificate authorities, or external identity providers (D15).
- Payload encryption in M1 (see 4.4 and Open issues).
- HTTP client authentication to the gateway. That is RFC-0008 (bearer tokens, loopback default).
- Approval UI and decision schema (RFC-0006), audit event schema (RFC-0013), NATS permission syntax (RFC-0010), process supervision and file layout beyond what 4.2 and 4.3 name (RFC-0012).
- Pattern-based prompt-injection detection. Pattern filters are not a security boundary.

## 3. Terminology

Terms below are not defined in RFC-0001.

- **kid**: key identifier of the form `<agent-id>:<K>` (4.1). It names one allowlist entry.
- **Trust file**: the per-host allowlist `trust.toml` (4.3). Entries are called key entries.
- **Operator key**: a key pair with role `operator`, held by the operator UI.
- **Signing block**: the four headers `A2AB-Kid`, `A2AB-Ts`, `A2AB-Nonce`, `A2AB-Sig` (4.4).
- **Signing input**: the byte string defined in 4.4 that the signature covers.
- **JCS**: JSON Canonicalization Scheme (RFC 8785).
- **Signed statement**: a JSON object with a `signature` member signed under 4.6.
- **Root grant**: a capability token issued by `operator` (depth 0).
- **Child grant**: a capability token issued by an agent that received a token for the same task (depth 1 to 3).
- **Redemption**: the first accepted use of a token that counts against `max_uses` (4.8).
- **Grant credential**: a per-grant random secret that authenticates one recipient session to one MCP proxy (4.9).
- **Stdio shim**: a small local executable, started by the recipient's agent as an MCP stdio server, that forwards MCP traffic to the proxy with the grant credential.
- **Trust class**: origin class of content, defined by `provenance.proto`.
- **Untrusted content**: any content whose trust class is not `TRUST_CLASS_OPERATOR` (4.9).

## 4. Specification

### 4.1 Identity and keys

- [0007-001] Every agent id that publishes on the bus MUST have at least one Ed25519 key pair (RFC 8032). M1 uses exactly one active key per agent id per host.
- [0007-002] A kid MUST be `<agent-id>:<K>`, where `<K>` is the base64url encoding (no padding) of the first 8 bytes of SHA-256 over the raw 32-byte public key. Kids are derived, never chosen. Verifiers recompute the kid and reject a mismatch.
- [0007-003] Public keys MUST be encoded as the raw 32-byte value in base64url without padding (43 characters). Other encodings are rejected.
- [0007-004] The only accepted signature algorithm is EdDSA over Ed25519. Any other `alg` value, including `none`, MUST be rejected.
- [0007-005] Key pairs MUST be generated on the host by the supervisor (RFC-0012) at first start of an agent id. Private keys MUST NOT leave the host, be published on the bus, or appear in envelopes, Cards, logs, traces, audit events, error details, process arguments or environment variables.
- [0007-006] The operator key MUST have role `operator` and agent id `operator`. Its private key MUST be held only by the operator UI process (RFC-0012). No key with role `agent` or `gateway` MAY be an operator key.
- [0007-007] Keys MUST NOT be trusted in-band. A message or card that carries key material (`jwk`, `jku`, `x5c`, `x5u`, or a public key field) gains no trust from it. Trust comes only from the trust file (4.3).
- [0007-008] Rotation is manual (D15). The operator adds an entry with a new kid and status `active`, and sets the old entry to status `retired` with `not_after`. Both kids are accepted during the overlap. The default overlap is 24 hours; the operator MAY set up to 30 days.
- [0007-009] A revoked kid (status `revoked`, or entry removed) MUST fail verification for every envelope, card and token, regardless of any issued or signed timestamp. Revocation fails closed.

### 4.2 Key storage

- [0007-010] Per-user configuration lives in `%LOCALAPPDATA%\a2abus\` on Windows and in `$XDG_CONFIG_HOME/a2abus/` (default `~/.config/a2abus/`) on Linux. Private keys live in `keys/` under it, one file per kid, named `<agent-id>.<K>.key.pem` (no `:` in file names, so Windows and Linux paths are identical in form).
- [0007-011] On Linux the `keys/` directory MUST be mode `0700` and key files `0600`. The loader MUST refuse a key file with any group or other permission bit set.
- [0007-012] On Windows a key file MUST have a discretionary ACL granting access only to the current user's SID, with inherited ACEs removed. The loader MUST check the ACL and refuse a file that fails the check. The trust file ACL MUST grant write access only to the current user. Exact checks are implementation detail [INFERENCE: to be verified on Windows during implementation].
- [0007-013] Private keys MUST be stored as unencrypted PKCS#8 PEM (RFC 8410). Protection at rest is the file permission rule above; passphrase protection is an Open issue.
- [0007-014] Key files MUST NOT be located inside any workspace or worktree (T13). The loader MUST refuse a key path under a workspace root.

### 4.3 Trust allowlist

- [0007-020] Each host has one trust file `trust.toml` in the per-user configuration directory (4.2). It is TOML 1.0, begins with `schema = "a2abus.trust/1"`, and is edited by the operator only.
- [0007-021] Each `[[key]]` entry has the fields: `kid` (string, required), `agent` (required agent id), `role` (`agent`, `gateway` or `operator`, required), `tenants` (array, default `["local"]`), `public_key` (required, 4.1), `status` (`active`, `retired` or `revoked`, default `active`), `not_before` and `not_after` (optional RFC 3339 UTC), `max_skew_ms` (integer, default 300000, maximum 3600000), `require_signature` (boolean, default false), `mcp_direct` (boolean, default false). Unknown fields are a load error.
- [0007-022] Load validation MUST check: kid equals the value derived under 4.1 from `agent` and `public_key`; kids are unique; `role = "operator"` only with `agent = "operator"`; `role = "gateway"` only with `agent = "gateway"`; `mcp_direct = true` only with role `agent`. Any violation is a load error.
- [0007-023] At supervisor start an invalid trust file is fatal and the supervisor MUST NOT start. On a runtime reload, an invalid file MUST be rejected; the last valid state stays in force and an audit event is emitted (RFC-0013).
- [0007-024] The supervisor MUST reload the trust file when it changes (debounce 1 second) and on an explicit operator command (RFC-0012). A reload applies to messages verified after it completes.
- [0007-025] When a card for agent id A is verified with kid K, the supervisor MUST pin A to K. A later card for A with a different kid MUST be rejected until the trust file changes. Pins are supervisor state, not bus state (T8).
- [0007-026] Signed traffic from a sender MUST be accepted only if the sender's kid is an allowlist entry in status `active` or `retired` (within `not_after`), and the entry's `tenants` include the subject's tenant. An unknown kid or agent id is rejected with SIGNATURE_KEY_UNKNOWN.

### 4.4 Envelope signatures

- [0007-030] Envelope signing is OPTIONAL to send in M1 on one host. Receivers MUST verify every signature block they receive (L11).
- [0007-031] A sender MUST include the signature block when its entry has `require_signature = true`, and when the destination is on another host or in another tenant.
- [0007-032] The signature block is the four headers `A2AB-Kid`, `A2AB-Ts`, `A2AB-Nonce` and `A2AB-Sig` as carried by RFC-0002. All four MUST be present or none. A partial block is rejected with SIGNATURE_INVALID.
- [0007-033] A receiver MUST reject a message without a signature block when the sender's entry has `require_signature = true`, with SIGNATURE_MISSING.
- [0007-034] The signing input is the UTF-8 bytes of exactly these lines, each terminated by LF (0x0A), in this order. It covers requests only; replies and events are governed by [0007-144]:
  1. `a2abus-sig/1`
  2. `Subject: <full subject the message was published to>`
  3. `A2AB-Msg-Id: <value>`
  4. `A2AB-Sender: <value>`
  5. `A2AB-Kid: <value>`
  6. `A2AB-Ts: <value>`
  7. `A2AB-Nonce: <value>`
  8. `A2A-Version: <value, or empty>`
  9. `A2A-Extensions: <value, or empty>`
  10. `A2AB-Origin: <value, or empty>`
  11. `A2AB-Ttl-Ms: <value, or empty>`
  12. `A2AB-Causation-Id: <value, or empty>`
  13. `A2AB-Payload-Type: <value, or empty>`
  14. `A2AB-After-Seq: <value, or empty>`
  15. `Content-Type: <value, or empty>`
  16. zero or more lines `A2AB-SP-<key>: <value>`, one per `A2AB-SP-<key>` header present on the request, sorted by `<key>` in bytewise order; no lines when none is present
  17. `Payload-SHA256: <value>`

Header values are the exact bytes transmitted. An absent optional header contributes its line with an empty value; line 16 contributes nothing when absent. A value containing CR or LF makes the message invalid. The signed set is exactly these headers. `Payload-SHA256` is not a transmitted header: the receiver computes it over the received payload bytes ([0007-035]). `traceparent` and `tracestate` are not signed and are informational. `A2AB-Origin` is honored only when the verified sender is `gateway` (RFC-0002 [0002-014]); it is signed so a relaying peer cannot be asserted as origin by a non-gateway sender. `A2AB-Ttl-Ms`, `A2AB-Causation-Id`, `A2AB-Payload-Type`, `A2AB-After-Seq` and `Content-Type` are carried as defined by RFC-0002 and are signed when present or absent.
- [0007-035] `Payload-SHA256` MUST be the base64url (no padding) SHA-256 of the payload bytes exactly as published. Verifiers hash the received bytes and MUST NOT re-serialize them.
- [0007-036] `A2AB-Sig` MUST be the base64url (no padding) encoding of the 64-byte Ed25519 signature over the signing input.
- [0007-037] Senders MUST emit `A2A-Extensions` in the canonical order defined by RFC-0003 before signing, so the signed string is unambiguous. Verifiers verify the header bytes as received and MUST NOT reorder them.
- [0007-038] `A2AB-Sender` MUST equal the agent id of the entry for `A2AB-Kid`, and MUST equal the publisher identity the broker established (RFC-0010). A mismatch is rejected with SIGNATURE_IDENTITY_MISMATCH.
- [0007-039] At verification the signing key is checked in this order: (a) the kid is in the allowlist and its entry serves the tenant (026); otherwise reject with SIGNATURE_KEY_UNKNOWN; (b) the status is `active`, or `retired` with now before `not_after`, and now is not before `not_before`; otherwise reject with SIGNATURE_KEY_INACTIVE; a `revoked` entry is rejected with SIGNATURE_KEY_REVOKED; (c) the role is `agent` or `gateway` for peer messages and `operator` for decisions (095, 098); otherwise reject with KEY_ROLE_DENIED; (d) the entry's agent id equals `A2AB-Sender` (038); otherwise reject with SIGNATURE_IDENTITY_MISMATCH.
- [0007-040] `A2AB-Ts` MUST be RFC 3339 UTC with millisecond precision. It MUST lie within `max_skew_ms` of the receiver's clock; otherwise reject with SIGNATURE_TIMESTAMP_SKEW. This check bounds only the signature replay window and does not extend any other window. `A2AB-Ts` is also the reference for message staleness (RFC-0002 [0002-072]) and submission age (RFC-0005 [0005-027]); those rules alone decide staleness and age.
- [0007-041] A receiver MUST store each `(kid, nonce)` for at least twice `max_skew_ms` and reject a repeat with SIGNATURE_REPLAYED. A full store MUST also reject a new envelope with SIGNATURE_REPLAYED (fail closed): it MUST NOT evict live entries and MUST NOT change state. `A2AB-Nonce` is 16 random bytes, base64url encoded.
- [0007-042] Message dedupe on `A2AB-Msg-Id` (RFC-0005) applies in addition to the nonce check. Neither replaces the other. The dedupe store MUST retain each `A2AB-Msg-Id` for at least the staleness horizon of RFC-0002 [0002-072] (maximum TTL 3600000 ms plus 30 s, that is 3630 s), as RFC-0010 [0010-026] does for the event stream.
- [0007-043] Verification MUST complete before delivery to a Backend, any task state change, or any token use. Rejected messages are not delivered and produce an audit event (aggregation: [0007-145]).
- [0007-044] A sender MUST sign the exact bytes it publishes. Outbound redaction ([0007-130] to [0007-133]) happens before signing.
- [0007-045] Signatures do not provide confidentiality. Payload bytes are visible to the broker and to every identity whose broker permissions allow subscribing to the subject. M1 does not encrypt payloads (T10).
- [0007-046] When a signature block is present, a receiver MUST use only signed headers for security decisions. Unsigned `A2AB-*` headers are informational.
- [0007-144] Accepted M1 gap: replies and events carry no signature block in M1, because signing covers requests only ([0007-034]). Their integrity on one host rests on the broker's subject permissions, which are defense in depth and not trusted for integrity (A1; only the Worker that owns a task publishes its `ev` subjects, RFC-0010), and on the single trust domain (A5). Per-agent read isolation on the event stream is not enforceable in M1 NATS: any identity holding a read bundle can read events. Mitigations: the single trust domain (A5); event and audit payloads carry redacted summaries (132, RFC-0013); cross-host or multi-tenant use requires per-event signatures and payload encryption (031; Open issues 6), which need an RFC-0002 change before that profile is enabled.
- [0007-145] A receiver SHOULD aggregate audit events per sender when failures exceed a rate the operator configures (T9).
- [0007-146] Timestamps are not reformatted. A sender MUST sign the `A2AB-Ts` header text exactly as published and MUST compute `Payload-SHA256` over the protobuf bytes as published. A receiver MUST NOT normalize a timestamp (for example its precision or offset) before verifying the signature or the digest. Signed statements (090, 091) are canonicalized as 090 defines, but the timestamp strings inside them, including `issuedAt`, are signed as received and are not reformatted.

### 4.5 Agent Card signatures

- [0007-050] A card published across hosts MUST carry at least one signature in `AgentCard.signatures`. A card published on one host SHOULD carry one (L11, R-S2).
- [0007-051] Each signature is an A2A `AgentCardSignature` (`docs/interop/a2a-spec-notes.md` section 8.3). Its protected header, the base64url encoding of a JSON object, MUST contain `alg` (`EdDSA`) and `kid` (an allowlist kid), and SHOULD contain `typ` (`JOSE`). The members `jku`, `jwk`, `x5c` and `x5u` MUST NOT appear; a consumer that sees one rejects that signature.
- [0007-052] The JWS signing input is `BASE64URL(protected) || "." || BASE64URL(JCS(card))`, where JCS(card) is the RFC 8785 form of the card JSON with the `signatures` member removed. The signature is Ed25519 over those ASCII bytes.
- [0007-053] A consumer MUST verify at least one signature. A signature verifies only if its kid is an active allowlist entry with role `agent`, its agent id equals the card's agent id (RFC-0004 field), and the Ed25519 check passes. A card with no verifiable signature MUST NOT be used for routing or trust decisions; it MAY be displayed as unverified.
- [0007-054] A consumer MUST ignore failing signatures in a card that has at least one valid signature, and SHOULD log them.
- [0007-055] A signature made by a key that is revoked, or retired past `not_after`, MUST NOT be accepted (A2A section 8.3).
- [0007-056] Security claims in a card (for example the approval mode the supervisor recorded for the agent, T15) MUST be trusted only from a card that verified under 4.5.
- [0007-057] Signing does not change card content. RFC-0004 owns the card fields.

### 4.6 Signed statements

- [0007-090] A signed statement is a JSON object with a `signature` member. Its signature is Ed25519 over the JCS form of the object with `signature` removed; the value is base64url without padding. The key field is named by the schema (`issuerKid` for capability tokens).
- [0007-091] Verifiers MUST canonicalize the object as received (the JSON they parsed), not a typed re-serialization of it.
- [0007-092] Signed statements use the proto3 JSON mapping. 64-bit integers are JSON strings. Floating-point values MUST NOT appear in signed statements.
- [0007-093] Verifiers MUST reject a signed statement that contains members its schema does not define (fail closed).
- [0007-094] A signed statement MUST be verified against an active allowlist key whose role the schema requires: `operator` for root grants and approval decisions, `agent` for child grants.

### 4.7 Operator approvals

- [0007-095] When signing is enabled for a tenant, an approval decision (RFC-0006) that authorizes an action requiring operator approval, including delegation, MUST be signed by an operator-role kid. The decision is a SendMessage request, so its envelope signature (A2AB-Sig, [0007-034]) is that signature. With signing disabled (M1 default), decisions MUST be accepted only from the supervisor's local operator channel (RFC-0012) and never from the gateway (L10, T7, T12).
- [0007-096] A signed decision MUST bind the `ApprovalDecision` fields defined by RFC-0006 (`approval.proto`): `request_id`, `task_id`, `option_id`, `nonce` (equal to the request nonce), `decided_at`, `decided_by` and `request_sha256`. `request_sha256` is the lowercase hex SHA-256 of the protobuf bytes of the ApprovalRequest, checked by the owner Worker (RFC-0006 [0006-051]). The signature covers the whole decision, so a change to any field, including `option_id` or `request_sha256`, invalidates it.
- [0007-097] The first accepted decision closes the request. A later decision for the same request id is governed by RFC-0006 [0006-015]: the same option returns the stored result, and a different option is refused with APPROVAL_ALREADY_RESOLVED. A repeated `(kid, nonce)` is rejected with SIGNATURE_REPLAYED under 041, including a byte-identical redelivery; a fresh envelope carrying a later decision for the same request is not SIGNATURE_REPLAYED.
- [0007-098] A decision signed by a key whose role is `agent` or `gateway` MUST be rejected with KEY_ROLE_DENIED. A gateway cannot approve on behalf of anyone (T12).
- [0007-099] The operator UI SHOULD coalesce identical pending approvals and SHOULD cap pending operator prompts per agent across its tasks (default 3) to limit approval fatigue (T7). This per-agent cap is separate from the per-task limit of RFC-0006 [0006-022], which is also 3.

### 4.8 Delegation tokens

- [0007-100] A `CapabilityToken` (`delegation.proto`) is the only delegation credential. It MUST contain no secrets: no provider keys, bus credentials, grant credentials, or upstream MCP credentials. It MAY appear in envelopes. Logs and audit events MUST record only `tokenId` and its digest, never the token body.
- [0007-101] A recipient MUST verify a full token on first receipt and cache it by `tokenId` until its expiry. Later messages on the same task MAY carry only a `DelegationRef` (`delegation.proto`). A `DelegationRef` whose token is neither cached nor included is rejected with CAPABILITY_INVALID.
- [0007-102] A `DelegationRef.tokenDigest` MUST equal `sha-256:` plus base64url over the JCS form of the full token including its signature. A mismatch is rejected with CAPABILITY_INVALID.
- [0007-103] Root grants MUST have `issuer = "operator"`, `depth = 0`, and an operator-role `issuerKid`. They MUST be created only from an approved delegation decision (095). No agent or gateway can create a root grant (T2, T12).
- [0007-104] A child grant MUST be issued by the audience of a valid token T for the same task, and its scope MUST be a subset of T's scope for every resource and action, and its constraints MUST satisfy 148. The child MUST NOT end later than T, MUST NOT be issued before T (147), MUST have `maxUses` no greater than T's, MUST have `depth = T.depth + 1`, and MUST have `ancestors = T.ancestors + [T]`, root first. `depth` MUST NOT exceed 3. A violated chain rule (issuer, depth, ancestors, end time, issue time, `maxUses`) is rejected with CAPABILITY_CHAIN_INVALID; a scope or constraint that widens T is rejected with CAPABILITY_SCOPE_DENIED.
- [0007-105] A chain MUST NOT name as audience any agent id that already appears in the chain as an issuer or audience (no loops; T3).
- [0007-106] A recipient MUST check, in order: (a) the signature of the token and of every ancestor (090), a non-verifying signature failing with CAPABILITY_INVALID, an issuer kid not in the allowlist for the tenant failing with SIGNATURE_KEY_UNKNOWN, and a role or identity mismatch failing with KEY_ROLE_DENIED or SIGNATURE_IDENTITY_MISMATCH; (b) the chain invariants (104, 105, 147); (c) `audience` equals the recipient's agent id, otherwise CAPABILITY_AUDIENCE_MISMATCH; (d) `taskId` equals the envelope task id, otherwise CAPABILITY_TASK_MISMATCH; (e) receiver clock is no earlier than `issuedAt` minus the issuer's `max_skew_ms` (CAPABILITY_NOT_YET_VALID otherwise), and before `issuedAt` plus `ttlMs` (CAPABILITY_EXPIRED at or after expiry; no grace after expiry); (f) no issuer kid in the chain is revoked or inactive, and no such key is not yet valid or past its `not_after` (CAPABILITY_REVOKED). Failure codes are in 140.
- [0007-107] `maxUses` defaults to 1 when absent. An explicit value MUST be an integer of at least 1; 0, a negative value or a non-integer is rejected with CAPABILITY_INVALID. A redemption is the first accepted use of a token to start an agent session or attach a grant for its task. Further messages and tool calls within that session are governed by scope and by the proxy (114), not counted. Exceeding `maxUses` is rejected with CAPABILITY_USED. Redemption counts live in recipient supervisor state (T4).
- [0007-108] A token authorizes only messages on its `taskId` received by its `audience`. A token presented by another recipient is rejected with CAPABILITY_AUDIENCE_MISMATCH (106(c)); a message on another task is rejected with CAPABILITY_TASK_MISMATCH (106(d)).
- [0007-109] Default deny: an agent's own configuration (its providers, its MCP servers, its workspace) is that agent's authority and is not delegated authority. Any action an agent takes for another agent's task requires a valid token that scopes that action (T2, T14).
- [0007-110] When a kid is revoked or becomes inactive, every token it issued and every token whose chain contains it is invalid from that moment (computed at verification from the chain). Supervisors MUST move affected tasks to the terminal state RFC-0005 defines for cancellation and MUST stop their proxies (T11).
- [0007-111] Token expiry is hard. When `issuedAt + ttlMs` passes, redemptions and proxy calls under the token fail; the proxy for the grant MUST stop and the grant credential MUST be invalidated (T11).
- [0007-147] A child grant's `issuedAt` MUST be no earlier than its parent's `issuedAt`; an earlier value is rejected with CAPABILITY_CHAIN_INVALID (104, 106(b)).
- [0007-148] Constraint narrowing, v1: every constraint key of the parent MUST exist in the child. A parent array constraint MUST narrow to a subset of its items. Every other parent constraint value (number, string, boolean or object) MUST equal the child's value. v1 narrowing is equality or array-subset only: numeric and string bounds are not narrowable, so a child cannot change them. Extra child keys are allowed. A parent with no constraints allows any child constraint. A child that violates this is rejected with CAPABILITY_SCOPE_DENIED.

### 4.9 MCP access and proxies

- [0007-112] A scope entry whose resource begins `mcp:` describes MCP server access. Its mode defaults to `MCP_MODE_PROXY` (`delegation.proto`).
- [0007-113] In proxy mode the supervisor on the host that holds the upstream MCP configuration MUST start one proxy per grant. The proxy MUST listen on loopback only (127.0.0.1 or ::1, ephemeral port), MUST hold the upstream credential, and MUST be the only component that talks to the upstream server.
- [0007-114] The proxy MUST enforce the token's scope (resource, actions, constraints), mode, expiry and `maxUses` (counted at redemption, 107). It MUST log each call with `tokenId` and reject everything else with an MCP-level error.
- [0007-115] The proxy MUST NOT return upstream credentials, token bodies or grant credentials in tool results or errors, and MUST NOT forward any recipient-supplied credential upstream.
- [0007-116] Each grant has a grant credential: a random 256-bit secret generated by the supervisor, bound to one `tokenId` and one audience, valid until token expiry or proxy stop. It MUST travel only over supervisor local IPC and the recipient session's `mcpServers` entry (interop acp-omp-claude section 2). It MUST NOT appear in envelopes, cards, tokens, provenance, logs or audit.
- [0007-117] The recipient's driver MUST insert the grant into `session/new` `mcpServers`. The preferred entry is a stdio shim: absolute `command`, `args` `["--grant", "<grant-id>"]`, and `env` with `A2ABUS_MCP_GRANT` set to the grant credential. stdio is required of every ACP agent. An `http` entry with an `Authorization: Bearer` header MAY be used only when the agent advertised `mcpCapabilities.http` (not verified for omp or claude; see Open issues).
- [0007-118] Direct mode (`MCP_MODE_DIRECT`) is allowed only when the recipient's entry sets `mcp_direct = true`. The supervisor issues a scope-limited upstream credential expiring with the token, delivered through the path in 116, never on the bus.
- [0007-119] Cross-host MCP delegation is denied in M1 in both modes. A grant's proxy and grant credential are torn down when the task reaches a terminal state (RFC-0005), whichever comes first with expiry (111) or revocation (110).

### 4.10 Provenance and untrusted content

- [0007-120] Every Message and Artifact produced or relayed by an agent MUST carry `Provenance` metadata under `urn:a2abus:ext:provenance:1` (`provenance.proto`). Text parts MUST NOT be rewritten to carry provenance (R-S7).
- [0007-121] Each relaying agent MUST append one `HopRecord` and MUST NOT alter earlier entries. The origin fields are set by the originating component. The gateway (RFC-0008) sets `originAgent` to the mapped peer id and `trustClass` to `TRUST_CLASS_REMOTE_PEER` for an authenticated peer, or `TRUST_CLASS_EXTERNAL_HTTP` otherwise.
- [0007-122] A message with more than 8 hops MUST be rejected with PROVENANCE_TOO_DEEP.
- [0007-123] When `contentDigest` is present, a receiver MUST recompute it over the JCS form of the `parts` array as received and reject a mismatch with PROVENANCE_INVALID.
- [0007-124] Any content whose trust class is not `TRUST_CLASS_OPERATOR` is untrusted input. Model-facing components MUST treat its text as data: it MUST NOT be able to change approvals, grants, identities or scopes. Enforcement is by capability scope and approval gates (T1, R-S7); text framing is hygiene only.
- [0007-125] Before untrusted text reaches a Backend, the worker runtime MUST: (a) reject NUL and control characters other than tab, LF and CR; (b) reject text larger than 262144 bytes (256 KiB), measured before redaction, the limit of RFC-0002 [0002-110]; (c) apply redaction (132); (d) wrap the text in a data block with a fresh 128-bit random boundary per message. The block's header line states the trust class and origin agent id. The text inside is unmodified. If the boundary occurs in the text, a new boundary is generated.
- [0007-126] Framing MUST be applied exactly once, by the component adjacent to the model. The gateway and drivers MUST NOT add framing to content they forward. Hermes frames content it receives from remote peers itself (`docs/interop/hermes.md` section 7), so Hermes-bound content is framed by Hermes and omp- or claude-bound content is framed by the worker.
- [0007-127] Tool and MCP results are `TRUST_CLASS_TOOL_OUTPUT`. They are framed under 125 when they re-enter a model context.
- [0007-128] Framing applies to model input only. It MUST NOT alter stored, signed or forwarded bytes.
- [0007-129] Approval prompts and audit events MUST show the origin agent, trust class and hop chain of the content that triggered the request.

### 4.11 Redaction and credential handling

- [0007-130] Credential-shaped strings are: (a) provider API key formats for configured providers; (b) bearer or `Authorization` values; (c) PEM private key blocks; (d) JWT-shaped strings; (e) cloud access key identifiers; (f) high-entropy tokens: a maximal run of at least 32 characters from `[A-Za-z0-9+/_=-]` whose Shannon entropy over byte frequencies is strictly above 4.5 bits per byte. Patterns (a) to (e) are maintained as data and are not reproduced in this RFC.
- [0007-131] Each match MUST be replaced by `[REDACTED:<class>]`, where `<class>` is one of `provider-key`, `bearer`, `private-key`, `jwt`, `cloud-key-id`, `high-entropy`.
- [0007-132] Redaction MUST be applied to logs, traces, audit events (RFC-0013), outbound bus and HTTP egress content, and to text entering a model context (125).
- [0007-133] Outbound content MUST be redacted before signing (044). Inbound signed payloads MUST NOT be mutated; receivers redact only their copies for logs, audit and model input.
- [0007-134] Bridges (drivers and the gateway) MUST NOT read an agent's provider configuration or environment for keys and MUST NOT forward them. The supervisor passes provider keys only into the environment of the agent process it launches (T5, T14, M1-10).
- [0007-135] Each agent MUST have its own bus credential, generated at first start and stored under the 4.2 rules. The broker MUST permit each credential to publish only on its own sender-scoped subjects and subscribe only to its inbound and inbox subjects (binding-neutral; RFC-0010 maps this to NATS). No credential may hold control-plane access (T5, T6).
- [0007-136] No credential-shaped value MUST appear in envelopes, cards, tokens, provenance, subjects or error metadata. Subjects carry only tenant, agent id and verb (T10).
- [0007-137] Error details and log lines MUST NOT echo the input that failed verification. They MAY echo kids, token ids and request ids.

### 4.12 Errors

- [0007-140] Trust failures MUST be returned as `google.rpc.Status` through the RFC-0002 mapping, with the reasons below in `ErrorInfo.reason`. Authentication-type failures use `UNAUTHENTICATED`: all `SIGNATURE_*` reasons except `SIGNATURE_IDENTITY_MISMATCH`, and `CAPABILITY_INVALID`, `CAPABILITY_EXPIRED`, `CAPABILITY_NOT_YET_VALID`, `CAPABILITY_REVOKED`, `CAPABILITY_CHAIN_INVALID`. Authorization-type failures use `PERMISSION_DENIED`: `SIGNATURE_IDENTITY_MISMATCH`, `KEY_ROLE_DENIED`, `CAPABILITY_SCOPE_DENIED`, `CAPABILITY_AUDIENCE_MISMATCH`, `CAPABILITY_TASK_MISMATCH`, `CAPABILITY_USED`. Provenance failures use `INVALID_ARGUMENT`: `PROVENANCE_INVALID`, `PROVENANCE_TOO_DEEP`.
- [0007-141] `ErrorInfo.domain` MUST be `a2a-protocol.org` for errors defined by A2A, with A2A reason strings, and `a2abus.v1` for bus-level errors, including every trust failure in [0007-140] (RFC-0002 [0002-090] owns the domain values). Reason strings are exact and are part of the wire contract.
- [0007-142] Error metadata MAY include `kid`, `tokenId` and `requestId`. It MUST NOT include signature bytes, token bodies, key material or credentials.
- [0007-143] Card verification failures are not RPC errors. The card is treated as unverified (053).

## 5. Interoperability

Stock A2A v1.0 peers. A stock A2A HTTP client sees nothing new on the wire unless it talks to the gateway. Envelope signing is a bus header set and never touches A2A HTTP requests; the gateway (RFC-0008) does not forward `A2AB-*` headers over HTTP. Cards use the A2A JWS form exactly as `docs/interop/a2a-spec-notes.md` section 8.3 describes (JCS over the card minus `signatures`, protected `alg` and `kid`, `typ` `JOSE`). A stock consumer that does not verify signatures parses the card unchanged. A stock consumer that does verify must use JCS as specified; this RFC does not diverge.

Extension metadata. `urn:a2abus:ext:delegation:1` and `urn:a2abus:ext:provenance:1` are unknown to stock peers. Under the extension rules (`docs/interop/a2a-spec-notes.md` section 2), unknown non-required extensions are ignored. This RFC never sets `required`. A sender MUST NOT delegate work to a peer that has not activated the delegation extension (RFC-0003), because that peer would ignore the token and act without authority. Our design therefore never relies on a stock peer enforcing delegation.

Hermes. Hermes sends no `A2A-Extensions` and no metadata (`docs/interop/hermes.md` section 8.3). Its authentication is per-peer bearer tokens with peer identity as the token's name, or an `ip:<addr>` identity for a shared token (section 5). That is an HTTP-edge identity. The gateway maps it to a bus agent id (RFC-0008, L14). Hermes keys are never created or reused for our signatures.

Hermes push signatures. Hermes signs push payloads with HMAC-SHA256 over `X-A2A-Signature` using `A2A_PUSH_SECRET` or the bearer token (`docs/interop/hermes.md` section 6.8). That is a shared-secret MAC for outbound push, not our Ed25519 scheme. We do not reuse those secrets, and the gateway does not treat push MACs as agent identity.

Hermes ingress framing. Hermes wraps inbound remote text in its own untrusted-content framing and filters it (section 7). Our rule 126 means the gateway does not add a second wrap, so Hermes-bound content from omp or claude is framed once, by Hermes. Hermes's filter is pattern-based and is not relied on as a boundary (124).

Hermes gateway ingress example (`provenance.proto`, proto-JSON, under `metadata`):

```json
"urn:a2abus:ext:provenance:1": {
  "originAgent": "hermes",
  "originKid": "gateway:<K>",
  "trustClass": "TRUST_CLASS_REMOTE_PEER",
  "receivedVia": "RECEIVED_VIA_GATEWAY",
  "contentDigest": "sha-256:<base64url>",
  "hops": [
    {"agent": "gateway", "kid": "gateway:<K>", "ts": "2026-10-26T19:32:10.000Z", "taskId": "task-0190f3b0"}
  ]
}
```

Hermes error codes. Hermes returns HTTP 401 (`-32050`), 403 (`-32052`) and 429 (`-32051`) at its edge (section 1.7). Those are HTTP and Hermes-local codes. Bus trust failures are returned as in 140, and the gateway maps them to HTTP status per RFC-0008. Hermes's `[INPUT_REQUIRED]` text marker (section 8.5) is ordinary content; its provenance is `REMOTE_PEER`.

ACP agents (omp, claude). The driver answers `session/new` with `mcpServers` built per 116 and 117. stdio entries are mandatory for ACP agents (`docs/interop/acp-omp-claude.md` section 2). The `http` form needs `mcpCapabilities.http`, which is [UNVERIFIED] for both agents. omp reads MCP definitions only from the client in ACP sessions (`docs/interop/acp-omp-claude.md` section 3, OMP-MCP), so the grant entry is the only MCP source for a delegated session; this is intended. Permission requests (`session/request_permission`) are unaffected by tokens; the approval mapping is RFC-0006's. Claude's `bypassPermissions` is clamped for root users (`docs/interop/acp-omp-claude.md` section 4). The refuse-start rule and the dangerous-mode list are owned by RFC-0009 (T15); the resolved mode is recorded on the card as an approval-extension parameter (056; field shape to be confirmed by RFC-0003 and RFC-0006).

Session attach. A grant is attached when a session is created (`session/new`). Attaching a grant to an existing session requires `session/load` or `session/resume` with new `mcpServers`, which is [UNVERIFIED] for omp and claude. Default: a new session per grant.

Worked example, signed envelope. Subject `a2a.v1.local.rpc.omp.send`, sender `claude`, task `task-0190f3b0`:

```
A2AB-Msg-Id: 0190f3b0-7a11-7c3e-9d20-4f6b1a2c3d4e
A2AB-Sender: claude
A2AB-Kid: claude:Qm7vE2pX9aA
A2AB-Ts: 2026-10-26T19:30:00.123Z
A2AB-Nonce: k2Vw9sQeLr0xYtB1uN8cZg
A2AB-Sig: <86-character base64url of the 64-byte signature>
A2A-Version: 1.0
A2A-Extensions: urn:a2abus:ext:delegation:1,urn:a2abus:ext:provenance:1
A2AB-Ttl-Ms: 900000
A2AB-Causation-Id: 0190f3a0-5c2e-7d1b-9a40-2e7f8b3c4d5a
A2AB-Payload-Type: lf.a2a.v1.SendMessageRequest
Content-Type: application/x-protobuf
```

The signing input is these lines (LF-terminated), with `<...>` replaced by the exact values:

```
a2abus-sig/1
Subject: a2a.v1.local.rpc.omp.send
A2AB-Msg-Id: 0190f3b0-7a11-7c3e-9d20-4f6b1a2c3d4e
A2AB-Sender: claude
A2AB-Kid: claude:Qm7vE2pX9aA
A2AB-Ts: 2026-10-26T19:30:00.123Z
A2AB-Nonce: k2Vw9sQeLr0xYtB1uN8cZg
A2A-Version: 1.0
A2A-Extensions: urn:a2abus:ext:delegation:1,urn:a2abus:ext:provenance:1
A2AB-Origin: 
A2AB-Ttl-Ms: 900000
A2AB-Causation-Id: 0190f3a0-5c2e-7d1b-9a40-2e7f8b3c4d5a
A2AB-Payload-Type: lf.a2a.v1.SendMessageRequest
A2AB-After-Seq: 
Content-Type: application/x-protobuf
Payload-SHA256: <base64url SHA-256 of the body bytes>
```

Body (`lf.a2a.v1.SendMessageRequest`, proto-JSON; `message` holds the Message), delegation carried once with its token:

```json
{"message":{
  "messageId": "m-0190f3b0-0001",
  "contextId": "ctx-0190f3a0",
  "taskId": "task-0190f3b0",
  "role": "ROLE_AGENT",
  "parts": [{"text": "Summarize the docs search results for the design review."}],
  "metadata": {
    "urn:a2abus:ext:delegation:1": {
      "tokenId": "0190f3b0-1d2e-7c3a-8f4b-6a5d9e8c7b21",
      "tokenDigest": "sha-256:<base64url>",
      "delegator": "claude",
      "taskId": "task-0190f3b0",
      "token": {
        "tokenId": "0190f3b0-1d2e-7c3a-8f4b-6a5d9e8c7b21",
        "issuer": "operator",
        "issuerKid": "operator:<K>",
        "audience": "omp",
        "taskId": "task-0190f3b0",
        "contextId": "ctx-0190f3a0",
        "scope": [
          {"resource": "mcp:docs", "actions": ["tools/call"],
           "constraints": {"tools": ["search_docs"]}, "mcpMode": "MCP_MODE_PROXY"}
        ],
        "issuedAt": "2026-10-26T19:31:00.000Z",
        "ttlMs": "900000",
        "maxUses": 1,
        "signature": "<86-character base64url>"
      }
    },
    "urn:a2abus:ext:provenance:1": {
      "originAgent": "claude",
      "originKid": "claude:Qm7vE2pX9aA",
      "trustClass": "TRUST_CLASS_LOCAL_AGENT",
      "receivedVia": "RECEIVED_VIA_BUS",
      "contentDigest": "sha-256:<base64url>",
      "hops": []
    }
  }
}}
```

Note the `role` string follows the A2A proto-JSON form used in `docs/interop/a2a-spec-notes.md` section 3 [UNVERIFIED for the exact enum spelling in this project's pinned proto]. The `ttlMs` string follows 092.

Worked example, ACP session with the grant (omp receives the grant credential through the driver, not the bus):

```json
{"jsonrpc":"2.0","id":4,"method":"session/new","params":{
  "cwd":"C:\\a2abus\\work\\task-0190f3b0",
  "mcpServers":[{
    "name":"a2abus-grant",
    "command":"C:\\Program Files\\a2abus\\a2abus-mcp-shim.exe",
    "args":["--grant","0190f3b0-1d2e-7c3a-8f4b-6a5d9e8c7b21"],
    "env":[{"name":"A2ABUS_MCP_GRANT","value":"<256-bit grant credential>"}]
  }]
}}
```

On Linux the `cwd` and `command` values are POSIX absolute paths; the shape is identical.

Worked example, trust file (values abbreviated; kids and keys are placeholders):

```toml
schema = "a2abus.trust/1"

[[key]]
kid = "claude:Qm7vE2pX9aA"
agent = "claude"
role = "agent"
tenants = ["local"]
public_key = "<43-character base64url raw public key>"
status = "active"
max_skew_ms = 300000
require_signature = false
mcp_direct = false

[[key]]
kid = "operator:<K>"
agent = "operator"
role = "operator"
tenants = ["local"]
public_key = "<43-character base64url raw public key>"
status = "active"
```

Worked example, Agent Card signature. The protected header decodes to `{"alg":"EdDSA","kid":"omp:Zb1xR4kLq0M","typ":"JOSE"}`:

```json
"signatures": [
  {
    "protected": "<base64url of the header JSON above>",
    "signature": "<86-character base64url>"
  }
]
```

Worked example, trust failure returned to a caller:

```json
{
  "code": 16,
  "message": "signature key revoked",
  "details": [{
    "@type": "type.googleapis.com/google.rpc.ErrorInfo",
    "reason": "SIGNATURE_KEY_REVOKED",
    "domain": "a2abus.v1",
    "metadata": {"kid": "claude:Qm7vE2pX9aA"}
  }]
}
```

Code 16 is `UNAUTHENTICATED` (140).

## 6. Security considerations

Threat mapping (`docs/requirements.md` section 8). "Residual" is what remains after this RFC.

| Threat | Addressed by | Residual |
|---|---|---|
| T1 prompt injection | 107-111 (scope and grants), 109, 120-129 (provenance, untrusted rule, hygiene), 099 (prompt cap) | Content can still mislead a model; scope and approvals bound the damage. Pattern filters are not relied on. |
| T2 confused deputy via delegation | 100-111, 103, 104, 109; chain shown per 129 | None beyond approval quality. |
| T3 ping-pong and cost | 104 (depth 3), 105 (no loops); budget extension (RFC-0006, L9) | Cost within a single allowed chain is bounded by budget, not by this RFC. |
| T4 replay and duplication | 041, 042, 097, 101-102, 107 | Replays are rejected by nonce within the skew window and by `A2AB-Msg-Id` dedupe over the staleness horizon (042). Cross-host skew tuning remains open (Open issues 1). |
| T5 credential leakage | 005, 010-014, 100, 116, 130-137 | Same-OS-user access to key files (see below). |
| T6 impersonation on bus | 026, 038, 030-033, 135 | Without signing in M1 on one host, identity rests on broker auth (R-S1, A5). |
| T7 forged or fatigued approvals | 095-099 (signed decisions, roles, consumed once, prompt cap) | Operator attention remains a human limit. |
| T8 poisoned discovery | 050-057, 025, 026 (pinning, allowlist) | First contact with a new agent relies on the operator adding its key. |
| T9 flooding | 043 (reject before work, aggregate audit); RFC-0005 and RFC-0008 rate limits (R-S6) | Volume attacks still consume broker and CPU before rejection. |
| T10 metadata leakage | 045, 136 | Broker and subscribers see payloads and subjects; no payload encryption in M1. |
| T11 stale authority in partitions | 009, 110, 111 (hard expiry, fail closed) | Clock skew can shorten or extend a window by up to `max_skew_ms` on signature timestamps only. |
| T12 gateway attack surface | 098, 103, 121, 126; loopback default and bearer rules in RFC-0008 | HTTP surface is RFC-0008's. |
| T13 concurrent writes | 014 (keys outside workspaces); worktree isolation (RFC-0012) | None in this RFC. |
| T14 driver as privileged confused deputy | 109, 116, 134, 135; one driver per agent (RFC-0009) | Driver runs as the agent's OS user; compromise of that user is out of scope. |
| T15 approval bypass via agent modes | 056 (verified card claims), 095 (signed decisions); refuse-start and dangerous-mode list (RFC-0009) | Agents started with approval-bypassing modes are only as safe as the RFC-0009 allowlist and refuse-start rule. |

Security requirements (`docs/requirements.md` R-S1 to R-S7) and where this RFC meets them:

| Requirement | Met by | Status in M1 |
|---|---|---|
| R-S1 Per-agent identity key; messages signed independently of the broker | 001-046; 030-033 (optional on one host, required cross-host) | Accepted gap for replies and events: [0007-144] |
| R-S2 Agent Cards signed | 050-057 | Met |
| R-S3 Grants bound to task, request and scope; single use; expiring; default deny | 100-111; 107 (single use), 111 (expiry), 109 (default deny); request binding in RFC-0006 [0006-016] | Met |
| R-S4 Attenuated per-task delegation; no raw long-lived secrets in envelopes | 104 (attenuation); 100 and 116 (no secrets in envelopes) | Met |
| R-S5 Redaction in logs, traces and outbound content | 130-133 | Met; thresholds fixed in 130(f); fixture set per Open issues 9 |
| R-S6 Rate limits and quotas per peer; REJECTED on overload | 043 and 145 (rejection before work, aggregated audit); limits in RFC-0005 and RFC-0008 | Owned by RFC-0005 and RFC-0008 |
| R-S7 Task content untrusted; provenance in metadata; text not rewritten | 120-129 | Met |

Residual risks specific to this design:
- Agents, drivers and the TUI run as the same OS user on one host (A5). Any agent running as that user can read key files, the trust file and bus credential files. Operator signing (095) therefore protects against bus and gateway forgery, not against local code running as the user. This is recorded as an Open issue with a default.
- Grant credentials and proxy traffic are on loopback, reachable by any local process of the same user (113). The grant credential is the only barrier to using a proxy; it expires with the token.
- Signatures do not hide payloads (045).
- Replies and events are unsigned in M1, and event-stream read isolation is not enforceable in M1 NATS. This is an accepted gap ([0007-144]).

Abuse cases are described, not reproduced: forged envelopes, stripped or modified headers, reused nonces and tokens, tokens presented to a different agent or task, chains that widen scope, approval decisions signed by non-operator keys, replays of cards after rotation, and credential-shaped strings inside content are each rejected by the referenced rules.

## 7. Rationale and alternatives considered

- **Ed25519 (EdDSA) over ECDSA P-256.** Deterministic signatures, small keys and fixed encodings reduce implementation error. A2A permits EdDSA for JWS (`docs/interop/a2a-spec-notes.md` section 8.3). ES256 was considered for wider library support on Windows; rejected because one algorithm keeps verification simple.
- **Header-based envelope signing instead of JWS over the message.** A JWS over the body would base64-encode the payload again and would not bind the subject or the A2A-Extensions set. The signing input in 4.4 binds exactly the fields that drive routing and extension behavior.
- **Trust file, no CA.** D15 forbids a CA. A local allowlist is the smallest mechanism that gives pinning, rotation and revocation. SPIFFE/SPIRE and mTLS were rejected: they add infrastructure that is not needed on one host. mTLS remains a transport option for RFC-0010.
- **Operator-signed root grants.** Ties authority to a human decision and makes the gateway unable to grant (T12). Alternative: agent-issued root grants from their config. Rejected because it turns static config into ambient authority (T2).
- **Attenuated child tokens with embedded ancestors.** Verifiable offline, depth-limited, with no fetch from issuer. Macaroons were rejected as a new cryptographic construction with no interop story. Embedding ancestors costs bytes; depth 3 bounds that.
- **Token embedded once, then referenced.** Keeps repeated messages small without a fetch protocol. Alternative: always reference, with a fetch API. Rejected: it adds a request path that can fail in partitions (T11).
- **Redemption counted at session attach, not per call.** Per-call counting would break normal MCP use; scope and proxy enforcement bound calls instead.
- **MCP through a delegator-controlled proxy.** Keeps upstream credentials out of the recipient and out of the bus (L12). Direct mode is kept only as an explicit per-recipient opt-in because it cannot revoke a credential the recipient holds.
- **Framing once, at the model boundary.** Double framing degrades model behavior and is inconsistent with Hermes's own framing. Framing is hygiene; capability scope is the boundary.
- **No payload encryption in M1.** The broker is trusted on one host (A5), and encryption adds key distribution. Deferred with a default in Open issues.
- **Key validity and token issuer keys.** A key that is not yet valid or past `not_after` is rejected with SIGNATURE_KEY_INACTIVE on an envelope (039). A token whose issuer key is not valid is rejected with CAPABILITY_REVOKED (106(f)), and a token whose own `issuedAt` is in the future is CAPABILITY_NOT_YET_VALID (106(e)). Folding these into SIGNATURE_KEY_UNKNOWN was rejected: it would hide a clock or rotation problem from the operator.
- **Full replay store fails closed.** When the `(kid, nonce)` store is full, a new envelope is rejected with SIGNATURE_REPLAYED (041) and no live entry is evicted, because eviction would reopen a replay window. A separate reason code was rejected to keep the reason set in 140 unchanged.
- **Separate reasons for widening and chain errors.** A scope or constraint that widens the parent is CAPABILITY_SCOPE_DENIED (104, 148). Every other attenuation failure (end time, issue time, maxUses, depth, ancestors, issuer) is CAPABILITY_CHAIN_INVALID (104, 147). Operators can then tell a policy violation from a malformed chain.

## 8. Conformance

| ID | Test idea | Level |
|---|---|---|
| 0007-001 | Start agent; assert one active Ed25519 key per agent id per host | MUST |
| 0007-002 | Recompute kid from a fixture key; mismatched kid rejected | MUST |
| 0007-003 | Public key in non-raw or padded base64url rejected at load | MUST |
| 0007-004 | Token or envelope with `alg` other than EdDSA (including none) rejected | MUST |
| 0007-005 | Grep logs, traces, audit, errors, argv and env for key bytes after a run: none | MUST |
| 0007-006 | Operator-role entry with agent other than operator fails load; operator key absent from agent processes | MUST |
| 0007-007 | Message carrying an inline public key does not verify unless the kid is allowlisted | MUST |
| 0007-008 | Rotate key with overlap: both kids verify; after `not_after` old kid fails | MUST |
| 0007-009 | Revoke kid; old envelopes and cards with old timestamps fail | MUST |
| 0007-010 | Key file path resolves per OS; file naming has no `:` | MUST |
| 0007-011 | Linux: key file with mode 0644 refused; dir 0755 refused | MUST |
| 0007-012 | Windows: key file ACL with an extra user SID refused | MUST |
| 0007-013 | Key file not PKCS#8 PEM refused | MUST |
| 0007-014 | Key path under a workspace refused | MUST |
| 0007-020 | Trust file with wrong schema marker refused | MUST |
| 0007-021 | Unknown field in `[[key]]` refused; defaults applied when absent | MUST |
| 0007-022 | Each validation rule has a failing fixture (kid mismatch, duplicate kid, role/agent mismatch, mcp_direct on gateway) | MUST |
| 0007-023 | Invalid file at start: supervisor exits; invalid file at reload: last valid kept and audit event emitted | MUST |
| 0007-024 | Edit file: new rules apply to next verified message, not earlier ones | MUST |
| 0007-025 | Card with new kid for pinned agent rejected until trust file changes | MUST |
| 0007-026 | Signed message from unknown kid or from wrong tenant rejected with SIGNATURE_KEY_UNKNOWN | MUST |
| 0007-030 | Unsigned message accepted in M1 local mode; signed message with bad signature rejected | MUST |
| 0007-031 | Cross-host send without block is not emitted (sender test) | MUST |
| 0007-032 | Only three of four signature headers present: SIGNATURE_INVALID | MUST |
| 0007-033 | `require_signature = true` entry sends unsigned: SIGNATURE_MISSING | MUST |
| 0007-034 | Golden vectors: signing input bytes match for fixtures (requests only), including empty optional headers, `A2AB-After-Seq`, `Content-Type` and `A2AB-SP-<key>` lines | MUST |
| 0007-035 | Modify one body byte: Payload-SHA256 mismatch rejects | MUST |
| 0007-036 | Signature over wrong input rejected; 64-byte length enforced | MUST |
| 0007-037 | Sender with unsorted A2A-Extensions produces failing vector; sorted order passes; verifier verifies received header bytes unchanged | MUST |
| 0007-038 | Sender header differs from broker identity: SIGNATURE_IDENTITY_MISMATCH | MUST |
| 0007-039 | Each check (a)-(d) has a failing fixture: unknown or wrong-tenant kid SIGNATURE_KEY_UNKNOWN; retired past not_after or not yet valid SIGNATURE_KEY_INACTIVE; revoked SIGNATURE_KEY_REVOKED; operator-role key on a peer message KEY_ROLE_DENIED; agent id mismatch SIGNATURE_IDENTITY_MISMATCH | MUST |
| 0007-040 | Timestamp outside skew rejected; inside skew accepted; deadlines unaffected | MUST |
| 0007-041 | Same (kid, nonce) twice: second rejected SIGNATURE_REPLAYED; entry kept for 2x skew; full store rejects a new envelope SIGNATURE_REPLAYED and evicts no live entry | MUST |
| 0007-042 | Same nonce with new msg-id and same msg-id with new nonce: each covered by its check | MUST |
| 0007-043 | Rejected message never reaches Backend; audit emitted | MUST |
| 0007-044 | Outbound body with credential-shaped string: sent bytes are redacted and signed | MUST |
| 0007-045 | Documentation check: no encryption claim in M1 | MUST |
| 0007-046 | Unsigned header altered on signed message does not change decision | MUST |
| 0007-050 | Cross-host card without signature refused by consumer | MUST |
| 0007-051 | Protected header lacking `kid` or with `jku` rejected | MUST |
| 0007-052 | JWS golden vector: JCS of card minus `signatures` verifies | MUST |
| 0007-053 | Card with no verifiable signature shown unverified and not used for routing | MUST |
| 0007-054 | Card with one bad and one good signature accepted; bad logged | MUST |
| 0007-055 | Signature by revoked or expired key rejected | MUST |
| 0007-056 | Approval-mode claim on unverified card ignored by supervisor | MUST |
| 0007-057 | Signing does not alter card fields (diff test) | MUST |
| 0007-090 | Signed-statement golden vector verifies; mutated member fails | MUST |
| 0007-091 | Verifier uses received bytes: reordered members with same values still verify | MUST |
| 0007-092 | Statement with a JSON number for a 64-bit field rejected or canonical | MUST |
| 0007-093 | Statement with an extra member rejected | MUST |
| 0007-094 | Root grant signed by agent-role key rejected | MUST |
| 0007-095 | Approval with signing enabled and no operator signature rejected; M1 decisions only via local channel | MUST |
| 0007-096 | Decision with altered `option_id` or `request_sha256` fails signature verification; missing `request_sha256` refused | MUST |
| 0007-097 | Second decision for the same request id: same option returns stored result; different option APPROVAL_ALREADY_RESOLVED; repeated (kid, nonce) SIGNATURE_REPLAYED | MUST |
| 0007-098 | Decision signed by gateway key rejected KEY_ROLE_DENIED | MUST |
| 0007-099 | Ten identical requests produce one prompt; a fourth distinct pending prompt for one agent across its tasks is held (cap of 3) | SHOULD |
| 0007-100 | Scan envelopes, cards and logs for secrets during delegation: none; token body absent from logs | MUST |
| 0007-101 | Second message with DelegationRef only accepted from cache; cold cache rejected CAPABILITY_INVALID | MUST |
| 0007-102 | Altered token digest rejected | MUST |
| 0007-103 | Root grant from agent key rejected; root from non-decision rejected | MUST |
| 0007-104 | Child with widened scope or constraint CAPABILITY_SCOPE_DENIED; later end, larger maxUses, depth 4 or earlier issuedAt CAPABILITY_CHAIN_INVALID | MUST |
| 0007-105 | Chain re-naming an earlier audience rejected | MUST |
| 0007-106 | Each check (a)-(f) has a failing fixture with its reason | MUST |
| 0007-107 | maxUses 1: second session attach rejected CAPABILITY_USED; tool calls within session not counted; absent maxUses behaves as 1; 0 or non-integer CAPABILITY_INVALID | MUST |
| 0007-108 | Token presented by another recipient CAPABILITY_AUDIENCE_MISMATCH; token on another task CAPABILITY_TASK_MISMATCH | MUST |
| 0007-109 | Delegated action without token refused; own-config action allowed | MUST |
| 0007-110 | Revoke ancestor kid: descendants fail; affected tasks reach terminal state; proxies stopped | MUST |
| 0007-111 | Clock past expiry: redemption and proxy calls fail; proxy process gone | MUST |
| 0007-112 | MCP scope entry without mode behaves as proxy | MUST |
| 0007-113 | Proxy listening address is loopback only; upstream unreachable from recipient | MUST |
| 0007-114 | Call outside scope, mode or expiry refused by proxy with MCP error | MUST |
| 0007-115 | Tool results and errors contain no upstream or grant credential (fixture scan) | MUST |
| 0007-116 | Grant credential absent from envelopes, cards, logs; delivered only via local IPC and env | MUST |
| 0007-117 | session/new contains stdio shim with absolute command; http only with mcpCapabilities.http | MUST |
| 0007-118 | Direct mode refused without `mcp_direct`; with it, credential expires with token | MUST |
| 0007-119 | Cross-host MCP delegation refused; proxy torn down at terminal state | MUST |
| 0007-120 | Every agent message has provenance; text part bytes unchanged | MUST |
| 0007-121 | Relay appends one hop; earlier hops unchanged; gateway sets REMOTE_PEER only when authenticated | MUST |
| 0007-122 | Nine hops rejected PROVENANCE_TOO_DEEP | MUST |
| 0007-123 | Altered parts with stale contentDigest rejected | MUST |
| 0007-124 | Untrusted text instructing approval or scope change does not change any grant or decision | MUST |
| 0007-125 | Worker hygiene: NUL rejected; text over 262144 bytes rejected before redaction; boundary collision regenerates; redaction applied | MUST |
| 0007-126 | Gateway-forwarded Hermes text has no added wrap; one wrap at worker | MUST |
| 0007-127 | Tool output re-entering model is framed with TOOL_OUTPUT class | MUST |
| 0007-128 | Stored and signed bytes equal before and after framing | MUST |
| 0007-129 | Approval prompt displays origin, class and chain (UI test) | MUST |
| 0007-130 | Each class has positive and negative fixtures; patterns not in spec text | MUST |
| 0007-131 | Replacement token format matches exactly | MUST |
| 0007-132 | Credential in log, trace, audit, egress and model input is replaced | MUST |
| 0007-133 | Inbound signed payload bytes unchanged after receiver redaction | MUST |
| 0007-134 | Driver cannot read agent provider config (permission test); provider keys in agent env only | MUST |
| 0007-135 | Agent credential publishing outside own subjects, or subscribing to control plane, denied by broker | MUST |
| 0007-136 | Subject and error metadata fixture scan: no credential-shaped value | MUST |
| 0007-137 | Failed-input echo test: error and log lines do not contain the failed input | MUST |
| 0007-140 | Each reason maps to the listed gRPC code | MUST |
| 0007-141 | ErrorInfo.domain is `a2a-protocol.org` for A2A errors and `a2abus.v1` for bus-level errors; reason strings match exactly | MUST |
| 0007-142 | Error metadata fixture scan: no signature bytes, token body, keys | MUST |
| 0007-143 | Card failure produces unverified status, not RPC error | MUST |
| 0007-144 | Documentation check: reply and event fixtures carry no signature block; accepted-gap text present in section 4.4 and section 6 | MUST |
| 0007-145 | Flood of rejected messages above the configured rate yields aggregated audit events | SHOULD |
| 0007-146 | Envelope whose A2AB-Ts or Payload-SHA256 input was re-serialized fails verification; signed statement verifies received member text unchanged | MUST |
| 0007-147 | Child with issuedAt earlier than its parent's rejected CAPABILITY_CHAIN_INVALID | MUST |
| 0007-148 | Child array constraint with an item outside the parent's items, or a changed numeric or string constraint, rejected CAPABILITY_SCOPE_DENIED; extra child key accepted | MUST |

## 9. Open issues

1. **Cross-host clock skew.** The 300 s default skew is needed for replay bounds, and R-F3 says clocks are not synchronized. Default: `max_skew_ms` 300000 per sender, configurable up to 3600000; deadlines stay relative (RFC-0005). Operators with larger drift raise the per-sender value.
2. **Header names and error domain.** Resolved. RFC-0002 [0002-030] carries `A2AB-Kid`, `A2AB-Ts`, `A2AB-Nonce`, `A2AB-Sig` and `A2AB-Sender`, and RFC-0002 [0002-090] and [0007-141] set the error domains: `a2a-protocol.org` for A2A errors and `a2abus.v1` for bus-level errors.
3. **Same-user residual risk.** Agents, drivers and the TUI share one OS user (A5), so local code can read key and trust files. Default: file ACL and permission rules (4.2), operator key held only by the operator UI and stored in the OS credential store where available [INFERENCE]. Separate OS accounts per agent are evaluated for M2.
4. **Passphrase protection of private keys.** Default: none; file permissions only (4.2 rule 013).
5. **Cross-host MCP.** Denied in M1 (119). Default for M2: TLS with the proxy's identity pinned to a kid. Not designed here.
6. **Payload confidentiality.** Default: none in M1 (045). A later profile may add per-recipient encryption; key distribution would then need its own RFC.
7. **Session resume with new MCP servers.** Whether omp and claude accept `mcpServers` on `session/load` or `session/resume` is [UNVERIFIED]. Default: a new session per grant (`docs/interop/acp-omp-claude.md` sections 2 and 3).
8. **`mcpCapabilities.http` in omp and claude.** [UNVERIFIED]. Default: stdio shim only (117).
9. **Redaction fixtures.** The thresholds in 130(f) are fixed: a run of at least 32 characters from `[A-Za-z0-9+/_=-]` with Shannon entropy above 4.5 bits per byte. Default: fix the fixture set with its positive and negative cases before any release that relies on them.
10. **Approval prompt cap.** Default 3 pending per agent across its tasks (099); RFC-0006 [0006-022] caps 3 per task. The two caps apply independently; both are to be tuned in use.
11. **Approval decision shape.** Resolved by RFC-0006 [0006-051]: `ApprovalDecision.request_sha256` binds the decision to the exact ApprovalRequest, and the operator signature covers the whole decision (095-096). The root grant is created from that approved request (103); its scope is bound through the request's tool, tier, summary, chain and expiry. Binding the minted token bytes is deferred to RFC-0006 section 9 open issue 9 (owner RFC-0007).
12. **Numeric and string bounds narrowing.** Not in v1 (0007-148): a child constraint value must equal the parent's value or, for an array, be a subset. Default: not in v1. Revisit with a defined comparison rule for numeric bounds before any use requires narrowing them.
