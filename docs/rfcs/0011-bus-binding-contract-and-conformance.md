# RFC-0011: Bus Binding Contract and Conformance

| Field | Value |
|---|---|
| Status | Proposed |
| Depends on | RFC-0000, RFC-0001 (seam trait `Bus`, crate map), RFC-0002 (wire contract the bus must carry), RFC-0007 (identity and ACL inputs), RFC-0010 (NATS implementation of this contract) |
| Owner area | Bus binding contract and conformance |
| Related interop notes | `docs/interop/nats.md` (sections on streams, dedupe, KV, consumers, no-responders) |

Normative language follows RFC-0000 section 1 (BCP 14). Requirement IDs are `[0011-NNN]`.

## 1. Summary

RFC-0002 needs a bus with specific delivery properties. This RFC states those properties as a bus-agnostic contract: publish with acknowledgement, request/reply, durable streams with message-id deduplication and replay, per-key key-value storage with TTL and compare-and-set, and a content-addressed blob store. Each implementation declares capabilities. A profile states which capabilities are required, and a process refuses to start when a required capability is missing. A conformance suite, run against the in-memory `bus-mem` implementation and against each binding, checks the contract and fault behaviour. RFC-0010 implements the contract on NATS JetStream; this RFC does not name any product in its normative text.

## 2. Motivation and scope

RFC-0001 rule 0001-001 keeps concrete bus names out of everything except binding crates and executables. RFC-0001 rule 0001-004 requires `a2a-core` to reach the bus only through `bus-api`, testable on `bus-mem` (R-F4). Those rules need a precise contract, otherwise each binding quietly differs and the tests cannot tell a real divergence from a missing feature. The contract also makes the delivery assumptions of RFC-0005 explicit: at-least-once delivery (D3), dedupe keyed by unique message id within a bounded window (R-D1), per-task ordered events (R-D2), and large content by reference with a hash (R-D4).

In scope:

- The operations a bus MUST expose to the crates of RFC-0001 (the `Bus` seam trait).
- Delivery and ordering guarantees the contract makes, and the ones it explicitly does not make.
- Capability declaration, profiles, and fail-closed startup.
- A bus error taxonomy that RFC-0002 maps to wire errors.
- Tenant isolation and permission inputs (enforced per RFC-0007 and RFC-0010).
- The in-memory `bus-mem` reference behaviour and fault injection.
- The conformance suite structure, levels, and how bindings report results.
- Known capability gaps of other bus technologies (MQTT, Zenoh) as design evidence.

Non-goals:

- Subject names, headers and A2A operation mapping (RFC-0002).
- NATS stream, consumer, KV and object-store configuration (RFC-0010).
- Task state, sequence allocation, redelivery policy and idempotent handlers (RFC-0005).
- Signature algorithms and key management (RFC-0007).
- Any bus feature that A2A does not express (D8). The contract exposes no operation that carries data without an A2A meaning.

## 3. Terminology

Terms below are not defined in RFC-0001.

- **Bus binding**: a crate that implements the contract in section 4 for one technology. `bus-mem` and `bus-nats` are the M1 bindings.
- **Capability**: a named, boolean-or-numeric property of a binding, declared at start (section 4.4).
- **Profile**: a named set of required capabilities (section 4.4.2).
- **Durable stream**: a bus-managed, append-only sequence of messages with retention, dedupe and replay for named consumers.
- **Dedupe window**: the interval during which the bus rejects a publish whose message id it has already accepted.
- **Consumer**: a named reader of a durable stream with its own acknowledgement state.
- **Blob store**: content-addressed storage of byte strings keyed by SHA-256 digest, scoped to a tenant.
- **Fault profile**: a set of injected misbehaviours (drop, duplicate, reorder, delay, partition, restart) applied by `bus-mem` for tests.

## 4. Specification

### 4.1 Scope of the contract

[0011-001] Every bus binding MUST implement the operations of section 4.2 that its declared profile (section 4.4.2) requires. A binding MUST NOT expose technology-specific operations to `a2a-core`, `a2a-worker`, `a2a-gateway` or `a2a-supervisor`; such operations stay inside the binding crate.

[0011-002] Bindings MUST present the operations through the `Bus` seam trait defined in RFC-0001 (4.4 and 0001-006). The Rust sketch in Appendix A is informative; the trait names and their meaning are normative, the signatures are not.

[0011-003] The contract is stated in terms of subjects, headers and byte payloads. The contract does not define subject grammar; RFC-0002 section 4.1 does. A binding MUST pass subjects and headers through unchanged, except where its technology requires a reversible encoding that is invisible to the caller.

### 4.2 Operations

