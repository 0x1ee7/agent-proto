# Spike: a2a-binding-conformance

Read-only audit of RFC-0002 (bus binding), RFC-0003 (extensions and card) and RFC-0008 (HTTP gateway) against A2A v1.0 (latest spec as fetched 2026-10 from https://a2a-protocol.org/latest/specification/, sections 3, 4, 5, 7, 11, 12, 13) and `docs/interop/a2a-spec-notes.md`. No system was run, no RFC was edited, no binary was executed. Spec text was read from the fetched page; anything not read is marked `[UNOBSERVED]`. Inferences are `[INFERENCE]`.

Verdict legend: PASS (requirement met by a cited RFC requirement), PARTIAL (met in part or only by implication), GAP (not addressed), DEVIATION (intentional, conflicts with spec; justified below).

## 1. Obligation table

### 1a. Section 12 (custom binding guidelines) and section 5 (binding equivalence)

| # | Spec obligation | Ref | RFC requirement | Verdict |
|---|---|---|---|---|
| 1 | 12.1 implement all core ops (§3) | §12.1 | 0002-020, 0002-022, 0008-037 (table covers all 11 ops) | PASS |
| 2 | 12.1 preserve data model (§4) | §12.1 | 0002-021, 0003-017, 0008-041 | PASS |
| 3 | 12.1 document binding completely | §12.1 | RFC-0002 whole; RFC-0002 §8 conformance | PASS |
| 4 | 12.2 map proto types, timestamps, binary data, enums | §12.2 | 0002-035, 0002-038 (binary, enums as numbers) | PARTIAL: timestamps not stated [INFERENCE: proto Timestamp type carries them] |
| 5 | 12.3 document service-parameter transmission, value constraints, reserved names, fallback | §12.3 | 0002-082 (transmission, `A2AB-SP-<k>`, `[a-z0-9-]{1,48}`, 1024-byte value limit, no body fallback) | PASS (see ambiguity A-5) |
| 6 | 12.4 map all A2A errors, preserve error info, native codes, document format | §12.4 | 0002-090, 0002-091 (all 9 A2A errors), 0008-045 | PASS |
| 7 | 12.5 streaming mechanism, ordering, reconnection, termination documented | §12.5 | 0002-041..046, 0002-053, 0002-060..063, 0008-054..057 | PASS (termination deviates, see D-4) |
| 8 | 12.6 support declared schemes; document credential transmission; challenges; secure transport | §12.6 | 0002-013 (no credentials on bus), 0008-021 (Bearer, WWW-Authenticate), 0008-068/069 (TLS) | PARTIAL: bus scheme not defined (0003-028 MAY) — GAP-A |
| 9 | 12.7 declare transport URI and endpoint URL in card | §12.7, §5.8 | 0002-100, 0002-101, 0003-025 | PASS |
| 10 | 12.8 test against reference; document differences | §12.8 | 0002-§8 table; this document | PARTIAL: no reference-SDK test planned in RFCs [UNOBSERVED] |
| 11 | 5.1 functional equivalence: same ops, behavior, errors, auth | §5.1 | Sections 1b–1h below | PARTIAL, see deviations D-1..D-5 |
| 12 | 5.2 agent declares all supported protocols; clients may choose; clients SHOULD fall back | §5.2 | 0002-100/102, 0003-025 | PASS |
| 13 | 5.4 map all A2A errors; custom binding MUST define mapping | §5.4 | 0002-091 | PASS |
| 14 | 5.5 camelCase JSON for JSON serializations | §5.5 | 0008-042 (gateway JSON); bus is protobuf (D-1) | PASS for JSON / DEVIATION for bus |
| 15 | 5.6.1 timestamps ISO 8601 UTC in JSON | §5.6.1 | not stated in RFCs; examples use `Z` (0002 §4.12) | PARTIAL [UNOBSERVED: §5.6.1 text not re-read] |
| 16 | 5.8 custom binding uses URI; breaking change needs new URI | §5.8 | 0002-100 (`urn:a2abus:binding:bus:1`) ; versioning in name `:1` [INFERENCE] | PARTIAL: no rule that bus binding URI changes on breaking change — GAP-B |

### 1b. Operations (§3.1)

| Op | Spec | RFC | Verdict |
|---|---|---|---|
| SendMessage (§3.1.1) | returns Task/Message; errors ContentTypeNotSupported, UnsupportedOperation, TaskNotFound | 0002-050, 0002-064, 0002-091, 0008-037 | PASS for error set; blocking default is D-3 |
| SendStreamingMessage (§3.1.2) | first Message-only stream closes after one Message; Task-first stream closes at terminal | 0002-052, 0002-054; 0008-054 | DEVIATION D-4 (closes also at interrupted) |
| GetTask (§3.1.3) | historyLength semantics (§3.2.4); TaskNotFound | 0002-020, 0008-037 (passes historyLength), 0002-091 (TASK_NOT_FOUND) | GAP-C (historyLength semantics not specified in RFC-0002/0008) |
| ListTasks (§3.1.4) | visibility per auth; cursor paging; sort by update desc; pageSize 1..100 default 50; nextPageToken always present (""=last page); includeArtifacts false omits artifacts; statusTimestampAfter; totalSize | 0008-038 (pageSize range), 0008-040 (visibility), 0008-037 (list op) | GAP-D (nextPageToken presence, totalSize, includeArtifacts, sort order, statusTimestampAfter unspecified) |
| CancelTask (§3.1.5) | TaskNotCancelable on terminal; duplicate cancel may be TaskNotFound | 0002-091 (TASK_NOT_CANCELABLE), 0002-091 table; 0008-040 | PASS |
| SubscribeToTask (§3.1.6) | Task first event; terminal → UnsupportedOperation; stream ends at terminal | 0002-060, 0002-062; 0008-057 | PASS for first event and -32004; end-at-terminal DEVIATION D-4 |
| Push CRUD (§3.1.7–3.1.10) | Create/Get/List/Delete; errors PushNotSupported, TaskNotFound | 0002-024, 0008-037, 0008-058..064 | PARTIAL: gateway rejects `authentication`/`token` (0008-059); see D-2 |
| GetExtendedAgentCard (§3.1.11) | only when capability true | 0003-027 (`extendedAgentCard: false`), 0008-019 | GAP-E (spec §3.3.4: capability false → UnsupportedOperationError; 0008-019 says -32007 for "no extended card") |

### 1c. Shared structures (§3.2–3.4)

| Obligation | Ref | RFC | Verdict |
|---|---|---|---|
| returnImmediately default false; blocking MUST wait for terminal or interrupted (INPUT_REQUIRED, AUTH_REQUIRED); reply carries latest state | §3.2.2 | 0002-050 (wait for terminal or INPUT_REQUIRED only), 0008-050..053 (100 s bounded wait) | DEVIATION D-3, GAP-F (AUTH_REQUIRED omitted from interrupted set) |
| historyLength: unset = server default; 0 = omit history; >0 ≤ N | §3.2.4 | none | GAP-C |
| metadata passthrough | §3.2.5 | 0003-012, 0008-042 | PASS |
| service params keys case-insensitive, values case-sensitive; standard `A2A-Version`, `A2A-Extensions` | §3.2.6 | 0002-082, 0002-030 | PASS (ambiguity A-5) |
| idempotency: Get idempotent; SendMessage MAY dedupe by messageId; duplicate cancel may return TaskNotFound | §3.3.1 | 0002-031, 0008-082 (worker dedupes on messageId) | PARTIAL: dedupe window not specified [UNOBSERVED: RFC-0005 not read] |
| errors: code, message, details with @type; ErrorInfo; authz/not-found not disclosing existence; validation SHOULD list failed params | §3.3.2 | 0002-090, 0002-095, 0008-040, 0008-004, 0008-045 | PARTIAL: no rule that validation errors name failing field (SHOULD) |
| async: return immediately with Task or Message | §3.3.3 | 0002-050 (returnImmediately) | PASS |
| capability validation: push w/o pushNotifications → PushNotSupported; stream ops w/o streaming → UnsupportedOperation; GetExtendedAgentCard w/o extendedAgentCard → UnsupportedOperation; declared-not-configured → ExtendedAgentCardNotConfigured; required ext not requested → ExtensionSupportRequired | §3.3.4 | 0002-024, 0002-062, 0002-022, 0003-016, 0008-028 | GAP-E (GetExtendedAgentCard with capability false) |
| contextId: agent MAY generate; if generated MUST return it; if cannot accept client's contextId MUST reject and MUST NOT generate new one | §3.4.1 | 0008-035 (passes none, worker assigns) | PARTIAL: rejection rule for unacceptable client contextId missing — GAP-G |
| taskId: server-generated, unique, in every Task; client-supplied taskId MUST reference existing task else TaskNotFound | §3.4.2 | 0002-004 (worker assigns, MUST NOT accept client task ids for new tasks), 0002-091 | PARTIAL: error code for unknown client taskId not stated — GAP-H |
| events in generation order; multiple streams each get same events; closing one does not affect others | §3.5.2 | 0002-042, 0002-043, 0008-056, 0008-057 (closed HTTP stream does not cancel) | PASS (multi-subscriber fan-out implied by per-subscription replay; [INFERENCE]) |

### 1d. Versioning and extensions (§3.6, §4.6)

| Obligation | Ref | RFC | Verdict |
|---|---|---|---|
| clients send A2A-Version each request; empty header = 0.3 | §3.6.1 | 0002-080 (missing/empty → VersionNotSupported), 0008-027 | DEVIATION D-5 |
| agents process per requested major.minor; unsupported → VersionNotSupportedError | §3.6.2 | 0002-080, 0008-027 (-32009) | PASS |
| SDK negotiation; no silent auto-fallback | §3.6.3 | 0003-022 (no fallback major), 0002-080 | PASS [UNOBSERVED: SDK behavior not in scope] |
| extension URIs carry version; breaking → new URI; unknown non-required ignored; required unsupported → error; no auto-fallback | §4.6.3 | 0003-001, 0003-020, 0003-015/016, 0003-022, 0002-083 | PASS |
| extension data placed under URI key in metadata | §4.6.2 | 0003-003, 0003-012 | PASS |
| AgentCapabilities: streaming, pushNotifications, extensions, extendedAgentCard | §4.4.3 | 0003-027, 0008-013 | PASS |
| AgentInterface: url absolute HTTPS for HTTP transports; protocolBinding; protocolVersion | §4.4.6 | 0003-025, 0008-014 (`public_base_url`) | PASS |

### 1e. Streaming and push (§4.3.3, §11.7, §13.2)

| Obligation | Ref | RFC | Verdict |
|---|---|---|---|
| SSE: each `data:` a StreamResponse; linear order; first Task or Message; closes at terminal OR interrupted; MAY resend final Task | §11.7 | 0008-054, 0002-052 | PASS (interrupted-close matches §11.7, conflicts §3.1.2 — D-4) |
| SSE keepalive / reconnect | §11.7 (implied) | 0008-055, 0008-057 | PASS |
| Push payload: POST webhook, StreamResponse body | §4.3.3 | 0008-061 | PASS |
| Push Content-Type application/a2a+json | §11.1 | 0008-061 | PASS |
| Push Authorization per `authentication` (MUST include credentials) | §4.3.3 | 0008-059 rejects authentication; HMAC instead (0008-062) | DEVIATION D-2 |
| Push at-least-once, retry with backoff, timeout 10–30 s, stop after N failures | §4.3.3 | 0008-061 (10 s, 5 attempts, 1/2/4/8 s backoff) | PASS |
| Push SSRF reject private/loopback/link-local | §13.2 | 0008-060 (host allowlist; re-check at delivery; no redirects) | PARTIAL: loopback exception is allowed for `http` when `push_allow` says so; acceptable but record it |
| Push: 2xx to ack; receiver dedupe; task id check | §4.3.3 | 0008-061 (dedupe by (taskId, seq)) | PASS (receiver side is the client's) |

### 1f. Errors on HTTP and auth (§7.3–7.6, §11.6)

| Obligation | Ref | RFC | Verdict |
|---|---|---|---|
| ErrorInfo MUST for A2A errors; domain `a2a-protocol.org`; reason UPPER_SNAKE; metadata | §11.6 | 0002-090, 0008-045 | PASS |
| Auth reject invalid/missing creds; SHOULD give challenge with scheme | §3.3.2, §7.4 | 0008-021 (401, `WWW-Authenticate: Bearer`) | PASS |
| Server authorizes per identity; authz errors MUST NOT reveal existence | §7.5, §3.3.2 | 0008-004, 0008-040 | PASS |
| Credentials in headers per request | §7.3 [UNOBSERVED: §7.3 text not re-read] | 0008-021, 0002-013 | PASS |
| AUTH_REQUIRED: agent MUST transition and include status message if in-task auth is needed | §7.6.1 | 0008-053 (no AUTH_REQUIRED in M1), 0008-079 (peer AUTH_REQUIRED → FAILED `BACKEND_ERROR`) | DEVIATION D-6 (see D-6) |
| Gateway challenge for bus identity | §7.4 | 0002-011, 0002-013 | PASS [INFERENCE] |

### 1g. Section 13 security

| Obligation | Ref | RFC | Verdict |
|---|---|---|---|
| §13.1 scope all access by authenticated caller | §13.1 | 0002-012, 0008-040 | PASS |
| §13.2 push: credentials, timeout, retry, SSRF, HTTPS SHOULD, tokens unique per config | §13.2 | 0008-060..063 | PARTIAL: no per-config unique token (no tokens at all, HMAC per peer 0008-062) |

### 1h. Bus-specific mapping (RFC-0002 §4.1–4.10, not A2A obligations)

Covered by the RFC's own requirement IDs; each is an M1 choice. They are not checked against A2A except where listed above. See `docs/rfcs/0002-bus-protocol-binding.md` §8 for the conformance table.

## 2. GAPs and partials: proposed RFC text

Edits are proposals only; RFCs are not edited.

**GAP-A (RFC-0002, §4.7, after [0002-082]; RFC-0003 §4.5 [0003-028]).** New requirement [0002-085]: "The bus binding defines no authentication scheme. A bus card MUST NOT declare a `securitySchemes` entry for the bus interface until RFC-0007 defines one. Credentials are carried only as bus identity (RFC-0007), never in headers, subjects or payloads (0002-013)." Conformance row: `[0002-085]` | Bus card with a securitySchemes entry rejected by card check | MUST.

**GAP-B (RFC-0002, §4.1, new [0002-086]).** "The bus binding URI `urn:a2abus:binding:bus:1` changes to `urn:a2abus:binding:bus:2` for any change that breaks an existing bus client (A2A §5.8). Both URIs MAY be declared during migration." Row: card with `:2` and `:1` both declared; old client selects `:1`. MUST.

**GAP-C (RFC-0002, §4.3 operation table; RFC-0008 §4.4).** New [0002-087] for `get` and `subscribe`: "`historyLength` unset uses the Worker default; `0` MUST omit history from the returned Task; a value N > 0 MUST return at most N history items and MAY return fewer (A2A §3.2.4)." Gateway [0008-094]: "The gateway MUST pass `historyLength` unchanged to the worker and MUST NOT truncate history itself." Row: get with historyLength 0 returns Task with no history; with 2 returns ≤2. MUST.

**GAP-D (RFC-0002, §4.3 or new §4.5.4; RFC-0008 §4.4).** New [0002-088] for `list`: "The Worker MUST return `nextPageToken` on every ListTasks response, empty on the last page. `totalSize` MUST be present. Results are sorted by last update time descending. `includeArtifacts` false MUST omit artifacts. `statusTimestampAfter` filters strictly after the given time (A2A §3.1.4)." Gateway [0008-095] keeps pageSize rule and forwards these fields unchanged. Row: list with 3 tasks pageSize 2 gives token then empty token. MUST.

**GAP-E (RFC-0008 §4.2 [0008-019]; RFC-0002 [0002-022]).** Replace 0008-019 last sentence: "If the agent's derived card has `extendedAgentCard: false`, the gateway MUST return `-32004` (UnsupportedOperationError). If `extendedAgentCard: true` but no extended card is configured, it MUST return `-32007` (EXTENDED_AGENT_CARD_NOT_CONFIGURED)." Conformance row: `[0008-019]` with capability false → -32004. MUST.

**GAP-F (RFC-0002 [0002-050] and [0002-044]; RFC-0008 [0008-053]).** Add AUTH_REQUIRED to the interrupted set for blocking waits, but state it is never emitted in M1: "Blocking `send` returns at terminal or at INPUT_REQUIRED or AUTH_REQUIRED (A2A §3.2.2). M1 emits no AUTH_REQUIRED (see D-6). The stream-end rule of [0002-044] applies to INPUT_REQUIRED and AUTH_REQUIRED." Row: a Worker simulating AUTH_REQUIRED returns to blocking send. MUST.

**GAP-G (RFC-0002 §4.1 [0002-004]; RFC-0008 [0008-035]).** New [0002-089]: "If a `send` carries a `contextId` the Worker does not accept, the Worker MUST reject with `INVALID_PARAMS` and MUST NOT generate a different contextId (A2A §3.4.1)." Gateway [0008-035] adds: "The gateway MUST forward the client's contextId unchanged." Row: send with unacceptable contextId → rejected. MUST.

**GAP-H (RFC-0002 [0002-004]).** Extend: "A client-supplied `taskId` on `send` or `subscribe` that does not exist (or is not owned by the origin) MUST yield `TASK_NOT_FOUND` (A2A §3.4.2). A client-supplied taskId for a new task is rejected with `TASK_NOT_FOUND` as well." Row: send with unknown taskId → TASK_NOT_FOUND. MUST.

**Partial (RFC-0002 §4.8 [0002-090]):** Validation errors SHOULD name the failing field: "INVALID_PARAMS ErrorInfo.metadata SHOULD contain `field` naming the invalid parameter (A2A §3.3.2)." SHOULD.

**Partial (RFC-0008 [0008-021]):** Add scheme to challenge: "WWW-Authenticate MUST be `Bearer` (scheme name only; no token echo)." Already compatible; SHOULD row only.

**Partial (RFC-0008 [0008-060]):** Record the loopback `http` exception as an operator-visible default in card or ops doc. No RFC behavior change.

## 3. Deliberate deviations

Verdict: "Can a conformant A2A client reaching us only via the HTTP gateway break?" (bus-only deviations are invisible to HTTP clients and are listed for completeness.)

**D-1 Protobuf binary payloads on the bus (0002-035, 0002-038, 0002-037).** Bus-only. Gateway emits proto-JSON camelCase (0008-042). Break via gateway: **no**. Justification: bus efficiency, schema pinning. The spec's §5.5 JSON MUST applies to JSON serializations; the gateway satisfies it. [INFERENCE]

**D-2 No push on the bus; gateway push with HMAC and no `authentication` (0002-024, 0008-059, 0008-062).** Via gateway: a client that sends `authentication` or `token` in push-create gets `-32602 PUSH_CREDENTIALS_NOT_SUPPORTED`. Break: **possible**. A conformant client that always sends a bearer token for webhooks breaks on create. Spec §4.3.3 says MUST include credentials "as specified in authentication"; the gateway cannot honor it. Mitigation: document in card; gateway should advertise `pushNotifications: false` when credentials are required. Verdict: PARTIAL risk, documented.

**D-3 SendMessage blocking default with bounded wait (0002-050, 0008-050..052).** Spec §3.1.1 says "MUST return immediately", §3.2.2 says blocking by default and MUST wait until terminal or interrupted. The gateway returns a non-terminal Task at 100 s (0008-052), violating "MUST wait". Via gateway: a client that expects a terminal Task on a non-streaming send gets WORKING/SUBMITTED after 100 s. Break: **possible** for clients with no polling. Conformant A2A clients poll with GetTask (§3.2.2 says non-terminal state is returned for non-blocking). Verdict: PARTIAL risk; the gateway timeout is a documented deviation. Recommend a RFC-0008 note that the blocking wait is bounded by server policy.

**D-4 Stream and subscription end at interrupted (0002-044, 0002-062, 0008-054, 0008-057).** Spec §3.1.2 says stream closes only at terminal; §11.7 and §3.1.6 end at terminal (and §11.7 at interrupted). Via gateway: the SSE stream closes at INPUT_REQUIRED. Break: **possible**. A §3.1.2-strict client waiting for terminal sees a closed stream and must reconnect with SubscribeToTask. SubscribeToTask on INPUT_REQUIRED is allowed (0002-062), so recovery works. Verdict: PARTIAL risk; the closed stream is valid under §11.7.

**D-5 Missing or empty A2A-Version → VersionNotSupported (0002-080, 0008-027).** Spec §3.6.1 says empty header means 0.3. Via gateway: an old 0.3-only client fails with -32009. Break: **yes for 0.3-only clients**; conformant 1.0 clients always send `1.0` (§3.6.1). Verdict: DEVIATION accepted; spec §3.6.2 requires VersionNotSupported for unsupported versions, which covers this.

**D-6 No AUTH_REQUIRED in M1; peer AUTH_REQUIRED becomes FAILED (0008-053, 0008-079).** Spec §7.6.1: if an agent needs in-task auth it MUST use AUTH_REQUIRED. M1 agents do not request in-task auth, so the rule is not triggered. The bridge converts external AUTH_REQUIRED to FAILED with BACKEND_ERROR. Via gateway: a client that expects an auth challenge gets FAILED. Break: **no for M1** (no client flow is triggered); **yes** if an external agent behind the bridge needs auth (the flow is lost). Verdict: DEVIATION, documented here; GAP-F text records it.

## 4. Spec ambiguities and chosen interpretation

- **A-1 §3.1.1 "MUST return immediately" vs §3.2.2 blocking default.** Chosen: §3.2.2 (blocking default, `returnImmediately` opt-out), matching RFC-0002 §7. Caveat D-3.
- **A-2 §3.1.2 stream closes at terminal vs §11.7 terminal-or-interrupted.** Chosen: §11.7 (INPUT_REQUIRED closes). Caveat D-4.
- **A-3 Subscribe verb: §11.3.2 POST `/tasks/{id}:subscribe` vs proto GET.** Chosen: operation token is verb-agnostic (RFC-0002 §7 (c)); gateway verb not stated in RFC-0008. [UNOBSERVED: which verb the gateway serves; RFC-0008 table gives no verb.] Recommendation: state the HTTP verb in RFC-0008 §4.4.
- **A-4 §12.4 cites §3.2.2 for error handling.** Chosen: §3.3.2 (RFC-0002 §7 (d)).
- **A-5 Service parameter prefix: §3.2.6 says "prefixed with a2a-" yet names are `A2A-Version`/`A2A-Extensions`.** Chosen: the two standard names are used as written; other parameters use `A2AB-SP-<k>` (RFC-0002 0002-082) as the custom prefix. Recommendation: confirm with the spec maintainers.
- **A-6 §3.2.6 example version `0.3`.** Chosen: `1.0` only (RFC-0002 D-4).
- **A-7 §3.6.1 empty A2A-Version = 0.3.** Chosen: reject (D-5).
- **A-8 `historyLength` 0 "SHOULD be omitted" (§3.2.4) vs proto optional.** Chosen: 0 means omit (GAP-C).
- **A-9 AgentInterface `protocolBinding` values.** Spec core strings: `JSONRPC`, `GRPC`, `HTTP+JSON`. Chosen: custom binding as URI (§5.8, RFC-0002 0002-100).
- **A-10 Extended card when capability false.** Chosen: UnsupportedOperation (GAP-E), per §3.3.4.

## 5. Blockers

None. Not read: §3.5.1 push op text (lines ~362–478 of the artifact), §5.6.1, §7.3, §9 JSON-RPC error section, §10.6 gRPC errors, §13.1 text detail; RFC-0005 and RFC-0010 not read (cross-referenced by 0002-046, 0002-031). These affect only the marked [UNOBSERVED] rows.

## 6. RFC changes required (summary)

| RFC | Req | Change | Priority |
|---|---|---|---|
| 0002 | new 0002-085 | bus card has no securitySchemes until RFC-0007 defines one | GAP-A |
| 0002 | new 0002-086 | bus binding URI version bump on breaking change | GAP-B |
| 0002 | new 0002-087 + 0008-094 | historyLength semantics | GAP-C |
| 0002 | new 0002-088 + 0008-095 | ListTasks paging, totalSize, sort, includeArtifacts | GAP-D |
| 0008 | 0008-019 | -32004 when extendedAgentCard false; -32007 only when declared not configured | GAP-E |
| 0002 | 0002-050, 0002-044; 0008-053 | include AUTH_REQUIRED in blocking interrupted set; not emitted in M1 | GAP-F |
| 0002 | new 0002-089 + 0008-035 | reject unacceptable contextId; never invent a different one | GAP-G |
| 0002 | 0002-004 | unknown client taskId → TASK_NOT_FOUND | GAP-H |
| 0002 | 0002-090 | SHOULD name failing field in INVALID_PARAMS metadata | partial |
| 0008 | 0008-037 table | state HTTP verb for SubscribeToTask (POST per §11.3.2 or GET) | A-3 |
| 0008 | 0008-059 | advertise pushNotifications false when credentials are required | D-2 |
| 0008 | 0008-052 | document bounded blocking wait as deliberate deviation from §3.2.2 | D-3 |

No conformant A2A client reaching us only through the gateway is broken by the bus-only deviations (D-1). Gateway-visible risks: D-2 (push credentials), D-3 (bounded blocking), D-4 (interrupted stream close), D-5 (0.3 clients). D-6 is safe in M1.
