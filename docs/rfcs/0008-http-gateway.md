# RFC-0008: HTTP Gateway

| Field | Value |
|---|---|
| Status | Draft |
| Depends on | RFC-0001 (architecture), RFC-0002 (binding: subjects, headers, op tokens, bus errors), RFC-0003 (extensions and card), RFC-0004 (discovery and presence), RFC-0005 (lifecycle, delivery, cancel, deadlines), RFC-0006 (approval, budget, loop), RFC-0007 (identity, signing, trust, provenance), RFC-0009 (backends), RFC-0010 (NATS layout), RFC-0013 (audit and redaction) |
| Owner area | gateway |
| Related interop notes | `docs/interop/hermes.md`, `docs/interop/a2a-spec-notes.md`, `docs/interop/nats.md` |

## 1. Summary

The HTTP gateway exposes selected bus agents to stock A2A v1.0 JSON-RPC clients over HTTP and lets bus agents reach external A2A HTTP agents such as Hermes. It listens on loopback by default, authenticates every request to a peer identity before any bus publish, and translates operations one-to-one between the JSON-RPC binding and the bus binding of RFC-0002. The gateway keeps no authoritative state; its caches are rebuilt from the bus or from the worker that owns the task. A normative Hermes profile fixes the wire behavior that the Hermes plugin exercises, so Hermes works unchanged against the gateway.

## 2. Motivation and scope

Hermes, stock A2A SDK clients, and browsers-free tooling speak A2A over HTTP. The bus agents (omp, claude, tui) speak only the bus binding. The gateway is the single place where HTTP trust decisions are made (threat T12 in `docs/requirements.md`).

Goals:
- G1: each exposed bus agent is reachable at a per-agent JSON-RPC URL with a derived Agent Card.
- G2: an external A2A HTTP agent (Hermes at its configured URL, default port 9900) is bridged onto the bus as a bus agent.
- G3: identity, rate limits, trusted peers, loopback default, and TLS are enforced before publish.
- G4: the Hermes interop profile is normative and testable.

Non-goals (M1):
- NG1: serving the HTTP+JSON (REST) binding. Only `protocolBinding: JSONRPC` is served. Open issue 1.
- NG2: gRPC, WebSocket, and push receivers (the gateway only sends push).
- NG3: multi-instance high availability; one gateway process per host.
- NG4: browser clients (CORS is off; see 4.1).
- NG5: pre-1.0 payload shapes. Pre-1.0 method names are accepted as aliases; pre-1.0 bodies are not.
- NG6: changing Hermes server behavior. Divergences are recorded in section 5.

Scope boundary: the bus wire (subjects, headers, error encoding, op tokens) is owned by RFC-0002. This RFC owns the HTTP side and the mapping between the two.

## 3. Terminology

- **Exposed agent**: a bus agent listed in gateway configuration `expose`. Only exposed agents have HTTP routes.
- **Peer**: an authenticated HTTP caller, identified by a peer id that matches the agent-id grammar of RFC-0001. A peer id need not be a bus agent.
- **Effective origin**: the identity recorded on bus messages and used for ownership. It is the peer id of an authenticated HTTP request, or the envelope sender for a bus-native request (RFC-0002 header rule).
- **Derived card**: the Agent Card the gateway serves over HTTP, produced from the bus card (RFC-0003/RFC-0004).
- **Blocking wait**: the gateway-side wait for a stream-end event that implements a non-streaming SendMessage over an asynchronous bus.
- **Stream end**: the first event for a task whose state is terminal or interrupted, marked with header `A2AB-Stream-End: true` (RFC-0002).
- **Outbound bridge**: a bus worker whose backend is an external A2A HTTP agent (section 4.9).
- **External task id**: the task id assigned by an external agent; distinct from the bus task id.
- **Gateway-local error**: an error produced by the gateway at HTTP admission, not by a bus worker (codes -32050, -32051, -32052 and the transport codes in 4.6).

## 4. Specification

### 4.1 Listeners and exposure

- [0008-001] The gateway MUST bind `127.0.0.1` (or `::1`) by default, on port 9910 unless configured otherwise.
- [0008-002] The gateway MUST refuse to start when the configured listener is not loopback unless all of the following hold: TLS is configured (TLS 1.3 minimum, cert and key files owner-only per 4.9), at least one peer has a token digest, and `trusted_peers` is non-empty.
- [0008-003] The gateway MUST expose only agents listed in `expose`. The default is empty. The agent id `gateway` MUST NOT be exposed.
- [0008-004] Each peer MUST list the agents it may reach (`agents`). A request to an agent outside that list MUST be answered as "agent not found" (4.6), without revealing whether the agent exists.
- [0008-005] CORS is off. The gateway MUST NOT emit `Access-Control-*` headers and MUST reject any request carrying an `Origin` header with HTTP 403 (JSON-RPC `-32600`, reason `ORIGIN_NOT_ALLOWED`). A future allowlist (`cors_allow`, default empty) is reserved and not implemented in M1.
- [0008-006] On a loopback listener the gateway MUST validate the `Host` header against `127.0.0.1`, `localhost`, `[::1]` with the bound port, or `public_base_url`; anything else gets HTTP 400 (JSON-RPC `-32600`, reason `HOST_NOT_ALLOWED`).
- [0008-007] The gateway MUST NOT derive any URL from request headers (`Host`, `X-Forwarded-*`). Card URLs come only from `public_base_url`.
- [0008-008] The gateway MUST NOT trust forwarded-address headers for identity or rate-limit keys. The socket peer address is the only source address.
- [0008-009] Loopback anonymous access (4.3, `loopback_anonymous`) MUST be disabled whenever a reverse proxy forwards to this listener, because the proxy's loopback address would otherwise be treated as a local caller. Operators declare this in configuration; the default is enabled only for direct loopback.

### 4.2 Agent routes and cards

- [0008-010] Each exposed agent has a base URL `http://<host>:<port>/agents/<agentId>` (no trailing slash). JSON-RPC POSTs are accepted at exactly that path and at that path with one trailing slash.
- [0008-011] `GET <base>/.well-known/agent-card.json` MUST return the derived card for that agent. It is public (no authentication), carries `ETag` and `Cache-Control: max-age=60`, and returns HTTP 304 on a matching `If-None-Match`.
- [0008-012] If `default_agent` is configured (M1: `omp`) and exposed, the root paths `/.well-known/agent-card.json` and `POST /` serve that agent. The root card is the same document as the agent's own card, including the canonical interface URL `<public_base_url>/agents/<agentId>`. Otherwise the root paths MUST return HTTP 404, and a POST to `/` MUST return HTTP 404 with JSON-RPC error `-32601`, reason `NO_AGENT_SELECTED`. Root routing never selects an agent other than `default_agent`.
- [0008-013] The derived card MUST contain: `name`, `description`, `version`, `skills`, `defaultInputModes`, `defaultOutputModes`, `capabilities` (`streaming` true; `pushNotifications` per configuration; `extendedAgentCard` as the bus card states), and the bus card's extension declarations, unchanged. It MUST add one HTTP bearer security scheme in `securitySchemes` (exact field shape per RFC-0003 and the card notes in `docs/interop/a2a-spec-notes.md`, `[UNVERIFIED]`). It MUST NOT declare a card-level security requirement, because the card itself is public.
- [0008-014] The derived card's `supportedInterfaces` MUST contain exactly one entry: `url` = `<public_base_url>/agents/<agentId>`, `protocolBinding` = `JSONRPC`, `protocolVersion` = `1.0`. The `tenant` field MUST be omitted.
- [0008-015] The derived card MUST NOT contain bus URLs, bus credentials, bus interface entries, or any field marked internal by RFC-0003.
- [0008-016] The derived card is an unsigned view. The gateway MUST remove `signatures` and MUST NOT copy the bus card's JWS, because the content differs and the signature would not verify.
- [0008-017] If the agent is not exposed or no bus card exists, the card route MUST return HTTP 404 (not 503).
- [0008-018] The gateway MUST serve cards from an in-memory cache fed by a watch on the card bucket (RFC-0004). Card requests MUST NOT wait on the bus.
- [0008-019] `GetExtendedAgentCard` requires an authenticated peer on the agent's allowlist. Unauthenticated calls get HTTP 401. If the agent declares no extended card, the gateway returns `-32007` (4.6).