[0011-010] `publish(subject, headers, payload) -> PublishAck`: append-or-send a message. For a durable subject, `PublishAck` MUST NOT be returned until the message is stored in a durable stream. A binding MUST report the stream sequence it assigned. A binding MUST expose a kill-before-ack injection (`kill_before_ack`, [0011-061]) that stops the binding after the message is stored and before the `PublishAck` is returned; the caller then receives `Timeout` or `Unavailable`, never success ([0011-064]). A binding that declares `partition_tolerant` MAY expose this behaviour as part of its partition faults instead.

[0011-011] `request(subject, headers, payload, timeout) -> Reply`: send a message and receive exactly one reply or an error. The result is one of `Reply(headers, payload)`, `NoResponder`, `Timeout`, or a `BusError` (section 4.6).

[0011-012] `serve(subject_pattern) -> RequestStream`: receive requests for a subject pattern. Each request exposes its headers, payload, principal ([0011-092]) and a single-use reply handle. The handle is single-use by type: replying consumes it, so a second reply does not compile. A binding MUST NOT offer any other path that sends a second reply to one request.

[0011-013] `append(stream, subject, headers, payload, msg_id) -> PublishAck`: publish into a durable stream with a message id. A binding MUST reject a second append with the same `msg_id` made within the stream's dedupe window with `Duplicate(original_seq)`, and MUST NOT store a second copy. An append to a subject that the named stream does not capture MUST fail with `Absent`.

[0011-014] `consume(stream, consumer_name, filter, start) -> Consumer`: create or attach a named durable consumer. `start` is one of `from_seq(n)`, `from_first`, `new_only`. A consumer MUST deliver messages with stream sequence at or after `from_seq(n)` and MUST NOT silently skip retained messages. The filter and `start` are fixed when the consumer is created. A later `consume` on an existing consumer resumes it with its original filter, and the binding MUST NOT apply a different filter to it.

[0011-015] `Consumer::next() -> Delivery`: each delivery exposes headers, payload, the stream sequence and a delivery attempt counter. The consumer then calls exactly one of `ack()`, `nak(delay)`, or `term()`. `ack()` and `term()` both settle the delivery so that it is never redelivered; the contract defines no dead-letter behaviour for `term()`, and a binding MAY implement `term()` as `ack()`. `in_progress()` extends the ack deadline to now plus `ack_wait`; the delivery stays invisible to other consumers until then. A delivery left unacknowledged for longer than the consumer's `ack_wait` MUST be redelivered (at-least-once, D3).

[0011-016] `kv_put(bucket, key, value, ttl) -> Revision`, `kv_get(bucket, key) -> Entry | Absent`, `kv_create(bucket, key, value, ttl) -> Revision | Conflict` (only if absent), `kv_update(bucket, key, value, expected_revision, ttl) -> Revision | Conflict` (compare-and-set), `kv_delete(bucket, key, expected_revision?)`, `kv_watch(bucket, prefix, start) -> Watch`. A key with a `ttl` MUST disappear from `kv_get` and `kv_watch` after that interval without a delete being issued by the writer. A write that re-creates a key whose previous entry expired, was deleted or was purged MUST apply its `ttl` on that same write, and MUST be fenced by the revision of the marker that the expiry or delete left behind. A binding MUST NOT silently drop the `ttl` on such a re-create (RFC-0010 [0010-031] documents the NATS case).

[0011-017] `blob_put(tenant, bytes) -> Digest` and `blob_get(tenant, digest) -> bytes | Absent`. The digest is the SHA-256 of the bytes. `blob_get` MUST NOT return bytes whose digest differs from the requested one; a binding that detects corruption MUST return `Corrupt` (section 4.6), not the bytes.

[0011-018] `capabilities() -> Capabilities` returns the declaration of section 4.4 and MUST be consistent for the life of the process.

[0011-019] `close()` releases resources. In-flight unacknowledged deliveries are not acknowledged by `close`; they remain pending for redelivery.

### 4.3 Delivery and ordering guarantees

[0011-020] Delivery to a durable consumer is at-least-once. The contract MUST NOT be implemented as at-most-once for durable streams. A consumer MUST be prepared to receive a message more than once and in a different order than a publisher's order across subjects (R-D1, D3).

[0011-021] Within one subject, a binding that declares `ordered_per_subject` MUST deliver messages to one consumer in publish order. A binding without that capability MUST NOT carry RFC-0002 event subjects, because the per-task sequence rule [0002-042] relies on publish order plus acknowledgement.

[0011-022] A publisher that publishes one message at a time and waits for `PublishAck` before the next, on one subject, MUST see its messages stored in publish order on that subject. A binding MUST NOT reorder acknowledged appends on one subject.

