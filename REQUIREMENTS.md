# Bus-Based Agent-to-Agent Protocol: Requirements & Threat Model

Status: draft v0. Scope: requirements only. No wire format, no code.
Origin: gRPC/HTTP2 A2A handoff, re-targeted at a message bus.

## 1. Decisions taken

| ID | Decision | Consequence |
|----|----------|-------------|
| D1 | Bus-agnostic | Protocol specifies semantics and a minimal transport-binding contract; NATS/MQTT/Kafka/etc. are bindings. Guarantees are the floor that every binding MUST meet or emulate. |
| D2 | Edge-to-cloud federation | Multiple brokers, intermittent links, store-and-forward. Peers cannot assume a shared broker or low latency. |
| D3 | At-least-once delivery, idempotent handlers | Every message carries a unique ID; receivers dedupe; effects keyed on IDs. |
| D4 | Deliverable includes a threat model | See section 6. |

Carried over from the handoff: ACP-style lifecycle, human permission gates, MCP handoff, protobuf as the schema language (assumption, confirm in Q1), lightweight footprint.

## 2. Goals and non-goals

Goals
- G1 Agents discover each other and their capabilities over the bus.
- G2 Submit a task, observe progress, receive a terminal result; survive disconnects.
- G3 Explicit lifecycle with human-approval suspension.
- G4 Safe composition: injection isolation, loop bounds, scoped credential delegation.
- G5 Small footprint: implementable on an edge device with a minimal bus client and a protobuf runtime.

Non-goals (proposed)
- N1 Not a broker. Does not define routing, clustering or persistence of the bus.
- N2 Not an LLM runtime API or an MCP replacement; MCP is bridged, not re-specified.
- N3 No synchronous-RPC emulation as the primary model (request/reply is a convenience layered on async).
- N4 No REST/JSON gateway in core (may be a separate profile).

## 3. Actors

- Requester: agent or orchestrator that submits tasks.
- Worker: agent that executes tasks.
- Operator: human who approves gated actions via a control UI. Also an agent-like peer on the bus.
- Bus: opaque, untrusted-for-confidentiality transport (see T-assumptions).
- Registry (optional): discovery aid; the protocol must work without one.

## 4. Functional requirements

Discovery
- F1 An agent publishes a signed manifest: id, version, role, capabilities, accepted input types, supported MCP bridges, limits (max depth, max concurrent tasks), protocol versions.
- F2 Manifests are retained or re-announced so late joiners and edge nodes returning from offline can discover peers. Includes TTL and liveness (presence).
- F3 Capability matching by name plus version range. Free-text role is informational only.

Task lifecycle
- F4 States: QUEUED, PROCESSING, PENDING_APPROVAL, SUCCEEDED, FAILED. Proposed additions to evaluate: CANCELED, REJECTED (policy/validation), EXPIRED (deadline), INPUT_REQUIRED (non-permission clarification). The handoff conflates these into FAILED, which loses information.
- F5 Legal transition table is normative. SUCCEEDED, FAILED, CANCELED, REJECTED, EXPIRED are terminal and immutable.
- F6 State changes are events with a monotonic per-task sequence number. Duplicate or reordered events are tolerated; the highest sequence wins.
- F7 Requester can cancel; worker acknowledges. Cancel is best-effort, with final state reported.
- F8 Every task has a deadline or TTL. Abandoned tasks must reach a terminal state.

Streaming and results
- F9 Worker emits ordered, resumable progress/delta chunks (per-task sequence, requester can request replay from sequence N).
- F10 Large payloads (artifacts) go by reference (content-addressed pointer plus hash) when exceeding a bus message limit; inline otherwise. Binding declares max message size.
- F11 Result carries structured error (code taxonomy, retryable flag), not just int and string.

Human-in-the-loop
- F12 Worker emits a permission request: capability, scope, human-readable reason, expiry. Task enters PENDING_APPROVAL.
- F13 Grants are signed by an operator identity, bound to task id + request id + scope, single use, expiring. Default on timeout is deny.
- F14 Approval routing is decoupled: the request can go to the requester's operator, not necessarily the worker's.

Delegation and MCP
- F15 A requester can delegate a narrowly scoped capability to a worker for one task (see T5). Delegation never transmits raw long-lived secrets in the clear.
- F16 Delegated MCP access is by reference to a mediated endpoint or a short-lived attenuated token, with task-bound scope and expiry.

Multi-hop
- F17 A worker can sub-delegate. Each message carries a correlation chain: root task id, parent id, depth, hop budget.

Loop and budget control
- F18 Every task carries budgets: max depth, max hops, max tokens, max wall time, max cost units. Budgets are monotonically non-increasing when delegating and enforced by each hop.
- F19 Cycle detection via chain of agent ids plus task fingerprint; violation yields FAILED with a distinct error code.

## 5. Non-functional requirements

Delivery and consistency (from D3)
- R1 Every message has a globally unique id (and a causation id). Handlers are idempotent on it.
- R2 Receivers keep a bounded dedupe window; window size vs. max redelivery horizon is a stated invariant.
- R3 Ordering guaranteed only per task (sequence number), never globally. Binding must preserve per-key order or the protocol reorders using seq.
- R4 Redelivery of a task submission to a busy or restarted worker must not execute it twice (dedupe by task id; recorded state survives restart).

Federation and edge
- R5 Operates across partitions: sender queues locally, delivers on reconnect. Message TTL honors the task deadline.
- R6 Addressing is logical (agent id), not broker-specific; routing across brokers is the binding's job, but addresses must survive a leaf-to-hub hop.
- R7 Clock skew tolerant: no hard dependence on synchronized wall clocks. Expiry uses relative durations plus an issuer-stamped time with a skew allowance.