### 4.3 Request admission

- [0008-020] Requests MUST be checked in this order, and no bus publish happens before step 9: (1) Host and Origin (4.1); (2) method and `Content-Type`; (3) body size; (4) authentication; (5) agent route and peer allowance; (6) body parse; (7) `A2A-Version`; (8) rate limit and trust gate; (9) method dispatch and parameter rules.
- [0008-021] Authentication MUST use `Authorization: Bearer <token>`. The gateway MUST store only SHA-256 digests of peer tokens and MUST compare digests in constant time. A missing or invalid token MUST return HTTP 401 with `WWW-Authenticate: Bearer` and JSON-RPC `-32050`. A presented invalid token MUST NOT fall back to loopback anonymous access.
- [0008-022] With `loopback_anonymous` enabled, a request with no `Authorization` header from a loopback socket MUST be assigned the effective origin `loopback`. This origin MUST NOT be a member of `trusted_peers` by implication and MUST use the `loopback` rate policy.
- [0008-023] Rate limits MUST be applied per effective origin (default 60 requests per 60 s, configurable) and per source address for failed authentication (default 30 per 60 s). Excess requests MUST return HTTP 429 with `Retry-After` and JSON-RPC `-32051` (reason `RATE_LIMITED`).
- [0008-024] Non-loopback callers MUST be listed in `trusted_peers`; otherwise HTTP 403 with JSON-RPC `-32052` (reason `PEER_NOT_TRUSTED`). Loopback callers are not subject to the list.
- [0008-025] `Content-Type` MUST be `application/json` (parameters allowed). Otherwise HTTP 415 with JSON-RPC `-32600`, reason `UNSUPPORTED_MEDIA_TYPE`.
- [0008-026] A request body larger than 1 MiB MUST be rejected with HTTP 413 before parsing.
- [0008-027] The `A2A-Version` header MUST be `1.0` or `1.0.0`. Any other value, including an absent header, MUST yield a JSON-RPC `-32009` (VersionNotSupportedError, reason `VERSION_NOT_SUPPORTED`) with HTTP 200. Deviation from the spec-notes default that an absent header means 0.3: the gateway serves no 0.3 payload shapes (NG5), so it cannot answer such a caller usefully. See Rationale.
- [0008-028] `A2A-Extensions` (request) MUST be parsed. Each required extension of the bus card that the request does not list MUST yield `-32008` (reason `EXTENSION_SUPPORT_REQUIRED`). The response `A2A-Extensions` header, present only when the request carried one, MUST list only URIs that the request listed and the gateway activated. The gateway MUST NEVER add an extension URI on behalf of an HTTP peer, including the approval extension.
- [0008-029] The gateway MUST emit only canonical PascalCase method names. It MUST accept the pre-1.0 aliases of `docs/interop/a2a-spec-notes.md` section 7.2 and the Hermes aliases `tasks/subscribe`, `tasks/pushNotificationConfig/create`, and `tasks/pushNotification/set` on ingress only.
- [0008-030] Alias methods MUST carry v1.0 parameter shapes. Pre-1.0 shapes (`kind` discriminators, lowercase roles, pre-1.0 field names) MUST yield `-32602` with message `pre-1.0 parameter shape not accepted`.
- [0008-031] An inbound `Message.metadata` or `Part.metadata` key that starts with the approval or delegation extension URI prefix (`urn:a2abus:ext:approval:` or `urn:a2abus:ext:delegation:`) MUST be rejected with `-32602`, reason `EXTENSION_NOT_ALLOWED_FROM_PEER` (domain `a2abus.v1`). The gateway MUST NOT strip these silently.
- [0008-032] The gateway MUST overwrite the provenance extension key (`urn:a2abus:ext:provenance:1`) on every inbound message with its own record: `{"source": "http-jsonrpc", "peer": "<effective origin>", "transport": "http"}`. Any caller-supplied provenance value MUST be discarded.
- [0008-033] Budget (`urn:a2abus:ext:budget:1`, RFC-0006): if the caller supplies budget values, the gateway MUST set each limit to the minimum of the caller's value and the peer's `ingress_budget`. If the caller supplies none, the gateway MUST set the peer's `ingress_budget`. The gateway MUST NEVER raise a limit.
- [0008-034] The gateway MUST NOT mint, forward, or synthesize approval decisions (RFC-0006). A continuation message from an HTTP peer is an ordinary message; it does not satisfy an approval gate.
- [0008-035] The gateway MUST NOT rewrite `contextId` or task ids in either direction. If a SendMessage has no `contextId`, the gateway passes none and the worker assigns one (RFC-0005).
- [0008-036] Every bus publish made for an HTTP request MUST carry `A2AB-Origin` set to the effective origin (RFC-0002 header rule).

### 4.4 Operation mapping

- [0008-037] The gateway MUST implement the operations below. Bus op tokens are those of RFC-0002 (the subject is `a2a.v1.<tenant>.rpc.<agent>.<op>`; the tenant is `local` in M1).

| JSON-RPC method | Bus op | Request body (proto-JSON) | Reply | Notes |
|---|---|---|---|---|
| SendMessage | `send` | `SendMessageRequest` | initial `Task` | blocking emulation, 4.7 |
| SendStreamingMessage | `stream` | `SendMessageRequest` | initial `Task` plus subscription | 4.7 |
| GetTask | `get` | `{id, historyLength?}` | `Task` | ownership check (0008-043) |
| ListTasks | `list` | `ListTasksRequest` | `ListTasksResponse` | filtered by effective origin by the worker |
| CancelTask | `cancel` | `{id}` | `Task` | ownership check |
| SubscribeToTask | `subscribe` | `{id}` | snapshot `Task` plus subscription | 4.7 |
| CreateTaskPushNotificationConfig | `push-create` | config object | config | 4.8 |
| GetTaskPushNotificationConfig | `push-get` | `{taskId, id}` | config | 4.8 |
| ListTaskPushNotificationConfigs | `push-list` | `{taskId}` | list | 4.8 |
| DeleteTaskPushNotificationConfig | `push-delete` | `{taskId, id}` | empty | 4.8 |
| GetExtendedAgentCard | `card-ext` | empty | extended card | 0008-019 |

- [0008-038] `ListTasks` `pageSize` MUST be between 1 and 100 (default 50, per the spec). Values above 100 MUST yield `-32602`. The gateway MUST NOT cache list results.
- [0008-039] Push configuration records are stored in the bus KV bucket named by RFC-0010 (the gateway does not own a bucket name). Records MUST NOT hold tokens or credentials (4.8).
- [0008-040] Ownership: for any operation that names a task, the gateway MUST pass the effective origin to the worker, and the worker MUST answer `TaskNotFound` (`-32001`) for a task owned by another origin, identical to a missing task. The gateway MUST NOT disclose the existence of other origins' tasks. Operator visibility exceptions of RFC-0005 apply only to bus-side operator identities; an HTTP peer is never an operator identity, and the gateway never forwards one.
- [0008-041] The gateway MUST NOT reorder, merge, or otherwise rewrite `Task` content. Its only egress changes are the sequence removal in 4.5 and the redaction in RFC-0013.