[0011-023] Deduplication: `append` with an identical `msg_id` within the dedupe window MUST NOT create a second stored message. Outside the window the bus is free to store a second copy, so the window MUST cover the redelivery horizon of the consumer (section 4.4.1 `dedupe_window`, and [0011-031]).

[0011-024] Replay: a consumer started with `from_seq(n)` on a stream that still retains `n` MUST see every retained message from `n` in stream order. If `n` is no longer retained, the binding MUST return `Unavailable` with reason `RETENTION_EXCEEDED` (section 4.6) rather than starting later silently.

[0011-025] The contract makes no guarantee about ordering across subjects, across streams, or between a request reply and events on other subjects. RFC-0002 MUST NOT depend on any such ordering.

[0011-026] `kv_watch` MUST deliver the current entry of every key matching the prefix as an initial snapshot, then exactly one end-of-snapshot marker, then changes. The marker MUST be delivered when the snapshot is empty. A `Watch` therefore yields `Entry` events and one `SnapshotEnd` event before the first live change. This rule applies to every binding, including the in-memory test binding `bus-mem`. A binding that cannot deliver the marker MUST NOT declare `kv_snapshot_end` (0011-030).

### 4.4 Capabilities and profiles

#### 4.4.1 Capability set

[0011-030] A binding MUST declare the following capabilities. Values are booleans or the numeric/duration fields shown.

| Capability | Type | Meaning |
|---|---|---|
| `headers` | bool | message headers are carried intact |
| `request_reply` | bool | `request` and `serve` are available |
| `no_responder_signal` | bool | a request with no server returns `NoResponder` promptly, not `Timeout` |
| `durable_streams` | bool | `append`, `consume` with acknowledgement |
| `dedupe_window` | duration | minimum guaranteed dedupe window for `append` |
| `replay_from_seq` | bool | `consume(from_seq(n))` for retained n |
| `ordered_per_subject` | bool | section 4.3 [0011-021] |
| `max_message_size` | bytes | largest payload accepted in one message |
| `kv` | bool | key-value operations are available |
| `kv_ttl` | bool | per-key TTL on `kv_put`, `kv_create`, `kv_update` |
| `kv_snapshot_end` | bool | `kv_watch` delivers one end-of-snapshot marker after the initial snapshot, also when it is empty ([0011-026]) |
| `blob_store` | bool | `blob_put`, `blob_get` are available |
| `partition_tolerant` | bool | the binding continues to operate with some peers partitioned and reconciles on heal, without duplicate stored messages beyond the dedupe rule |

[0011-031] A binding that declares `dedupe_window` MUST keep the window for at least the declared duration, for every stream that carries RFC-0002 event subjects. The profile for RFC-0002 requires `dedupe_window >= 3630 s` on those streams: the maximum TTL of 3600 s (RFC-0002 [0002-070]) plus the 30 s staleness allowance of RFC-0002 [0002-072]. This value is the RFC-0002 freshness horizon, not a NATS default.

[0011-032] `max_message_size` MUST be reported as the true limit of the binding after its own framing overhead. The profile for RFC-0002 requires a value of at least 262144 bytes (RFC-0002 [0002-110]).

[0011-033] A binding MUST NOT report a capability it cannot provide under the fault model of section 4.7. A binding that reports `partition_tolerant` MUST pass the partition tests of section 8 (level P).

#### 4.4.2 Profiles

[0011-034] The M1 profile `a2a-bus-m1` requires: `headers`, `request_reply`, `no_responder_signal`, `durable_streams`, `dedupe_window >= 3630 s`, `replay_from_seq`, `ordered_per_subject`, `max_message_size >= 262144`, `kv`, `kv_ttl`, `kv_snapshot_end`, `blob_store`. `partition_tolerant` is recommended, not required, in M1 (section 9 O-2).

[0011-035] A process MUST evaluate its declared profile against the binding's capabilities at start. If a required capability is absent, or a declared value is below the profile minimum, the binding's constructor MUST return an error that names the missing capability. The process MUST refuse to start and MUST log the missing capability by name. It MUST NOT start in a degraded mode that skips the missing feature (T11, fail closed).

[0011-036] A process MUST NOT use a feature that its profile does not require, and a binding MUST NOT advertise a feature in order to pass the profile check.

[0011-037] Profiles are data: a deployment selects one profile name. Adding a profile requires an RFC update, because it changes what the suite must test.

### 4.5 Identity, tenants and permissions

[0011-040] Every bus operation is made on behalf of an authenticated principal (an agent id, `gateway`, `tui`, `cli`, or `supervisor`). A binding MUST attach the principal to every operation for permission evaluation; anonymous operations MUST be rejected unless the deployment explicitly enables them for local development and the binding reports that mode (`anonymous_mode` flag is not in the M1 profile).