Footprint
- R8 Core library fits constrained targets: no mandatory registry, no mandatory persistent store beyond a small state file/KV, bounded memory per task.
- R9 Binding contract is small: publish, subscribe, optional durable consumer, max message size, per-key ordering flag. Reference bindings: in-memory (tests) plus one real bus.

Evolvability
- R10 Schema versioning with forward/backward compatibility rules; unknown fields are ignored, unknown critical extensions are rejected (explicit "critical" marker).
- R11 Protocol version negotiated via manifest.

Observability
- R12 Trace context (W3C traceparent) in every envelope. Task event stream is auditable.

Security (summary; detail in section 6)
- R13 End-to-end message authenticity and integrity independent of the broker (signed envelopes). Confidentiality end-to-end for sensitive payloads (encrypted to recipient).
- R14 Authorization at three layers: broker ACL (coarse, defense in depth), protocol-level (who may request what), per-task capability.

## 6. Threat model

Trust assumptions
- A1 The broker is not trusted for confidentiality or integrity; it is assumed available best-effort. Broker ACLs are defense in depth only.
- A2 Peer agents are semi-trusted: authenticated, but a compromised or prompt-injected peer is in scope.
- A3 Task content (instructions, tool results, artifacts) is untrusted data regardless of sender.
- A4 Operators are trusted; their control channel is a high-value target.

| # | Threat | Vector | Required mitigation |
|---|--------|--------|---------------------|
| T1 | Prompt injection via task payload or results | Instruction text from peer or tool output steers the worker LLM | Structural separation: envelope distinguishes instruction (from authenticated requester, still policy-bounded) from data (untrusted). Runtime framework wraps untrusted fields and never lets data fields grant authority. Delimiter wrapping is hygiene only, not a security boundary. Real boundary is capability limits: injected text can only invoke what the task's granted scope allows, and sensitive actions hit F12 gates. |
| T2 | Confused deputy / privilege escalation through delegation | Worker uses requester's authority for something else | Capabilities attenuated per task, bound to task id and audience; no ambient authority. |
| T3 | Infinite ping-pong and cost exhaustion | Cyclic delegation or runaway multi-turn | F18/F19: hop, depth, token, time budgets carried in envelope and enforced locally by every hop. Budgets cannot be raised by downstream. A tripped breaker emits FAILED with a distinct code. Honest note: token counts reported by peers are self-reported; each hop meters its own use. |
| T4 | Message replay, duplication (also inherent in at-least-once) | Broker or attacker re-injects old messages, esp. permission grants | Unique ids, nonce, expiry, dedupe; grants single use and bound to request id. |
| T5 | Credential/MCP token leakage | Secrets in metadata, logs, or broker storage | No raw secrets in envelopes. Use reference + short-lived attenuated token encrypted to the recipient, or a mediated proxy. Redaction in logs and traces required. |
| T6 | Impersonation / forged peer | Attacker publishes to another agent's subject or topic | Per-message signatures, key identity tied to agent id; manifest signed; key rotation and revocation story required (open Q6). |
| T7 | Forged or stolen human approval | Spoofed grant, UI phishing, approval fatigue | Grants signed by operator key; display shows the actual action, scope and requesting chain; rate-limit and batch-guard prompts; default deny. |
| T8 | Poisoned discovery | Fake manifest advertises a capability to attract tasks and data | Signed manifests, allowlists or trust anchors per deployment; requester pins expected agent identity before sending sensitive data. |
| T9 | Broker-level DoS and queue flooding | Spam of task submissions | Per-peer rate limits and queue quotas at worker; admission control returns REJECTED rather than silent drop. |
| T10 | Metadata leakage | Subjects, ids, sizes visible to broker | Document leakage; minimize sensitive data in routing keys. Encrypted payload, plaintext routing header minimum. |
| T11 | Partition-time stale authority | Offline edge node acts on expired grant or revoked key | Hard expiries, short-lived credentials, fail closed when freshness cannot be shown. |

## 7. Open questions (need your decision)

1. Q1 Schema language: keep protobuf (compact, good for edge) or something else (CBOR/JSON for bus-ecosystem friendliness)?
2. Q2 Identity and signing: key scheme (Ed25519 per agent?), who is the trust root, and how are keys distributed and revoked across federated brokers?
3. Q3 Is the supported topology request/reply per task (reply subject), or pub/sub event stream per task id with replay? Affects binding requirements (durable consumers).
4. Q4 Task ownership: can a task be handed off or resumed by a different worker instance (competing consumers / work queue), or is it pinned to one agent id?
5. Q5 How far is end-to-end encryption in scope for v1 versus signed-only?
6. Q6 Is a registry a first-class component or purely emergent from presence announcements?
7. Q7 Token metering: trusted self-report only, or is there a verifiable metering mechanism?
8. Q8 Is MCP delegation mediated (proxy) only, or can short-lived tokens be passed directly?
9. Q9 Which real bus is the first reference binding, and which are explicit non-targets?
10. Q10 Extra lifecycle states (CANCELED, REJECTED, EXPIRED, INPUT_REQUIRED): accept?

## 8. Suggested next steps

1. Resolve Q1, Q2, Q3 (they shape everything).
2. Write the normative state machine and envelope field list (no encoding yet).
3. Define the binding contract and an in-memory reference binding for conformance tests.
4. Pick a reference bus and write failure-scenario tests: duplicate delivery, partition and reconnect, restart mid-task, cycle trip.