### 4.5 Extensions, metadata, and Struct form

- [0008-042] Extension data travels in a `metadata` protobuf `Struct` keyed by the extension URI (RFC-0003). In JSON bodies it appears as a plain object keyed by that URI, for example `"metadata": {"urn:a2abus:ext:budget:1": {"maxTurns": 5}}`.
- [0008-043] Integers inside extension Structs MUST be exactly representable as IEEE-754 doubles (absolute value at most 2^53), because a Struct number is a double.
- [0008-044] On egress the gateway MUST remove the key `urn:a2abus:ext:sequence:1` from every `metadata` map (RFC-0005 ordering is internal, L8). Every other extension key passes through unchanged.

### 4.6 Errors

- [0008-045] Protocol errors MUST be JSON-RPC error objects whose `code` and `data` follow `docs/interop/a2a-spec-notes.md` section 6. `data` MUST be an array containing a `google.rpc.ErrorInfo` object with `reason`, `domain`, and `metadata` (may be empty). The domain is `a2a-protocol.org` for reasons defined by the A2A spec. Every A2A-Bus reason, including gateway-local reasons returned only on HTTP, uses domain `a2abus.v1` (RFC-0002). The `message` is human-readable and MUST NOT contain tokens, bus subjects, stack traces, or internal identifiers.
- [0008-046] Each error MUST use the HTTP status given for its reason in the table below. Reasons whose row gives HTTP 200 MUST be returned with HTTP 200, including bus-side reasons.

| JSON-RPC code | Reason (ErrorInfo) | Source | HTTP status | Meaning |
|---|---|---|---|---|
| -32700 | PARSE_ERROR | gateway | 400 | body is not JSON |
| -32600 | INVALID_REQUEST | gateway | 400 | malformed envelope; also ORIGIN_NOT_ALLOWED (403), HOST_NOT_ALLOWED (400), UNSUPPORTED_MEDIA_TYPE (415) |
| -32601 | METHOD_NOT_FOUND | gateway | 200 | unknown method; also NO_AGENT_SELECTED and AGENT_NOT_FOUND (404) |
| -32602 | INVALID_PARAMS | bus or gateway | 200 | invalid parameters; gateway reasons EXTENSION_NOT_ALLOWED_FROM_PEER, PUSH_URL_NOT_PERMITTED, PUSH_CREDENTIALS_NOT_SUPPORTED, PUSH_CONFIG_LIMIT, TENANT_MISMATCH |
| -32603 | INTERNAL | gateway or bus | 200 | unexpected failure; also AGENT_UNAVAILABLE and OVERLOADED (503 with Retry-After) |
| -32001 | TASK_NOT_FOUND | bus | 200 | missing or not owned by the origin |
| -32002 | TASK_NOT_CANCELABLE | bus | 200 | task already terminal |
| -32003 | PUSH_NOTIFICATION_NOT_SUPPORTED | bus or gateway | 200 | push disabled for the agent or peer |
| -32004 | UNSUPPORTED_OPERATION | bus | 200 | e.g. SubscribeToTask on a terminal task |
| -32005 | CONTENT_TYPE_NOT_SUPPORTED | bus | 200 | message part media type not accepted |
| -32006 | INVALID_AGENT_RESPONSE | bus | 200 | worker produced an invalid reply |
| -32007 | EXTENDED_AGENT_CARD_NOT_CONFIGURED | bus | 200 | agent has no extended card |
| -32008 | EXTENSION_SUPPORT_REQUIRED | gateway | 200 | required extension not requested |
| -32009 | VERSION_NOT_SUPPORTED | gateway | 200 | `A2A-Version` not `1.0` |
| -32050 | UNAUTHORIZED | gateway | 401 | missing or invalid bearer token (Hermes-compatible code) |
| -32051 | RATE_LIMITED | gateway | 429 | rate limit exceeded; `Retry-After` set (Hermes-compatible code) |
| -32052 | PEER_NOT_TRUSTED | gateway | 403 | non-loopback caller not in `trusted_peers` (Hermes-compatible code) |

- [0008-047] Codes -32050 through -32052 are gateway-local and used only at HTTP admission. The gateway MUST NOT emit them for bus errors. They match Hermes' codes (`docs/interop/hermes.md` section 1.7) so that Hermes-side error mapping still works.
- [0008-048] A bus error whose reason is not in the table MUST map to `-32603`, reason `INTERNAL`, and the gateway MUST log the bus reason with a correlation id (RFC-0013), not the reason text to the client.
- [0008-049] When the bus has no responder for the agent (RFC-0010 no-responders), the gateway MUST answer HTTP 503, `Retry-After`, JSON-RPC `-32603`, reason `AGENT_UNAVAILABLE`. When the worker rejects admission for overload, the gateway MUST answer HTTP 503 with reason `OVERLOADED`. Overload after admission is a task-level `TASK_STATE_REJECTED` (R-S6), not an error.

### 4.7 Streaming, blocking, and subscription

- [0008-050] A non-streaming SendMessage with `returnImmediately` absent or false MUST: publish `send`; receive the initial `Task`; subscribe to `ev.<agent>.<task_id>` with header `A2AB-After-Seq: 0` (replay from seq 1, RFC-0005 replay rule); wait for the first event carrying `A2AB-Stream-End: true` (RFC-0005 stream-end rule, 0008-053); and return `{"task": <assembled Task>}`. The assembled Task is the initial `Task` with each status update applied and each artifact update applied in stream order (`append` and `lastChunk` per the spec).
- [0008-051] With `returnImmediately` true, the gateway MUST return the initial `Task` as soon as the reply arrives and MUST release its subscription.
- [0008-052] The blocking wait deadline (`blocking_wait`, default 100 s) MUST be shorter than the default Hermes client timeout (120 s). When the deadline passes, the gateway MUST return HTTP 200 with the current non-terminal `Task` (state `TASK_STATE_SUBMITTED` or `TASK_STATE_WORKING`), release its subscription, and MUST NOT cancel the task. The caller continues with GetTask or SubscribeToTask.
- [0008-053] Interrupted state `TASK_STATE_INPUT_REQUIRED` ends a blocking wait with a normal result (RFC-0005, RFC-0006). It is not an error. A peer `AUTH_REQUIRED` is not a bus state in M1; it reaches the bus as a FAILED task (0008-079).
- [0008-054] SendStreamingMessage MUST respond HTTP 200 with `Content-Type: text/event-stream`. Each frame is `data: <JSON-RPC response>` followed by a blank line, where `result` is a `StreamResponse`. The first frame MUST be the `Task` (snapshot). Later frames are `statusUpdate` or `artifactUpdate` in stream order. The stream closes after the event with `A2AB-Stream-End: true` is sent.
- [0008-055] While a streamed task is non-terminal, the gateway MUST send a keepalive comment (`: keepalive`) at least every 15 s.
- [0008-056] The gateway MUST drop any event whose `seq` is not greater than the last delivered `seq` for that task. Events lacking `seq` MUST be ordered by bus arrival (RFC-0005 rules).
- [0008-057] SubscribeToTask MUST subscribe to the event subject with `A2AB-After-Seq: 0` before it reads the snapshot. It then reads the current `Task` snapshot, sends it as the first frame, drops events received before the snapshot read (already reflected in it), and delivers later events as in 0008-054 and 0008-056. If the task is already terminal, the gateway MUST answer `-32004` (UnsupportedOperationError) instead of a stream and release the subscription. A closed HTTP stream MUST NOT cancel the task.