[0011-041] Permission checks happen at the bus, before the message reaches a consumer. A principal MUST be able to publish only on subjects it is authorized to publish (RFC-0002 [0002-006], [0002-011]), subscribe or consume only on subjects and streams it is authorized to read, and read KV buckets and blobs only for its tenant. Authorization data is produced by RFC-0007 and enforced by the binding.

[0011-042] Tenant isolation is absolute: a principal authorized for tenant `A` MUST NOT read, write, consume, or fetch blobs in tenant `B`, including through key-value watches and blob digests that happen to be known (RFC-0002 [0002-012]). A binding MUST scope KV buckets and blob namespaces per tenant.

[0011-043] A binding MUST NOT include secret material (credentials, key material, bearer tokens) in any error, log record, watch event or diagnostic that it produces (RFC-0002 [0002-013], T5).

[0011-044] The binding MUST report permission denials as `PermissionDenied` (section 4.6) without revealing whether the denied subject, bucket or digest exists in another tenant. The error taxonomy is the only channel through which a caller observes a denial; the contract defines no denial callback or log for callers. Binding-level audit of denials (for example RFC-0010 [0010-065]) is outside this contract.

[0011-092] Every request and every delivery that a binding hands to a caller carries the principal of its transport peer, as authenticated by the binding. The binding derives it; the sender cannot set it, and neither headers nor payload can carry it. The binding MUST NOT deliver a request or delivery without a principal. The in-memory binding uses the identity the caller is registered under. RFC-0010 [0010-066] states how the NATS binding derives it. RFC-0002 [0002-011] depends on this rule.

### 4.6 Error taxonomy

[0011-050] Every bus operation returns either its success value or one of the following errors. A binding MUST NOT return an untyped error to a caller of this contract. `DuplicateInProgress` and `Internal` are produced only from a binding's own internal state, never by a caller's request; a binding that cannot produce a variant reports that variant's trigger test as `not_applicable` with the reason ([0011-081]).

| Error | Retryable | Meaning | RFC-0002 mapping (section 4.8 there) |
|---|---|---|---|
| `NoResponder` | yes, after backoff | request had no server | local `UNAVAILABLE` (not on wire) |
| `Timeout` | yes, if the operation is idempotent | no reply within the timeout | local `DEADLINE_EXCEEDED` (not on wire) |
| `Unavailable { reason }` | yes | bus or stream temporarily unavailable; `reason` includes `RETENTION_EXCEEDED` (not retryable at the same start) | `OVERLOADED` only when admission refused; otherwise local |
| `Overloaded { retry_after }` | yes | the binding refused for load | `OVERLOADED` with `RetryInfo` |
| `PermissionDenied` | no | principal not authorized | `PERMISSION_DENIED` |
| `PayloadTooLarge { limit }` | no, unless the payload is changed | above `max_message_size` | `PAYLOAD_TOO_LARGE` |
| `Duplicate { original_seq }` | no (success for the caller) | `append` dedupe hit | not on wire; the Worker treats it as already stored |
| `DuplicateInProgress` | yes, after backoff | the same `msg_id` is being stored | `DUPLICATE_IN_PROGRESS` |
| `Conflict { current_revision }` | caller decides | CAS or create failed | not on wire; Worker re-reads and retries (RFC-0005) |
| `Absent` | no | key, blob, or stream capture for the subject does not exist; an `append` to a subject that the named stream does not capture returns `Absent` ([0011-013]) | not on wire; `TASK_NOT_FOUND` when a task record is missing |
| `Corrupt { digest }` | no | blob bytes do not match digest | `INVALID_AGENT_RESPONSE` for artifact references (RFC-0002 [0002-112]) |
| `Unsupported { capability }` | no | operation needs a capability the binding does not declare | not on wire; startup error |
| `Internal` | yes, with backoff | binding fault, detail not exposed | `INTERNAL` locally |

[0011-051] `Duplicate` is a success outcome for an `append` caller. A caller that receives it MUST treat the original stored message as the result and MUST NOT publish again.

[0011-052] `Conflict` is not an error to the protocol layer. The caller MUST re-read and decide (for KV, `expected_revision` mismatch). A binding MUST NOT resolve a conflict by overwriting.

[0011-053] Retry decisions belong to the caller (RFC-0005). The binding MUST NOT retry a non-idempotent operation silently; it MAY retry idempotent internal steps (for example reconnects) and MUST NOT report success for an operation whose outcome is unknown. An `append` whose outcome is unknown after a transport error MUST be retried with the same `msg_id` so that dedupe resolves it (R-D1).

### 4.7 Fault model and fault injection

[0011-060] The contract is defined under the following faults: message loss, duplication, reordering across subjects, delay, consumer crash between delivery and ack, binding restart, and network partition between a process and the bus or between bus replicas.

[0011-061] `bus-mem` (R-F4) MUST implement the contract in process and MUST expose fault injection with these switches: `drop(p)`, `duplicate(p)`, `reorder(window)`, `delay(distribution)`, `partition(set_a, set_b)`, `restart_bus()`, `crash_consumer(name)`, `kill_before_ack()` ([0011-010]). Each switch MUST be deterministic under a seed. The switches are the synchronous control surface of `bus-api` (Appendix A).

[0011-062] Under `duplicate`, `drop` and `reorder`, the in-memory binding MUST still satisfy sections 4.3 and 4.6 as they apply to its declared capabilities. Faults change timing and multiplicity, never the error taxonomy or the stored content.

[0011-063] Under `restart_bus`, a durable stream MUST retain every acknowledged append. Unacknowledged deliveries MUST be redelivered after restart. `bus-mem` declares `durable_streams` as true only when it can meet this rule with its persistence mode; the default test mode is in-memory and MUST declare the persistence mode in its capability declaration, so that tests can state which durability they exercise.

[0011-064] Under `partition`, an operation whose outcome cannot be established MUST return `Timeout` or `Unavailable`, not success. A caller MUST treat expiry as authority loss (RFC-0002 [0002-073], T11).

### 4.8 Tenant and resource limits

[0011-070] A binding MUST enforce the per-tenant and per-principal limits that the deployment configures (message rate, pending deliveries, KV bucket size, blob bytes). When a limit is hit, the binding MUST return `Overloaded` and MUST NOT drop the message silently (R-S6, T9).

[0011-071] The binding MUST bound the number of unacknowledged deliveries per consumer. The bound is a configured value reported in capabilities as `max_pending`; it is not a profile field in M1.

[0011-072] A binding MUST bound the size of a KV value and a blob. The profile does not fix the value; the binding MUST report it in its limits.

### 4.9 Conformance declaration and results

[0011-080] Each binding MUST ship the conformance suite of section 8 as an executable test program run by the `a2a-conformance` crate (RFC-0001). The suite MUST be runnable without a network for `bus-mem`.

[0011-081] A binding declares its result as a conformance report: the profile tested, the binding name and version, the capability values, and, per requirement ID, `pass`, `fail` or `not_applicable` with the reason. A report MUST be reproducible from the seed it records.

[0011-082] A binding MUST NOT claim conformance to a profile if any required requirement of that profile is `fail`. A `not_applicable` result is allowed only where the capability is not declared, or where [0011-050] makes a variant unreachable from outside the binding.

[0011-083] The report is an artifact of the M1 plan (RFC-0014). Its format is defined by the `a2a-conformance` crate, not by this RFC.

### 4.10 Known gaps of other technologies

[0011-090] A binding for a technology that cannot meet the profile MUST NOT be used for the M1 profile. Evidence collected for this RFC is:

- MQTT 3.1.1 and 5 brokers offer per-topic publish but no stored-message replay by name with per-consumer acknowledgement in the base protocol. Replay and dedupe would be implemented outside the broker, which breaks the `durable_streams` and `dedupe_window` claims. [INFERENCE]
- Zenoh offers storage plugins with replay and query. Whether they provide the dedupe window and per-consumer acknowledgement of [0011-013] and [0011-015] is not established in this RFC. [UNVERIFIED]

[0011-091] A future binding for another technology MUST add its capability declaration and pass the suite before any profile names it. A new binding does not change this RFC unless the contract changes.

## 5. Interoperability

**Stock A2A peer.** A stock A2A client never sees the bus. It uses the HTTP gateway (RFC-0008), which speaks A2A JSON-RPC (L15). The bus contract is therefore invisible to A2A peers. The guarantees that matter to them (exactly one terminal state, ordered events, replay from a sequence) come from RFC-0002 and RFC-0005 on top of this contract, and the gateway presents them through A2A's own operations.

**Hermes.** Hermes never connects to the bus (D14). It sees HTTP and SSE, which the gateway produces from event subjects backed by this contract. No Hermes-visible behaviour depends on a bus capability directly. Hermes's reconnect behaviour is covered by the gateway's replay rules (RFC-0008) rather than by this RFC.

**ACP agents (Claude Code, omp).** ACP agents never see the bus. Their drivers (RFC-0009) run inside a Worker, and the Worker is the only bus client for its agent's subjects (RFC-0002 [0002-006]). The driver contract is unaffected by the bus contract, and no ACP message changes.

**Why the contract is still needed.** The interop surfaces above are all produced by the same few bus guarantees. A binding that silently lacked one (for example, no per-key TTL on KV) would make a Hermes-visible SSE stream or a Claude-visible task record wrong without any error. The profile check of [0011-035] makes this a startup failure instead.