### 4.8 Push notifications

- [0008-058] A push config record MUST hold: config id, task id, effective origin, `url`, and creation time. It MUST NOT hold tokens, credentials, or secrets (0008-059, 0008-063).
- [0008-059] Create requests with a non-empty `authentication` or `token` MUST yield `-32602`, reason `PUSH_CREDENTIALS_NOT_SUPPORTED`. In M1 the only authentication of webhook deliveries is the HMAC signature in 0008-062.
- [0008-060] The push `url` MUST use `https` (or `http` to a loopback host when the peer's `push_allow` says so). Its host MUST match the peer's `push_allow` list. Otherwise `-32602`, reason `PUSH_URL_NOT_PERMITTED`. The gateway MUST re-check the resolved address at delivery against the same policy (the SSRF guard), and it MUST NOT follow redirects.
- [0008-061] Delivery: the gateway subscribes to the task's event subject with a durable consumer per config, and POSTs each `StreamResponse` event to `url` with `Content-Type: application/a2a+json`, a 10 s timeout, and up to 5 attempts with 1, 2, 4, 8 s backoff. Delivery is at-least-once; receivers MUST deduplicate by `(taskId, seq)`. Hermes' own push sender uses `application/json` (`docs/interop/hermes.md` section 6.8); this is a known divergence, and the gateway keeps `application/a2a+json`.
- [0008-062] Each POST MUST carry `X-A2A-Signature: <lowercase hex HMAC-SHA256>` computed with the peer's push secret over the exact body bytes. The body MUST be serialized as JSON with object keys sorted by code point, separators `", "` and `": "`, and `ensure_ascii` disabled. This matches the Hermes signing convention (`docs/interop/hermes.md` section 6.8), so a receiver that re-serializes with the same rules verifies the signature.
- [0008-063] Push secrets MUST be stored in the host secret store (4.9), keyed by peer, not per task and not on the bus.
- [0008-064] A task MUST have at most 5 push configs per origin. A sixth Create yields `-32602` with reason `PUSH_CONFIG_LIMIT`.
- [0008-065] The gateway is a push sender only. It MUST NOT accept inbound push deliveries.

### 4.9 Secrets, TLS, and the outbound bridge

- [0008-066] Peer tokens are stored as SHA-256 digests in the gateway configuration. Outbound tokens and push secrets are referenced as `env:<NAME>` or `file:<path>`. Secret files MUST be owner-only: mode 0600 on Linux, owner-only ACL on Windows. The gateway MUST refuse to start when a referenced secret file is wider.
- [0008-067] Secrets MUST NOT appear on the bus, in Agent Cards, in logs, in audit records, or in error messages (RFC-0013 redaction). The `Authorization` header is redacted in every log path.
- [0008-068] Non-loopback listeners MUST use TLS 1.3 or newer. The gateway MUST verify certificates of every outbound HTTPS connection; no switch disables verification. A custom CA bundle path MAY be configured.
- [0008-069] The outbound bridge MUST send bearer tokens only over `https`, or to loopback addresses over `http`. It MUST refuse to send a bearer token over plaintext to any other host.
- [0008-070] Ownership: this section specifies the wire behavior of the outbound bridge. The bridge runs as a bus worker whose backend is `driver-a2a-http` (RFC-0009). Drivers MUST NOT depend on the gateway (RFC-0001 rule 0001-002), so the bridge's HTTP client is part of the driver and `gateway-http` does not link it. See Open issue 5.
- [0008-071] Card ingestion: the bridge fetches the external card with a 30 s cap. It selects the interface with `protocolBinding` `JSONRPC` and `protocolVersion` `1.0`. If none exists, the agent is not published and the bridge reports `EXTERNAL_VERSION_MISMATCH` in its health status. It refreshes every 300 s and on any error.
- [0008-072] The bus agent id of a bridged agent comes from bridge configuration. It MUST NOT be derived from the external card's `name`.
- [0008-073] The published bus card for a bridged agent MUST NOT contain the external URL. It MUST carry the provenance extension with `{"source": "external-a2a-http", "verified": false}` (RFC-0007) and MUST be signed with the bridge's agent key (RFC-0007).
- [0008-074] Transport: when the external card declares `capabilities.streaming`, the bridge MUST use `SendStreamingMessage` and reattach with `SubscribeToTask` after a dropped stream. Otherwise it MUST use `SendMessage` and poll `GetTask` every 2 s, backing off to 10 s. The external request timeout is 330 s (0009-053: the Hermes server reply timeout of 300 s plus a margin). A stream with no frame or keepalive for 30 s MUST be treated as dropped. The Hermes server sends keepalives every 5 s (`docs/interop/hermes.md` section 3).
- [0008-075] The external context id MUST equal the bus context id (no rewrite; 0008-035). Hermes' ownership binding (peer, tenant, contextId) is therefore stable across the hop.
- [0008-076] External task ids are stored in a bus KV bucket owned by the bridge (bucket name per RFC-0010), keyed by bus task id. On start the bridge rebuilds its mapping from that bucket.
- [0008-077] Transport retries MUST reuse the A2A `messageId` of the logical message. A new bus message to the external agent MUST use a new `messageId`. Hermes counts admissions per `(contextId, messageId)` and treats retries with the same pair as non-admissions (`docs/interop/hermes.md` sections 4 and 6.2), so reuse keeps the anti-loop budget correct.
- [0008-078] Outbound error mapping (bridge to bus task state; the reason is a token from RFC-0009, carried in status metadata per RFC-0005):

| External condition | Bus task result | Reason |
|---|---|---|
| HTTP 401/403, JSON-RPC -32050/-32052 (bridge health `PEER_AUTH_FAILED`) | `TASK_STATE_FAILED` | `BACKEND_ERROR` |
| HTTP 429, JSON-RPC -32051 (after bounded retries; bridge health `PEER_RATE_LIMITED`) | `TASK_STATE_REJECTED` | `RATE_LIMITED` |
| JSON-RPC -32001 on poll or resubscribe | `TASK_STATE_FAILED` | `BACKEND_LOST_TASK` |
| JSON-RPC -32600, -32602 | `TASK_STATE_FAILED` | `BACKEND_ERROR` |
| JSON-RPC -32009 | `TASK_STATE_FAILED` | `BACKEND_ERROR` |
| Timeout (330 s per 0009-053, or stream idle 30 s without reattach success) | `TASK_STATE_FAILED` | `BACKEND_TIMEOUT` |
| Connection refused or unreachable after bounded retries | `TASK_STATE_FAILED` | `BACKEND_CRASHED` |

- [0008-079] External terminal and interrupted states (`COMPLETED`, `FAILED`, `CANCELED`, `INPUT_REQUIRED`) MUST pass through unchanged, including external text. A peer `REJECTED` maps per 0009-047 (bus FAILED with `AGENT_REFUSED`). A peer `AUTH_REQUIRED` maps to bus FAILED with `BACKEND_ERROR` (0009-047). The bridge's bus failure reason is a bare token from the RFC-0009 set that driver-a2a-http produces (`BACKEND_CRASHED`, `BACKEND_ERROR`, `BACKEND_TIMEOUT`, `BACKEND_LOST_TASK`, `RATE_LIMITED`, `AGENT_REFUSED`). `LOOP_DETECTED`, `BUDGET_EXHAUSTED`, and `AGENT_LIMIT` are not produced by the bridge. The token is the value of the `a2abus.failure` key in status message metadata (RFC-0001; registry RFC-0005 section 4.9). It is bus-visible and is not an ErrorInfo object, so no domain is attached. These reasons appear inside Task results, so HTTP status stays 200 (0008-046). Bridge health classes (`PEER_AUTH_FAILED`, `PEER_RATE_LIMITED`, `EXTERNAL_VERSION_MISMATCH`) are carried in the non-bus `health_class` field of bridge health status (RFC-0009) and never reach the bus or an HTTP body.

### 4.10 Statelessness and caches

- [0008-080] The gateway MUST NOT hold authoritative state. Its caches are: the card cache (from the card bucket), rate-limit counters, in-flight blocking waits, and a short LRU of recently seen `(origin, task)` pairs used only to skip repeat ownership checks.
- [0008-081] A gateway restart MUST lose only rate-limit counters and in-flight waits. Clients recover by GetTask or SubscribeToTask. Nothing else changes.
- [0008-082] The gateway MUST NOT keep a task index. Ownership and submission dedupe belong to the worker (RFC-0005). The gateway forwards the A2A `messageId` and the effective origin, so the worker can dedupe retries of the same HTTP request even though each HTTP request gets a fresh `A2AB-Msg-Id`.
- [0008-083] Each bridge keeps its external-id mapping in bus KV (0008-076), so a restarted bridge rebuilds without a local store.
- [0008-084] M1 runs one gateway process per host. Rate limits are per process. Multi-instance deployments are out of scope (NG3).

### 4.11 Hermes profile

- [0008-085] For Hermes-profile peers the gateway MUST accept the Hermes request shape exactly: `POST` to the agent base path; headers `Content-Type: application/json`, `A2A-Version: 1.0`, optional `Authorization: Bearer`; no `A2A-Extensions`; body `SendMessage` with `params.message` carrying `role: "ROLE_USER"`, `parts` as `{text, mediaType}`, `messageId` (32 lowercase hex characters, a uuid4 without hyphens), and `contextId` (string). The JSON-RPC `id` may be a 32-hex string.
- [0008-086] Hermes-profile responses MUST follow the spec: SendMessage returns `result: {"task": ...}` or `{"message": ...}`. Hermes unwraps both (`docs/interop/hermes.md` section 8.4).
- [0008-087] For Hermes-profile peers, the gateway MUST pass `TASK_STATE_INPUT_REQUIRED` unchanged in the state field and MUST pass the text unchanged. No text prefix is added. Hermes detects input-required from the TaskState enum (`docs/interop/hermes.md` section 8.5). The gateway MUST NOT emit `TASK_STATE_AUTH_REQUIRED` to a Hermes-profile peer.
- [0008-088] The gateway MUST NOT require `A2A-Extensions` from Hermes-profile peers. Configuration validation MUST refuse to route a Hermes-profile peer to an agent that declares a required extension.
- [0008-089] If `params.tenant` is present, it MUST equal `local`. Any other value yields `-32602`, reason `TENANT_MISMATCH`. The card omits `tenant` (0008-014), so Hermes normally sends none.
- [0008-090] Hermes card discovery: Hermes fetches `<base>/.well-known/agent-card.json` and falls back to `agent.json` on 404 (`docs/interop/hermes.md` section 1.1, verified against Hermes `tools.py`). The gateway does not serve `agent.json`. If Hermes' fallback to the base URL for RPC is used, it still reaches the agent. The gateway MUST serve correct results for both paths (join order verified; Open issue 4 resolved).
- [0008-091] Anti-loop: the gateway MUST NOT change `contextId` to evade a peer's admission limits, MUST keep the `messageId` stable across retries of one logical message (0008-082), and MUST NOT split one turn into several bus admissions. The bus loop budget (RFC-0006) is the enforced limit.
- [0008-092] Timeouts: the configured `blocking_wait` (default 100 s) plus a 20 s margin MUST not exceed the Hermes peer `timeout` for that agent. The default Hermes client timeout is 120 s. The operations guide MUST record this pairing.
- [0008-093] A Hermes-profile peer whose agent takes longer than `blocking_wait` receives a non-terminal `Task` with no artifact text. This is a known limitation (Open issue 3).

### 4.12 Worked examples

These examples are normative for field names and ordering. Bus payloads are protobuf on the wire (RFC-0001 L1); bodies below show their proto-JSON form. Ids are illustrative. Hermes uses a 32-hex uuid4 `messageId`. Task ids are worker-assigned UUIDv7 values (RFC-0005).

**A. Blocking SendMessage from Hermes to omp**

HTTP request (Hermes peer `hermes`, bearer token redacted):

```
POST /agents/omp HTTP/1.1
Host: 127.0.0.1:9910
Authorization: Bearer <redacted>
Content-Type: application/json
A2A-Version: 1.0

{"jsonrpc":"2.0","id":"4f1c2d9e8b7a4c3d9e2f1a0b6c5d4e3f",
 "method":"SendMessage",
 "params":{"message":{"role":"ROLE_USER",
   "parts":[{"text":"Summarize the build log.","mediaType":"text/plain"}],
   "messageId":"9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4d",
   "contextId":"ctx-9c1e0f4a2b7d3e58"}}}
```

Bus messages published by the gateway (sender identity `gateway`):

```
Subject: a2a.v1.local.rpc.omp.send
Reply-To: _INBOX.gw.7f3a9c
A2AB-Msg-Id: 0193f2a4-6b10-7f00-8a11-2c3d4e5f6a70
A2AB-Origin: hermes
A2A-Version: 1.0
traceparent: 00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01
Content-Type: application/x-protobuf

{"message":{"role":"ROLE_USER",
  "parts":[{"text":"Summarize the build log.","mediaType":"text/plain"}],
  "messageId":"9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4d",
  "contextId":"ctx-9c1e0f4a2b7d3e58",
  "metadata":{"urn:a2abus:ext:provenance:1":{"source":"http-jsonrpc","peer":"hermes","transport":"http"},
              "urn:a2abus:ext:budget:1":{"maxTurns":5}}}}
```

Initial reply on `_INBOX.gw.7f3a9c`:

```
{"task":{"id":"0193f2a4-7c1e-7d0a-9b3e-5f2a8c4d1e60",
  "contextId":"ctx-9c1e0f4a2b7d3e58",
  "status":{"state":"TASK_STATE_SUBMITTED"}}}
```

The gateway then subscribes to `a2a.v1.local.ev.omp.0193f2a4-7c1e-7d0a-9b3e-5f2a8c4d1e60` with header `A2AB-After-Seq: 0` (replay from seq 1). Events:

```
Subject: a2a.v1.local.ev.omp.0193f2a4-7c1e-7d0a-9b3e-5f2a8c4d1e60
A2AB-Msg-Id: 0193f2a4-6c01-7f00-8a11-2c3d4e5f6a71
{"statusUpdate":{"taskId":"0193f2a4-7c1e-7d0a-9b3e-5f2a8c4d1e60",
  "contextId":"ctx-9c1e0f4a2b7d3e58",
  "status":{"state":"TASK_STATE_WORKING"},
  "metadata":{"urn:a2abus:ext:sequence:1":{"seq":1}}}}

{"artifactUpdate":{"taskId":"0193f2a4-7c1e-7d0a-9b3e-5f2a8c4d1e60",
  "contextId":"ctx-9c1e0f4a2b7d3e58",
  "artifact":{"artifactId":"a-1","name":"summary",
    "parts":[{"text":"Build failed at link step.","mediaType":"text/plain"}]},
  "lastChunk":true,
  "metadata":{"urn:a2abus:ext:sequence:1":{"seq":2}}}}

Subject: a2a.v1.local.ev.omp.0193f2a4-7c1e-7d0a-9b3e-5f2a8c4d1e60
A2AB-Msg-Id: 0193f2a4-6c02-7f00-8a11-2c3d4e5f6a72
A2AB-Stream-End: true
{"statusUpdate":{"taskId":"0193f2a4-7c1e-7d0a-9b3e-5f2a8c4d1e60",
  "contextId":"ctx-9c1e0f4a2b7d3e58",
  "status":{"state":"TASK_STATE_COMPLETED"},
  "metadata":{"urn:a2abus:ext:sequence:1":{"seq":3}}}}
```

HTTP response (sequence metadata removed per 0008-044):

```
HTTP/1.1 200 OK
Content-Type: application/json

{"jsonrpc":"2.0","id":"4f1c2d9e8b7a4c3d9e2f1a0b6c5d4e3f",
 "result":{"task":{"id":"0193f2a4-7c1e-7d0a-9b3e-5f2a8c4d1e60",
   "contextId":"ctx-9c1e0f4a2b7d3e58",
   "status":{"state":"TASK_STATE_COMPLETED","timestamp":"2026-10-26T12:00:09Z"},
   "artifacts":[{"artifactId":"a-1","name":"summary",
     "parts":[{"text":"Build failed at link step.","mediaType":"text/plain"}]}]}}}
```

**B. SendStreamingMessage (same peer, new message)**

HTTP request: same headers; body `"method":"SendStreamingMessage"` with the `SendMessageRequest` params; `id` `"s-1"`. The response is `200 OK` with `Content-Type: text/event-stream`:

```
data: {"jsonrpc":"2.0","id":"s-1","result":{"task":{"id":"0193f2a4-8d20-7a11-9c4e-6b3a9d5e2f01","contextId":"ctx-9c1e0f4a2b7d3e58","status":{"state":"TASK_STATE_SUBMITTED"}}}}

data: {"jsonrpc":"2.0","id":"s-1","result":{"statusUpdate":{"taskId":"0193f2a4-8d20-7a11-9c4e-6b3a9d5e2f01","contextId":"ctx-9c1e0f4a2b7d3e58","status":{"state":"TASK_STATE_WORKING"}}}}

: keepalive

data: {"jsonrpc":"2.0","id":"s-1","result":{"artifactUpdate":{"taskId":"0193f2a4-8d20-7a11-9c4e-6b3a9d5e2f01","contextId":"ctx-9c1e0f4a2b7d3e58","artifact":{"artifactId":"a-1","parts":[{"text":"Build failed","mediaType":"text/plain"}]},"lastChunk":false}}}

data: {"jsonrpc":"2.0","id":"s-1","result":{"statusUpdate":{"taskId":"0193f2a4-8d20-7a11-9c4e-6b3a9d5e2f01","contextId":"ctx-9c1e0f4a2b7d3e58","status":{"state":"TASK_STATE_COMPLETED"}}}}
```

Bus: request on `a2a.v1.local.rpc.omp.stream` (same headers as A); the reply is the initial Task; events on `a2a.v1.local.ev.omp.0193f2a4-8d20-7a11-9c4e-6b3a9d5e2f01`; the final event carries `A2AB-Stream-End: true`. The gateway closes the HTTP stream after that event.

**C. CancelTask**

HTTP request:

```
POST /agents/omp HTTP/1.1
Host: 127.0.0.1:9910
Authorization: Bearer <redacted>
Content-Type: application/json
A2A-Version: 1.0

{"jsonrpc":"2.0","id":"c-7","method":"CancelTask",
 "params":{"id":"0193f2a4-8d20-7a11-9c4e-6b3a9d5e2f01"}}
```

Bus request: subject `a2a.v1.local.rpc.omp.cancel`, `A2AB-Origin: hermes`, body `{"id":"0193f2a4-8d20-7a11-9c4e-6b3a9d5e2f01"}`. The worker's reply is the Task in `TASK_STATE_CANCELED`, and the event subject receives a `statusUpdate` with that state and `A2AB-Stream-End: true`.

HTTP response:

```
{"jsonrpc":"2.0","id":"c-7","result":{"task":{"id":"0193f2a4-8d20-7a11-9c4e-6b3a9d5e2f01","contextId":"ctx-9c1e0f4a2b7d3e58","status":{"state":"TASK_STATE_CANCELED"}}}}
```

If the task is already terminal, the worker's reply carries a bus error with ErrorInfo reason `TASK_NOT_CANCELABLE`, and the gateway answers:

```
{"jsonrpc":"2.0","id":"c-8","error":{"code":-32002,"message":"Task cannot be canceled in its current state",
 "data":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"TASK_NOT_CANCELABLE","domain":"a2a-protocol.org","metadata":{}}]}}
```

**D. Rejected version header**

A request with `A2A-Version: 0.3` (or none) returns HTTP 200 with `error.code` `-32009`, reason `VERSION_NOT_SUPPORTED`, and no bus publish occurs.

## 5. Interoperability

- **Stock A2A v1.0 clients (for example the official Python SDK)** see a standard agent card at the per-agent base URL, one JSONRPC interface, standard method names, standard result wrapping, standard `ErrorInfo` errors, and standard SSE framing. They do not see bus URLs or signatures. The card is unsigned; a client that requires signed cards cannot verify it over HTTP (Open issue 6). Pre-1.0 method names work; pre-1.0 bodies do not, which the spec permits.
- **Hermes** (`docs/interop/hermes.md`): Hermes sends the request shape in 0008-085 and tolerates the bare or wrapped result (section 8.4). It reads the reply text from artifacts, then status message. Divergences the profile accepts: Hermes server returns a bare Task for non-1.0 callers, and it returns `-32602` for a bad `A2A-Version` where the spec says `-32009`. The gateway follows the spec (0008-027) and does not emulate Hermes server quirks. The `-32050..-32052` codes are kept for Hermes-side mapping.
- **Hermes card discovery**: Hermes' card fetch and RPC fallback are covered by 0008-010 and 0008-012 with both code paths working.
- **ACP agents (claude, omp)**: unaffected. The gateway talks only to the bus; ACP is between the worker driver and the agent (RFC-0009).
- **Bus-native agents**: unaffected. Their origin is their envelope sender; no HTTP path is involved.
- **Design avoids breaking peers** by: keeping shapes unchanged (only the sequence key is removed on egress); only adding metadata keys on ingress; keeping `contextId` and task ids unchanged; and never requiring an extension header from Hermes.

## 6. Security considerations

Threats addressed (T12 and related rows of `docs/requirements.md`):

- **Unauthenticated or guessed-token access**: bearer digests in constant time, 401 before any route or body parse, failed-auth rate limits per source address.
- **Local cross-user and drive-by browser access**: loopback default bind; no CORS; any request with `Origin` rejected; `Content-Type` must be `application/json`, which a no-preflight cross-origin form cannot send; `Host` validation against DNS rebinding.
- **Forged approvals or delegations through metadata**: rejected at ingress (0008-031); approvals come only from the operator path (RFC-0006); continuation text does not satisfy approval gates (0008-034).
- **Forged provenance or budget raising**: the gateway overwrites provenance and only lowers budget (0008-032, 0008-033).
- **Retry-driven admission inflation**: stable `messageId` and worker dedupe (0008-082, 0008-077).
- **Cross-origin task access and context collision**: ownership by effective origin; contexts scoped by origin at the worker (0008-040, Open issue 8 of RFC-0005 dependency).
- **Secret leakage**: no secrets on bus, cards, logs, or errors; owner-only secret files; refuse to start on wide permissions (0008-066, 0008-067).
- **Server-side request forgery through push**: allowlisted hosts, re-check at delivery, no redirects (0008-060).
- **Proxy misconfiguration**: loopback anonymous disabled when a proxy forwards (0008-009); forwarded headers ignored (0008-008).
- **Weak transport**: TLS 1.3 for non-loopback listeners; certificate verification always on outbound; bearer only over TLS or loopback (0008-068, 0008-069).
- **Denial of service**: body cap, rate limits, blocking-wait deadline that releases resources, bounded retries.

Residual risks: any local process that can open a TCP connection to the loopback port can use the `loopback` origin if `loopback_anonymous` is enabled. Operators on hosts with untrusted local users MUST disable it. Rate counters are per process.

## 7. Rationale and alternatives

- **Per-agent path, not a tenant field or a header**: `tenant` is an isolation namespace in this design (RFC-0001), not an agent selector. A path per agent keeps Hermes' fallback and stock SDK base-URL resolution working, and it avoids the Hermes check that compares tenant with the routed agent.
- **JSON-RPC only in M1**: a single binding keeps the mapping table small. The HTTP+JSON binding can be added later over the same operations (Open issue 1).
- **Blocking emulation with a 100 s deadline**: an asynchronous bus has no natural blocking call. The deadline sits below the Hermes default client timeout of 120 s so Hermes gets a parseable Task instead of a socket timeout. The alternative of holding the connection until the task finishes exceeds Hermes' timeout for long tasks.
- **Absent `A2A-Version` rejected**: the spec's 0.3 default would require serving 0.3 shapes, which the gateway does not do (NG5). Rejecting explicitly tells the caller what is wrong.
- **Unsigned derived card**: signing the derived card would need the agent's private key or a gateway key that could impersonate agents. The bus card keeps its signatures for bus consumers.
- **Push secret per peer, HMAC over canonical body**: keeps the gateway stateless (no per-task secrets) and matches Hermes' header convention. Client-supplied push tokens were rejected because storing them would put receiver credentials into the bus or into per-task local state.
- **Refuse to start on non-loopback without TLS, token, and trusted list**: stricter than Hermes, which returns 403 to every caller in that state. Failing at start is safer than failing per request.
- **Loopback anonymous mirrors Hermes**: zero-configuration local use, with the documented residual risk in section 6.
- **Outbound bridge placement**: the HTTP client belongs with the driver because drivers may not depend on the gateway (RFC-0001 rule 0001-002). The alternative of a shared HTTP client crate is recorded in Open issue 5.

## 8. Conformance

Test plan (not run by this RFC):

- **T-A, stock client**: the official A2A Python SDK (pinned version in the test lock) resolves the derived card at `<base>/.well-known/agent-card.json`, then exercises SendMessage, SendStreamingMessage, GetTask, ListTasks, CancelTask, SubscribeToTask, push create/get/list/delete, and GetExtendedAgentCard. `[UNVERIFIED]`: exact SDK class and method names are to be confirmed against the pinned version.
- **T-B, Hermes-shaped client**: a plain HTTP test client reproduces the exact request of `docs/interop/hermes.md` section 8.3 (headers, body, 32-hex ids) and the tolerant parse of section 8.4. It asserts: bare and wrapped results both parse; a retry with the same `messageId` creates no second worker admission (assert on the worker's admission log); the `[INPUT_REQUIRED] ` prefix appears only for the Hermes profile.
- **T-C, security**: start refused on non-loopback without token or trusted list; Origin rejected; Host rebinding rejected; approval and delegation metadata rejected; provenance overwritten; budget never raised; cross-origin GetTask returns `-32001`; push to non-allowlisted host rejected; no secret in logs or errors (scan captured logs).
- **T-D, bridge**: a fake external A2A server (streaming and non-streaming modes) exercises card ingestion, reattach after dropped stream, poll fallback, each row of the outbound error table, and messageId reuse on retry.

| ID | Test idea | Level |
|---|---|---|
| 0008-001 | Start with defaults; confirm bound address is loopback | MUST |
| 0008-002 | Start with non-loopback bind and each missing precondition; expect refusal | MUST |
| 0008-003 | Config with agent `gateway` in `expose`; expect refusal; default has no routes | MUST |
| 0008-004 | Authenticated peer calls non-allowed agent; expect 404 `AGENT_NOT_FOUND` | MUST |
| 0008-005 | Request with Origin header; expect 403 and no `Access-Control-*` headers | MUST |
| 0008-006 | Request with foreign Host header; expect 400 | MUST |
| 0008-007 | Set X-Forwarded-Host; card URLs unchanged | MUST |
| 0008-008 | Set X-Forwarded-For from loopback; rate key stays socket address | MUST |
| 0008-009 | Config with proxy flag and loopback anonymous; expect refusal or disabled origin | MUST |
| 0008-010 | POST to base path and base path with one trailing slash; both route | MUST |
| 0008-011 | GET card; ETag and 304 on If-None-Match; public without token | MUST |
| 0008-012 | Default agent unset: root card and root POST give 404 and `NO_AGENT_SELECTED`; default set: root card equals the agent card and root POST routes to it | MUST |
| 0008-013 | Card field presence against bus card fixture; bearer scheme present; no card-level security requirement | MUST |
| 0008-014 | Card has one JSONRPC interface, no tenant | MUST |
| 0008-015 | Scan card for bus URLs and internal fields | MUST |
| 0008-016 | Card has no `signatures` | MUST |
| 0008-017 | Card request for unexposed agent; expect 404 | MUST |
| 0008-018 | Stop bus; card still served from cache | MUST |
| 0008-019 | GetExtendedAgentCard without token; expect 401; with token and no extended card; expect -32007 | MUST |
| 0008-020 | Malformed body from unauthenticated caller; expect 401 and no bus publish | MUST |
| 0008-021 | Wrong token; expect 401 and `WWW-Authenticate`; digest-only config check | MUST |
| 0008-022 | Loopback, no header; origin `loopback`; with invalid token, expect 401 | MUST |
| 0008-023 | Exceed rate limit; expect 429 and `Retry-After`; repeated bad tokens rate limited | MUST |
| 0008-024 | Non-loopback caller not in list; expect 403 `PEER_NOT_TRUSTED` | MUST |
| 0008-025 | Content-Type text/plain; expect 415 | MUST |
| 0008-026 | 1 MiB+1 body; expect 413 before parse | MUST |
| 0008-027 | `A2A-Version` absent, `0.3`, `1.0`, `1.0.0`; expect -32009 for first two, success for last two | MUST |
| 0008-028 | Required extension missing; expect -32008; response echoes only requested and activated URIs; no approval URI ever added | MUST |
| 0008-029 | Each alias in the table; expect routed to canonical op; responses use canonical names | MUST |
| 0008-030 | Alias with pre-1.0 shape; expect -32602 | MUST |
| 0008-031 | Approval and delegation metadata keys; expect -32602 | MUST |
| 0008-032 | Caller-supplied provenance; bus message carries gateway record | MUST |
| 0008-033 | Budget higher than peer ingress; expect min; absent budget; expect ingress value | MUST |
| 0008-034 | Continuation after INPUT_REQUIRED from HTTP peer; worker does not treat as approval | MUST |
| 0008-035 | Request with contextId; bus contextId equal; no contextId; worker assigns | MUST |
| 0008-036 | Capture bus publishes; each carries A2AB-Origin | MUST |
| 0008-037 | Method table contract test per row | MUST |
| 0008-038 | pageSize 0, 100, 101; expect -32602 above 100 | MUST |
| 0008-039 | Push create; inspect bus KV record fields | MUST |
| 0008-040 | Two origins; GetTask of other origin's task; expect -32001 identical to missing | MUST |
| 0008-041 | Golden-file compare of Task content through gateway | MUST |
| 0008-042 | Round-trip extension Struct in JSON form | MUST |
| 0008-043 | Integer above 2^53 in extension; expect rejection on ingress and no truncation | MUST |
| 0008-044 | Egress contains no sequence key; other keys intact | MUST |
| 0008-045 | Error body shape and ErrorInfo presence for each row | MUST |
| 0008-046 | HTTP status per table row | MUST |
| 0008-047 | Bus error mapped to gateway-local code; expect failure in test | MUST |
| 0008-048 | Unknown bus reason; expect -32603 INTERNAL and no reason text | MUST |
| 0008-049 | Stop agent; expect 503 `AGENT_UNAVAILABLE`; overload stub; expect 503 `OVERLOADED` | MUST |
| 0008-050 | Blocking send against fake worker; result contains artifacts in order | MUST |
| 0008-051 | returnImmediately true; expect Task at once and subscription released | MUST |
| 0008-052 | Worker slower than deadline; expect WORKING Task at about 100 s; task keeps running | MUST |
| 0008-053 | INPUT_REQUIRED ends blocking wait with 200 | MUST |
| 0008-054 | Stream frame order and close after stream-end header | MUST |
| 0008-055 | Idle stream; keepalive at least every 15 s | MUST |
| 0008-056 | Duplicate seq injected; second copy dropped | MUST |
| 0008-057 | Subscribe before snapshot; pre-snapshot events dropped; terminal task yields -32004; disconnect does not cancel | MUST |
| 0008-058 | Push record field inspection; no token or credentials stored | MUST |
| 0008-059 | Create with token or authentication; expect -32602 | MUST |
| 0008-060 | Push to non-allowlisted host; redirect; resolved private address; expect refusal | MUST |
| 0008-061 | Receiver outage; retries at 1, 2, 4, 8 s; duplicates deduplicated by receiver | MUST |
| 0008-062 | Recompute HMAC over captured body bytes with Python-style serialization | MUST |
| 0008-063 | Inspect secret store; no per-task secret | MUST |
| 0008-064 | Sixth push config for one origin; expect -32602 | MUST |
| 0008-065 | POST to push receiver path on gateway; not served | MUST |
| 0008-066 | Wide-permission secret file; expect refusal | MUST |
| 0008-067 | Scan bus, cards, logs, audit, errors for token and push secret values | MUST |
| 0008-068 | Non-loopback listener TLS version; outbound invalid certificate; expect failure | MUST |
| 0008-069 | Bridge bearer to plaintext non-loopback host; expect refusal | MUST |
| 0008-070 | Dependency check: driver-a2a-http does not link gateway-http | MUST |
| 0008-071 | External card without JSONRPC 1.0 interface; agent not published; health reports mismatch | MUST |
| 0008-072 | External card name changed; bus agent id unchanged | MUST |
| 0008-073 | Bus card has no external URL; provenance `verified: false`; signature verifies | MUST |
| 0008-074 | Streaming card; dropped stream reattaches; non-streaming card polls at 2 s and backs off to 10 s | MUST |
| 0008-075 | Bridged context equals bus context | MUST |
| 0008-076 | Restart bridge; mapping rebuilt from KV | MUST |
| 0008-077 | Retry of external send reuses messageId; new logical message gets new one | MUST |
| 0008-078 | Each outbound table row against fake external server | MUST |
| 0008-079 | External text and terminal states pass through; failure reason is a bare RFC-0009 token under `a2abus.failure` in status metadata; no ErrorInfo domain | MUST |
| 0008-080 | Inventory of gateway state; only permitted caches | SHOULD |
| 0008-081 | Restart gateway mid-stream; client resubscribes and receives later events | MUST |
| 0008-082 | Same HTTP retry after gateway restart; worker reports one admission | MUST |
| 0008-083 | Restart bridge mid-task; mapping rebuilt; no duplicate external task | MUST |
| 0008-084 | Document per-process limits; no multi-instance deployment | SHOULD |
| 0008-085 | T-B request shape against gateway; expect success | MUST |
| 0008-086 | Result wrapper parsed by Hermes-shaped client | MUST |
| 0008-087 | INPUT_REQUIRED for Hermes peer; state enum passed unchanged; no text prefix added; AUTH_REQUIRED never emitted to Hermes | MUST |
| 0008-088 | Hermes peer with required-extension agent; expect configuration refusal | MUST |
| 0008-089 | tenant `local` accepted; other value expects -32602 | MUST |
| 0008-090 | Card 404 on first path; RPC via base URL still works | SHOULD |
| 0008-091 | Repeated retries do not exceed peer anti-loop budget; contextId unchanged | MUST |
| 0008-092 | Configuration validation warns when blocking_wait plus margin exceeds peer timeout | SHOULD |
| 0008-093 | Long agent turn via Hermes peer; non-terminal Task observed | SHOULD |

## 9. Open issues

1. **HTTP+JSON (REST) binding not served in M1.** Default: not served; the card lists only JSONRPC. A later RFC can add it over the same mapping table.
2. **Hermes detection of input-required.** Resolved: Hermes reads the TaskState enum (`docs/interop/hermes.md` section 8.5). No text prefix is added (0008-087).
3. **Long-running tasks over Hermes.** Default: `blocking_wait` 100 s; longer tasks return a non-terminal Task. Hermes has no polling tool, so such results are lost to Hermes users until a Hermes-side change. Operators limit turn length for Hermes-exposed agents.
4. **Hermes card join behavior.** Resolved: Hermes tries the JSON-RPC interface from the card, then the card `url`, then the base URL (`docs/interop/hermes.md` section 1.1). Both the per-agent card path and the base-URL fallback are correct (0008-090).
5. **Crate placement of the HTTP client.** Resolved by integrator ruling: the HTTP client bridge to external A2A agents lives in `driver-a2a-http` (RFC-0009). `gateway-http` does not link it, consistent with rule 0001-002.
6. **Gateway-attested card.** Default: none. The derived card is unsigned (0008-016).
7. **Push credentials and tokens.** Default: rejected in M1 (0008-059); HMAC only.
8. **Multi-instance rate limits.** Default: one process per host (0008-084).
9. **Loopback anonymous default.** Default: enabled for direct loopback, disabled automatically when a proxy is declared (0008-009). Operators on shared hosts disable it.
10. **Failure reason location.** Resolved: the `a2abus.failure` key in status message metadata (plain key, not an extension), value is the bare RFC-0009 token; registry owned by RFC-0005 section 4.9. Bus-visible reasons carry no ErrorInfo domain (0008-079).
11. **ListTasks page token format.** Default: opaque, worker-defined; the gateway passes it through unchanged.
12. **Hermes `[UNVERIFIED]` ErrorInfo spelling.** Reasons in this RFC follow `docs/interop/a2a-spec-notes.md` section 6.1, whose spelling is `[UNVERIFIED]`. Default: adopt the names as listed and confirm against the official proto before implementation.