## 6. Security considerations

- **T4 Replay and duplication.** The dedupe window (`dedupe_window`, [0011-023], [0011-031]) and the message id on `append` are the bus-level control. The window is sized from the RFC-0002 freshness horizon so that a message accepted by the freshness check cannot be stored twice after the window closes. Nonce and signature checks are in RFC-0007 and RFC-0002.
- **T6 Impersonation.** Every operation carries a principal (section 4.5 [0011-040]). Per-agent bus credentials and subject permissions are enforced by the binding ([0011-041]). Credential issuance is RFC-0007.
- **T9 Flooding.** Overload is reported explicitly ([0011-070]); unacknowledged deliveries are bounded ([0011-071]). Dropping silently is forbidden.
- **T10 Metadata leakage.** The bus can observe subjects, sizes and timing. This contract does not hide them; RFC-0002 keeps content out of subjects. Bindings MUST NOT add content-bearing fields to diagnostics ([0011-043]).
- **T11 Stale authority in partitions.** Under partition an operation with unknown outcome fails rather than succeeding ([0011-064]). A process that cannot confirm its profile refuses to start ([0011-035]).
- **T5 Credentials.** No credential appears in errors, logs or watch events ([0011-043]).
- **Blob integrity.** A digest-keyed read returns verified bytes or `Corrupt` ([0011-017]). A reference from another tenant is not resolvable ([0011-042]).

## 7. Rationale and alternatives considered

**Contract in terms of A2A needs, not technology.** The contract lists what RFC-0002 and RFC-0005 require (dedupe, replay, ordered events, expiring KV, digest blobs). Listing NATS operations would bind every crate to one product, which RFC-0001 rule 0001-001 forbids.

**Capabilities and profiles instead of a single feature set.** A small feature set (for example, only publish and subscribe) would be easy to implement and would silently weaken the guarantees. Capabilities let a binding be honest about what it provides, and profiles let the process refuse what it cannot run. This is the fail-closed choice (T11).

**Dedupe window sized from freshness.** The window could be a fixed default. Sizing it from the RFC-0002 TTL cap plus skew ties the two numbers together, so a change to the TTL cap changes the required window and the profile check catches a binding that does not match.

**Acknowledgement and redelivery at the consumer.** At-least-once with explicit acknowledgement is simpler to reason about than exactly-once. Exactly-once would require cross-system transactions that the bus does not offer, and RFC-0005's idempotent handlers provide the effective outcome.

**Fault injection in `bus-mem`.** Testing against a real broker alone does not make partition and duplicate behaviour reproducible. A seeded in-memory model lets the suite state exactly which fault it injected.

**Errors as a closed taxonomy.** Free-form errors would let each binding invent its own retry semantics. The closed set in section 4.6 gives one retry rule per error.

**Alternatives not chosen.** A bus with only request/reply (no durable streams) was rejected because RFC-0002 requires replay and at-least-once delivery (D2, D3). Using an external database as the event log was rejected for M1 because the bus is required to carry events (D7 reference NATS).

## 8. Conformance

The suite is organized in levels. Each requirement belongs to exactly one level. A binding reports a result for each level it declares.

| Level | Covers | Required for profile |
|---|---|---|
| C (core) | publish, request/reply, serve, errors, principal and headers | `a2a-bus-m1` |
| D (durable) | append, dedupe, consume, ack/nak/term, redelivery, replay | `a2a-bus-m1` |
| K (KV) | put/get/create/update/delete/watch, TTL | `a2a-bus-m1` |
| B (blob) | put/get by digest, corruption, tenant scope | `a2a-bus-m1` |
| O (ordered) | per-subject order under reorder and duplicate faults | `a2a-bus-m1` |
| T (tenant) | isolation and permission denials | `a2a-bus-m1` |
| P (partition) | behaviour under partition and restart | recommended in M1 |

| Requirement | Test idea | Level |
|---|---|---|
| [0011-001] | Compile-time check: no technology types exposed to core crates | C |
| [0011-002] | Trait method set matches section 4.2 names for the profile | C |
| [0011-003] | Subjects and headers round-trip byte-exact through the binding | C |
| [0011-010] | Publish returns ack with stream sequence only after store; kill_before_ack: caller gets `Timeout` or `Unavailable`, never success | D |
| [0011-011] | Request with a server returns one reply; no server returns `NoResponder` promptly | C |
| [0011-012] | Single-use reply is a compile-time property (`ReplyHandle::reply` consumes the handle; compile-fail check); distinct requests get distinct replies at run time | C |
| [0011-013] | Second append with the same msg_id inside window returns `Duplicate`; one stored message; append to an uncaptured subject returns `Absent` | D |
| [0011-014] | Consumer from seq n sees all retained messages from n in order; a second consume with another filter keeps the original filter | D |
| [0011-015] | Unacked delivery redelivered after ack_wait; nak delays; ack and term stop redelivery; in_progress extends deadline to now + ack_wait | D |
| [0011-016] | CAS conflict returns `Conflict`; TTL key absent after interval without delete; re-create after expiry or purge carries the TTL and is fenced by the marker revision | K |
| [0011-017] | Digest mismatch from storage returns `Corrupt`, bytes never returned | B |
| [0011-018] | Capabilities stable across calls | C |
| [0011-019] | close leaves unacked deliveries pending for redelivery | D |
| [0011-020] | Duplicate fault injection: consumer sees at-least-once, never fewer | D |
| [0011-021] | Reorder fault across subjects only; same-subject order preserved | O |
| [0011-022] | Sequential publish-ack on one subject stored in order | O |
| [0011-023] | Dedupe holds for the declared window; after the window duplicates may be stored | D |
| [0011-024] | Start from a sequence beyond retention returns `Unavailable` `RETENTION_EXCEEDED` | D |
| [0011-025] | No test depends on cross-subject order | O |
| [0011-026] | Watch yields snapshot entries then exactly one `SnapshotEnd`; empty bucket yields `SnapshotEnd` alone; same for `bus-mem` | K |
| [0011-030] | Capability table complete and typed | C |
| [0011-031] | `dedupe_window` at least 3630 s for the M1 profile | D |
| [0011-032] | `max_message_size` at least 262144 bytes after framing; 262145 rejected with `PayloadTooLarge` | C |
| [0011-033] | Each declared partition_tolerant claim passes level P tests | P |
| [0011-034] | Profile check passes for complete binding; each required capability removed in turn fails startup | C |
| [0011-035] | Startup with a missing or undersized capability: constructor returns an error naming the capability; process exits non-zero | C |
| [0011-036] | Process that does not need a feature does not probe for it; no capability advertised without implementation | C |
| [0011-037] | Profile names in configuration are validated against the registry | C |
| [0011-040] | Operation without principal rejected; denial observable only as `PermissionDenied` | T |
| [0011-041] | Publish and consume denial per subject and stream; allowed paths succeed | T |
| [0011-042] | Cross-tenant KV watch, KV get, blob get with known digest all denied | T |
| [0011-043] | Injected credential-like values absent from errors, logs and watch events | T |
| [0011-044] | Denied subject and absent subject return indistinguishable errors | T |
| [0011-092] | Each request and delivery carries the principal of its transport peer; a forged header or payload does not change it | T |
| [0011-050] | Each error variant produced by its trigger; variants a caller cannot trigger (`DuplicateInProgress`, `Internal`) reported `not_applicable` with reason; no untyped error type | C |
| [0011-051] | Duplicate on append treated as success by the caller helper | D |
| [0011-052] | Conflict never resolved by overwrite | K |
| [0011-053] | Transport error during append retried with same msg_id; dedupe holds | D |
| [0011-060] | Each fault switch exercised in the suite | P |
| [0011-061] | Same seed yields identical fault trace | P |
| [0011-062] | Under all faults, error taxonomy and stored content unchanged | P |
| [0011-063] | restart_bus keeps all acked appends; unacked deliveries redelivered | P |
| [0011-064] | Partition during append yields `Timeout` or `Unavailable`, never success without store | P |
| [0011-070] | Over-limit publish returns `Overloaded`; no silent drop under load test | C |
| [0011-071] | Consumer pending bound enforced | D |
| [0011-072] | KV value and blob size limits enforced and reported | K |
| [0011-080] | Suite runs against bus-mem without network | C |
| [0011-081] | Report reproduces from its recorded seed | C |
| [0011-082] | Binding with any required fail is not reported conformant | C |
| [0011-083] | Report format accepted by the `a2a-conformance` crate | C |
| [0011-090] | Non-conforming technology absent from profile binding list | C |
| [0011-091] | New binding passes suite before profile registration | C |

## 9. Open issues

- **O-1 Anonymous local mode.** Default: not in the M1 profile; local development uses real principals with local credentials (RFC-0007).
- **O-2 Partition tolerance in M1.** Default: recommended, not required, in `a2a-bus-m1`. The NATS binding (RFC-0010) MUST NOT claim it until level P passes.
- **O-3 Ordering on mixed bindings.** A binding without `ordered_per_subject` cannot carry RFC-0002 events. Default: such a binding is excluded from every profile that carries events.
- **O-4 Report format.** Default: owned by the `a2a-conformance` crate; this RFC fixes only the required fields of [0011-081].
- **O-5 Object store vs blob store.** Default: the contract's blob operations are the only large-content path. RFC-0010 may implement them with a NATS object store; no object store operation is exposed.
- **O-6 Dedupe window values.** Default: `dedupe_window >= 3630 s` for every stream that carries RFC-0002 event subjects ([0011-031]). Other streams, such as audit (15 min in RFC-0010), are governed by their own declared window. The NATS default of 2 minutes in `docs/interop/nats.md` is below this and MUST be overridden by RFC-0010.
- **O-7 Consumer reads on ev not yet authorized.** RFC-0010 provides ordered consumers filtered by task subject for event replay (0010-050), which could implement `durable_streams` and `replay_from_seq`. No identity may yet read ev subjects (0010-060). Default: `bus-nats` MAY declare those capabilities only after an ev read identity is authorized and the [0011-024] retention rule is tested, meaning a start below retention returns `Unavailable` `RETENTION_EXCEEDED` and not a silent later start. Until then [0011-035] refuses startup. The profile is unchanged.

## Appendix A. Informative Rust trait sketch

The sketch shows the seam of [0011-002]. Names are normative; types and signatures are not.

```rust
pub trait Bus: Send + Sync {
    fn capabilities(&self) -> &Capabilities;
    async fn publish(&self, subject: &Subject, headers: &Headers, payload: Bytes) -> Result<PublishAck, BusError>;
    async fn request(&self, subject: &Subject, headers: &Headers, payload: Bytes, timeout: Duration) -> Result<Reply, BusError>;
    async fn serve(&self, pattern: &SubjectPattern) -> Result<Box<dyn RequestStream>, BusError>;
    async fn append(&self, stream: &StreamName, subject: &Subject, headers: &Headers, payload: Bytes, msg_id: MsgId) -> Result<PublishAck, BusError>;
    async fn consume(&self, stream: &StreamName, consumer: &ConsumerName, filter: &Subject, start: StartPosition) -> Result<Box<dyn Consumer>, BusError>;
    async fn kv_put(&self, bucket: &Bucket, key: &Key, value: Bytes, ttl: Option<Duration>) -> Result<Revision, BusError>;
    async fn kv_get(&self, bucket: &Bucket, key: &Key) -> Result<Option<Entry>, BusError>;
    async fn kv_create(&self, bucket: &Bucket, key: &Key, value: Bytes, ttl: Option<Duration>) -> Result<Revision, BusError>;
    async fn kv_update(&self, bucket: &Bucket, key: &Key, value: Bytes, expected: Revision, ttl: Option<Duration>) -> Result<Revision, BusError>;
    async fn blob_put(&self, tenant: &Tenant, bytes: Bytes) -> Result<Digest, BusError>;
    async fn blob_get(&self, tenant: &Tenant, digest: &Digest) -> Result<Option<Bytes>, BusError>;
    async fn kv_delete(&self, bucket: &Bucket, key: &Key, expected: Option<Revision>) -> Result<(), BusError>;
    async fn kv_watch(&self, bucket: &Bucket, prefix: &Key, start: StartPosition) -> Result<Box<dyn Watch>, BusError>;
    async fn close(&self) -> Result<(), BusError>;
}

pub trait Consumer: Send {
    async fn next(&mut self) -> Result<Delivery, BusError>;
    async fn ack(&self) -> Result<(), BusError>;
    async fn nak(&self, delay: Duration) -> Result<(), BusError>;
    async fn term(&self) -> Result<(), BusError>;
}

// Abridged. bus-api::control defines the full trait. The switches of [0011-061],
// including kill_before_ack, are set through set_fault.
pub trait BusControl: Send + Sync {
    fn advance(&self, by: Duration);
    fn set_fault(&self, fault: Fault);
    fn restart_bus(&self);
    fn crash_consumer(&self, stream: &StreamName, consumer: &ConsumerName) -> bool;
}

pub enum BusError {
    NoResponder,
    Timeout,
    Unavailable { reason: String },
    Overloaded { retry_after: Duration },
    PermissionDenied,
    PayloadTooLarge { limit: usize },
    Duplicate { original_seq: u64 },
    DuplicateInProgress,
    Conflict { current_revision: u64 },
    Absent,
    Corrupt { digest: Digest },
    Unsupported { capability: &'static str },
    Internal,
}
```

`Delivery` carries headers, payload, the principal that published it ([0011-092]), stream sequence, attempt count, and `ack`, `nak(delay)`, `term` and `in_progress` methods; `in_progress` extends the ack deadline to now plus `ack_wait` ([0011-015]). `Request` carries headers, payload, the principal ([0011-092]) and a single-use reply handle ([0011-012]). The types `Subject`, `Headers`, `MsgId`, `Digest` and `Revision` are defined by `bus-api` and are not repeated here.
