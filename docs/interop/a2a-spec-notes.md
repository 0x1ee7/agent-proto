# A2A v1.0 — implementation notes for a message-bus binding and HTTP gateway

Scope: facts needed to implement (a) a custom "A2A over message bus" binding (NATS/JetStream reference) and (b) an HTTP gateway. Derived from the spec and normative proto. Citations use `[S §x.y]` = https://a2a-protocol.org/latest/specification/ and `[P]` = https://a2a-protocol.org/latest/definitions/ (normative `lf.a2a.v1` proto). Migration facts: `[W1]` = https://a2a-protocol.org/latest/whats-new-v1/. Pre-1.0 names: `[V03]` = https://a2a-protocol.org/v0.3.0/specification. `[INFERENCE]` = our design choice, not in spec. `[UNVERIFIED]` = not confirmed.

Spec-internal inconsistencies are listed in §10; check them before relying on an example.

---

## 1. Bindings, custom bindings, functional equivalence

| Item | Rule | Source |
|---|---|---|
| Transport | A2A MUST run over HTTP(S). Note: a custom bus binding is not HTTP at the wire level. | [S §3.1] |
| Core bindings | JSON-RPC 2.0, gRPC, HTTP+JSON/REST. Agents MUST implement at least one. | [S §3.1, §3.2] |
| Functional equivalence | All supported bindings MUST expose the same operations, same semantics, same errors, same auth schemes. | [S §5.1, §3.4.1] |
| Method map | Abstract operations map to binding-specific names (table §5.3 of spec; section 3). | [S §5.3] |
| Custom binding ID | Custom bindings SHOULD be identified by a URI in `AgentInterface.protocolBinding`. Breaking change => new URI (`/v1` -> `/v2`). | [S §5.8, §12.7] |
| Custom binding guidelines | Section 12.1–12.8 of spec describe what a custom binding must define. | [S §12] |
| Extension transports | Extra transports MUST keep functional equivalence, use namespaced identifiers, be documented. | [S §3.2.4] |
| Transport selection | Static, from AgentCard. Client picks any declared interface; SHOULD fall back if preferred fails. No runtime negotiation. | [S §3.4.2, §8.3.2] |

### 1.1 AgentInterface (proto)

```
message AgentInterface {
  string url = 1;               // endpoint URL (for bus binding: an address the binding defines)
  string protocol_binding = 2;  // open string; JSON name protocolBinding
  string tenant = 3;            // optional default tenant for requests on this interface
  string protocol_version = 4;  // JSON protocolVersion, e.g. "1.0"
}
```
Source: [P] `AgentInterface`; [S §8.3.1] ("supportedInterfaces ... first entry is preferred; each interface MUST accurately declare transport and URL; URLs MAY be reused").

Example from spec §5.8 (custom binding):
```json
{ "supportedInterfaces": [ { "url": "wss://agent.example.com/a2a/websocket",
  "protocolBinding": "https://example.com/bindings/websocket/v1", "protocolVersion": "1.0" } ] }
```
[S §5.8]

Notes for a bus binding:
- `url` is required by proto semantics (spec: "MUST accurately declare ... URL"). For a bus binding, define the URL format yourself, e.g. `nats://host:4222/a2a.<tenant>` `[INFERENCE]`. Spec does not define bus URL schemes.
- `protocolBinding` SHOULD be a URI. `[INFERENCE]` use e.g. `https://<your-domain>/bindings/nats-jetstream/v1`; spec gives no reserved name.
- `protocolVersion` is per interface, not per card [W1 AgentCard section].

### 1.2 Binding-level error mapping (summary; full table in §6)
- JSON-RPC: errors are JSON-RPC error objects with A2A codes -32001..-32009.
- gRPC: `google.rpc.Status` with `ErrorInfo` (see §7.3).
- HTTP+JSON: `google.rpc.Status` JSON body, `ErrorInfo` REQUIRED [S §11.6].
- Bus binding: no spec mapping exists `[UNVERIFIED]`. Recommended `[INFERENCE]`: reply envelope carries the same `google.rpc.Status`-shaped error object as HTTP+JSON, including `ErrorInfo.reason` and `domain`.

---

## 2. Extensions

| Item | Rule | Source |
|---|---|---|
| Declaration | `AgentCapabilities.extensions` = repeated `AgentExtension` | [P], [S §4.6.1] |
| AgentExtension fields | `uri` (string), `description` (string), `required` (bool), `params` (Struct) | [P] |
| Request opt-in (HTTP/JSON-RPC/gRPC metadata) | Header `A2A-Extensions`: comma-separated URIs | [S §3.2.6, §14.2.2] |
| Service-parameter header prefix | `a2a-` prefix for gRPC metadata / HTTP headers | [S §3.2.6, §10.2] |
| Message-level | `Message.extensions` (repeated string URIs) + `Message.metadata` keyed by extension URI | [P], [S §4.6] |
| Artifact-level | `Artifact.extensions` + `Artifact.metadata` keyed by URI | [P], [S §4.6] |
| Task status | TaskStatus message extension points (per spec §4.6) | [S §4.6] |
| Unsupported non-required | Ignore | [S §4.6] |
| Unsupported required | `ExtensionSupportRequiredError` (-32008) when the client did not request it | [S §3.3.4, §5.4] |
| Breaking change | New URI. No automatic fallback to previous version. | [S §4.6] |
| Extension methods | JSON-RPC `{namespace}/{action}`; MUST NOT conflict with core names | [S §3.5.5] |

Bus binding `[INFERENCE]`: carry `A2A-Extensions` as a bus message header (e.g., NATS header `A2A-Extensions`). Use the same semantics: unknown non-required extension => ignore; required and not requested => error.

---

## 3. Task model, state, lifecycle

### 3.1 Objects

| Object | Key fields (JSON names) | Source |
|---|---|---|
| Task | `id`(required), `contextId`, `status`(required: TaskStatus), `artifacts[]`, `history[]`, `metadata` | [S §4.1.1] [P] |
| TaskStatus | `state` (TaskState), `message` (Message), `timestamp` (ISO 8601 UTC, ms precision) | [P], [W1] |
| Message | `messageId`, `contextId`, `taskId`, `role` (ROLE_USER/ROLE_AGENT), `parts[]`, `metadata`, `extensions[]`, `referenceTaskIds[]` | [P] |
| Part | `oneof content { text | raw(bytes) | url | data(Value) }` + `metadata`, `filename`, `mediaType` | [P], [W1] |
| Artifact | `artifactId`, `name`, `description`, `parts[]`, `metadata`, `extensions[]` | [P] |

Part discrimination is by member presence (`text`, `raw`, `url`, `data`), not a `kind` field [W1 Part section].

### 3.2 TaskState (proto enum, JSON strings)

| Proto / JSON | Class | Source |
|---|---|---|
| `TASK_STATE_SUBMITTED` | non-terminal | [P], [W1] |
| `TASK_STATE_WORKING` | non-terminal | [P] |
| `TASK_STATE_INPUT_REQUIRED` | interrupted | [S §4.1.3] (interrupted), [P] |
| `TASK_STATE_AUTH_REQUIRED` | interrupted | [S §7.6.1] (MUST transition), [P] |
| `TASK_STATE_COMPLETED` | terminal | [S §3.3 / §5.4 SubscribeToTask] |
| `TASK_STATE_FAILED` | terminal | [S §3.1.6] |
| `TASK_STATE_CANCELED` | terminal | [S §3.1.5] |
| `TASK_STATE_REJECTED` | terminal | [S §3.1.6] |
| `TASK_STATE_UNSPECIFIED` | default (0) | [P] `[UNVERIFIED]` meaning |

Terminal: COMPLETED, FAILED, CANCELED, REJECTED (spec SubscribeToTask row, §5.3). Interrupted (task may resume): INPUT_REQUIRED, AUTH_REQUIRED.
Legacy v0.3 strings: `submitted`, `working`, `completed`, `failed`, `canceled`, `rejected`, `input-required`, `auth-required` [W1 TaskStatus section; V03 §6.3].

### 3.3 Transition rules (what the spec does and does not say)

- Spec does NOT publish a full transition matrix `[UNVERIFIED]` (searched §4.1.3, §3.1, key-concepts not fetched). Implementers must derive from the rules below.
- Stated rules:
  - Agent MAY set `input-required` mid-processing; client continues by sending a new message with same `taskId` and `contextId` [S §3.x (input-required paragraph, line ~736 of spec markdown)].
  - In-task auth: agent MUST transition to `TASK_STATE_AUTH_REQUIRED` and MUST include a TaskStatus message explaining it (unless negotiated out-of-band) [S §7.6.1].
  - AUTH_REQUIRED transition alone is not authorization for any operation [S §7.6.4].
  - Messages to a task in a terminal state => `UnsupportedOperationError` [S §3.1.1].
  - Messages to unknown `taskId` => `TaskNotFoundError` [S §3.1.1].
  - SubscribeToTask on terminal task => `UnsupportedOperationError` [S §5.3 table].
  - CancelTask: duplicate cancel may return `TaskNotFoundError` after purge [S §3.3.1].
- Immutability of terminal tasks: `[UNVERIFIED]` as an explicit rule; inferred from the above.

### 3.4 Blocking vs immediate

- `SendMessageConfiguration.returnImmediately` (bool, default false). With false, blocking Send returns when task reaches terminal OR interrupted state [S §3.2.2 / §3.1.1]. `[S §3.1.1 cites SendMessage; exact wording: "waits until terminal or interrupted unless returnImmediately"]`.

### 3.5 contextId / taskId rules

- `contextId`: server-generated for new tasks if not supplied; groups tasks + messages [S §3.4, P].
- `taskId`: server-generated for new tasks; client supplies to continue an existing task [S §3.4].
- Client continuation: same `taskId` AND `contextId` [S §3.4].
- `Message.referenceTaskIds`: references other tasks (context only) [P].
- `messageId`: client-generated unique id; used for idempotency (see §3.6).

### 3.6 SendMessageConfiguration, idempotency, history

| Field | Type / meaning | Source |
|---|---|---|
| `acceptedOutputModes` | repeated string | [P] |
| `taskPushNotificationConfig` | TaskPushNotificationConfig, `taskId` empty | [P], [S §3.2.2] |
| `historyLength` | optional int32. unset = no limit; 0 = no history (spec: SHOULD omit); >0 = at most N | [S §3.2.4] (conflicts with proto, see §10) |
| `returnImmediately` | bool, default false | [P] |

- Idempotency: Get* operations idempotent; SendMessage MAY be idempotent via `messageId` `[S §3.3.1]`; CancelTask idempotent.
- `GetTask` supports `historyLength`; `ListTasks` supports `pageSize` (default 50, max 100), `pageToken`, `contextId`, `status`, `statusTimestampAfter`, `includeArtifacts` (default false), `historyLength`, `tenant` [S §3.1.4].
- ListTasks ordering: last update descending; `nextPageToken` MUST be present, `""` on last page; `totalSize` field exists [S §3.1.4][P].

### 3.7 Message vs Task result

- Send may return `Task` or `Message` (`SendMessageResponse` oneof `task | message`) [P]. A simple, stateless reply may be a bare Message [S §3.7].

---

## 4. Streaming and updates

### 4.1 Event envelope

```
message StreamResponse {        // oneof exactly one
  Task task = 1;
  Message message = 2;
  TaskStatusUpdateEvent status_update = 3;     // JSON: statusUpdate
  TaskArtifactUpdateEvent artifact_update = 4; // JSON: artifactUpdate
}
```
[P]; exactly-one rule [S §10.7]. JSON discrimination by member name, no `kind` [W1, §A.2.1].

- `TaskStatusUpdateEvent`: `taskId`, `contextId`, `status` [P]. `final` boolean REMOVED in v1.0; stream closure is binding-specific [W1 SendStreamingMessage section].
- `TaskArtifactUpdateEvent`: `taskId`, `contextId`, `artifact`, `append` (bool), `lastChunk` (bool) [P].
  - `append`: append parts to a previously sent artifact with the same `artifactId` [P comment].
  - `lastChunk`: final chunk of that artifact [P comment].

### 4.2 Ordering and lifecycle

- Stream MUST start with `Task` (or a single `Message` for message-only result) [S §3.5, §3.1.2].
- Events MUST be delivered in order [S §3.5].
- Multiple concurrent streams allowed; all receive the same ordered events [W1].
- Send stream closes at terminal OR interrupted state [S §11.7]. SubscribeToTask closes on terminal `[W1 SubscribeToTask section]`.
- SubscribeToTask MUST first emit a current `Task` snapshot [S §3.1.6].
- SubscribeToTask on terminal => `UnsupportedOperationError` [S §5.3].

### 4.3 SSE (JSON-RPC / HTTP+JSON)

- Response `200`, `Content-Type: text/event-stream`, each `data:` is a full JSON-RPC response whose `result` is a `StreamResponse` [S §3.3.1, §9.x]. Caveat: spec's §3.3.1 text says `SendStreamingMessageResponse`; that is the v0.3 name [V03 §7.2.1].
- REST SSE ordering rule [S §11.7].

---

## 5. Push notifications

### 5.1 Objects

```
message TaskPushNotificationConfig {
  string tenant = 1;   string id = 2;   string task_id = 3;  // taskId
  string url = 4;      string token = 5;  AuthenticationInfo authentication = 6;
}
message AuthenticationInfo { string scheme = 1; string credentials = 2; }
```
[P]; field table [S §3.1.7]. `url` REQUIRED. `token` = unique per task/session. `authentication.scheme` aligned to OpenAPI names (e.g., "Bearer") [S §4.3].

### 5.2 Operations

- `CreateTaskPushNotificationConfig`: errors `PushNotificationNotSupportedError`, `TaskNotFoundError`. MUST establish webhook; config MUST persist until task completion or explicit deletion [S §3.1.7].
- Get/List/Delete similar; List returns `configs[]` + `nextPageToken` [P].
- Capability: push ops without `capabilities.pushNotifications` => `PushNotificationNotSupportedError` [S §3.3.4].

### 5.3 Payload

- Agent POSTs to `url` with body = `StreamResponse` JSON [S §4.3.3].
- Headers: `Content-Type: application/a2a+json` [S §4.3.3] (REST content type rule [S §11.4]).
- Auth per `authentication` (scheme + credentials) [S §4.3.3].
- Delivery is at-least-once; receiver MUST respond 2xx; receiver SHOULD be idempotent [S §13.2].
- Timeout recommended 10–30 s; retries with exponential backoff SHOULD; SHOULD reject private/loopback/link-local targets (SSRF) [S §13.2].
- Push payloads use HTTP+JSON format regardless of agent's bus binding [S §4.3.3 — "use HTTP JSON binding", paraphrase; check wording before relying on it].

### 5.4 Security

- Webhook URLs SHOULD be HTTPS; tokens SHOULD be secrets, rotated; clients SHOULD use unique tokens per config [S §13.2].
- Client MUST validate webhook authenticity with the provided credentials; SHOULD check `taskId` belongs to a task it created [S §13.2].

---

## 6. Error codes

### 6.1 A2A errors (spec §5.4 / §3.3.2 — values from [S §5.4])

| Name | JSON-RPC code | gRPC/Status code | HTTP | ErrorInfo.reason | Typical trigger |
|---|---|---|---|---|---|
| TaskNotFoundError | -32001 | NOT_FOUND | 404 | TASK_NOT_FOUND | unknown/invisible task id |
| TaskNotCancelableError | -32002 | FAILED_PRECONDITION | 400 | TASK_NOT_CANCELABLE | cancel on terminal task |
| PushNotificationNotSupportedError | -32003 | FAILED_PRECONDITION | 400 | PUSH_NOTIFICATION_NOT_SUPPORTED | push op without capability |
| UnsupportedOperationError | -32004 | FAILED_PRECONDITION | 400 | UNSUPPORTED_OPERATION | op not supported / message to terminal task / subscribe to terminal |
| ContentTypeNotSupportedError | -32005 | INVALID_ARGUMENT | 400 | CONTENT_TYPE_NOT_SUPPORTED | unsupported media type |
| InvalidAgentResponseError | -32006 | INTERNAL | 500 | INVALID_AGENT_RESPONSE | agent output invalid |
| ExtendedAgentCardNotConfiguredError | -32007 | FAILED_PRECONDITION | 400 | EXTENDED_AGENT_CARD_NOT_CONFIGURED | capability declared, card not set |
| ExtensionSupportRequiredError | -32008 | FAILED_PRECONDITION | 400 | EXTENSION_SUPPORT_REQUIRED | required extension not requested |
| VersionNotSupportedError | -32009 | FAILED_PRECONDITION | 400 | VERSION_NOT_SUPPORTED | unsupported A2A-Version |

Source: [S §5.4] (codes, gRPC, HTTP) and [S §3.3.2] (error table); reason strings derived by rule [S §10.6]: reason = error name minus "Error", UPPER_SNAKE; `ErrorInfo.domain` = `a2a-protocol.org`. Check that the reason strings in §3.3.2 match; they are derived `[UNVERIFIED]` for exact spelling per error.

### 6.2 Standard JSON-RPC errors

| Name | Code |
|---|---|
| JSONParseError | -32700 |
| InvalidRequestError | -32600 |
| MethodNotFoundError | -32601 |
| InvalidParamsError | -32602 |
| InternalError | -32603 |
| (server-defined range) | -32000..-32099 reserved for A2A [S §9.x] |

Source: [S §9.x error codes table]; JSON-RPC 2.0 spec https://www.jsonrpc.org/specification.

### 6.3 Error bodies

- JSON-RPC: `error.code`, `error.message`, optional `error.data` = array of objects each with `@type` [S §9.x].
- HTTP+JSON: google.rpc.Status: `{ "error": { "code", "message", "status", "details": [ {"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason", "domain", "metadata"} ] } }`; ErrorInfo REQUIRED [S §11.6]. `[INFERENCE]` on exact envelope key names; verify in §11.6 raw text.
- gRPC: `google.rpc.Status` + `ErrorInfo` in details [S §10.6].

### 6.4 Auth errors
- HTTP auth challenge: 401/403, `WWW-Authenticate` SHOULD be included on 401 [S §4.4 (v0.3 numbering) / latest §4.4].
- Agents SHOULD use HTTP 401/403 for auth, not A2A errors `[UNVERIFIED]` for a dedicated code.

---

## 7. Wire mapping

### 7.1 Operation table (spec §5.3, binding-independent names)

| Operation | JSON-RPC method | HTTP+JSON | gRPC |
|---|---|---|---|
| SendMessage | `SendMessage` | `POST /message:send` | `SendMessage` |
| SendStreamingMessage | `SendStreamingMessage` | `POST /message:stream` (SSE) | `SendStreamingMessage` (stream) |
| GetTask | `GetTask` | `GET /tasks/{id}` | `GetTask` |
| ListTasks | `ListTasks` | `GET /tasks` | `ListTasks` |
| CancelTask | `CancelTask` | `POST /tasks/{id}:cancel` | `CancelTask` |
| SubscribeToTask | `SubscribeToTask` | **see discrepancy** `POST /tasks/{id}:subscribe` (spec table) vs `GET /tasks/{id=*}:subscribe` (proto) | `SubscribeToTask` (stream) |
| CreateTaskPushNotificationConfig | `CreateTaskPushNotificationConfig` | `POST /tasks/{id}/pushNotificationConfigs` | same |
| GetTaskPushNotificationConfig | same | `GET /tasks/{id}/pushNotificationConfigs/{configId}` | same |
| ListTaskPushNotificationConfigs | same | `GET /tasks/{id}/pushNotificationConfigs` | same |
| DeleteTaskPushNotificationConfig | same | `DELETE /tasks/{id}/pushNotificationConfigs/{configId}` | same |
| GetExtendedAgentCard | `GetExtendedAgentCard` | `GET /extendedAgentCard` | `GetExtendedAgentCard` |

Sources: [S §5.3], [S §11.3] (REST paths), [P] (proto http annotations incl. `additional_bindings` with `/{tenant}/...`). Discrepancy: [S §11.3.2] lists `POST /tasks/{id}:subscribe`; proto `[P]` says GET.

### 7.2 JSON-RPC 2.0 wire

- Method names: PascalCase, same as operation names, e.g. `"method": "SendMessage"` [S §9.1, §9.x examples].
- `params` = the request object (e.g., `SendMessageRequest`, `GetTaskRequest` with `id`, `historyLength`) [S §9.x examples].
- `GetExtendedAgentCard` has no params [S §9.x].
- Content-Type `application/json`; streaming via SSE `text/event-stream` [S §9.1].
- Service params as HTTP headers for JSON-RPC: `A2A-Version`, `A2A-Extensions` [S §9.2].
- v0.3 names (pre-1.0) `[V03 §3.5.6]`:

| v0.3 JSON-RPC | v1.0 |
|---|---|
| `message/send` | `SendMessage` |
| `message/stream` | `SendStreamingMessage` |
| `tasks/get` | `GetTask` |
| `tasks/list` | `ListTasks` |
| `tasks/cancel` | `CancelTask` |
| `tasks/resubscribe` | `SubscribeToTask` |
| `tasks/pushNotificationConfig/set` | `CreateTaskPushNotificationConfig` |
| `tasks/pushNotificationConfig/get` | `GetTaskPushNotificationConfig` |
| `tasks/pushNotificationConfig/list` | `ListTaskPushNotificationConfigs` |
| `tasks/pushNotificationConfig/delete` | `DeleteTaskPushNotificationConfig` |
| `agent/getAuthenticatedExtendedCard` | `GetExtendedAgentCard` |

Source: [W1 Behavioral Changes], [V03 §3.5.6]. Pre-1.0 aliasing: [S Appendix A] says legacy names MUST remain resolvable until removal; removal `>= 0.5.0`. Spec §A.2 says servers MAY accept both forms during overlap; recommended emit only current form [S A.2].
`[UNVERIFIED]` whether any v1.0 server in the wild accepts `message/send`; this is only a spec option.

### 7.3 Enums, casing, fields

- JSON field names: camelCase [S §5.5] (`protocolVersion`, `contextId`, `defaultInputModes`, `pushNotificationConfig`).
- Enums: ProtoJSON strings, SCREAMING_SNAKE_CASE with prefix: `TASK_STATE_*`, `ROLE_USER`, `ROLE_AGENT` [S §5.5, W1].
- Timestamps: ISO 8601 UTC, ms precision `YYYY-MM-DDTHH:mm:ss.sssZ` [W1, S §5.x].
- Field presence: REQUIRED always present; optional scalars set only when explicitly set; default values omitted otherwise [S §5.x / §8.4 canonicalization].
- Request/response messages are named `*Request` / `*Response` (v0.3 `*Params` / `*SuccessResponse` are legacy) [S Appendix A].
- v0.3 `kind` discriminator removed [W1, S A.2.1].

### 7.4 gRPC

- Proto package `lf.a2a.v1` [P]. Service `A2AService` [V03 §3.2.2; P].
- Metadata for service params uses `a2a-` prefix [S §10.2].
- Errors: `google.rpc.Status` details [S §10.6].

---

## 8. Agent card, discovery, extended card, signatures

### 8.1 Location and retrieval

- Well-known: `https://<host>/.well-known/agent-card.json` [S §8.2, §14.3].
- Card MUST be served; Cache-Control and ETag SHOULD; conditional requests supported [S §8.6].
- Extended card: `GetExtendedAgentCard` (`GET /extendedAgentCard` over HTTP) [S §3.1.11, §5.3]. Requires `capabilities.extendedAgentCard = true`; if declared but not configured: `ExtendedAgentCardNotConfiguredError`; if not declared: `UnsupportedOperationError` [S §3.3.4].
- Clients SHOULD use `capabilities.extendedAgentCard`, not the v0.3 top-level `supportsAuthenticatedExtendedCard` [W1; S Appendix A.2].

### 8.2 AgentCard (proto, fields 1–14 per [P])

Key fields: `name`, `description`, `supportedInterfaces[]` (AgentInterface), `provider` (AgentProvider), `version`, `documentationUrl`, `capabilities` (AgentCapabilities: `streaming`, `pushNotifications`, `extensions[]`, `extendedAgentCard`), `securitySchemes` (map), `securityRequirements[]`, `defaultInputModes`, `defaultOutputModes`, `skills[]` (AgentSkill), `signatures[]` (AgentCardSignature). Exact field numbers: see [P]. `protocolVersion` / `url` / `preferredTransport` / `additionalInterfaces` / `supportsAuthenticatedExtendedCard` are REMOVED from the card [W1 AgentCard section].

### 8.3 Signing (JWS + JCS)

- Canonicalization: RFC 8785 JCS over the card with `signatures` field omitted [S §8.4].
- Protected header: `alg` MUST; `kid` MUST; `typ` SHOULD be `JOSE`; `jku` MAY [S §8.4].
- Signing input: `BASE64URL(protected) || '.' || BASE64URL(payload)` (RFC 7515) [S §8.4].
- Client MUST verify at least one signature; expired/revoked keys MUST NOT be used [S §8.4].
- `AgentCardSignature` proto: `protected` (string), `signature` (string), `header` (Struct) [P].

### 8.4 Discovery in client

- Choose interface: preferred = first in `supportedInterfaces` [S §8.3.1]. Client picks any declared interface [S §8.3.2].

---

## 9. Versioning, service parameters, security

- `A2A-Version`: header (HTTP), gRPC metadata, JSON-RPC param/header; value `Major.Minor` (e.g. `1.0`); empty = `0.3` for compatibility; unsupported => `VersionNotSupportedError` [S §3.6, §14.2.1].
- REST also accepts `?A2A-Version=1.0` query parameter [S §11.x example].
- `A2A-Extensions`: header with comma-separated URIs [S §14.2.2].
- Service params are abstract; each binding maps them [S §3.2.6].
- TLS: production MUST use HTTPS; TLS 1.3+ recommended; deprecated TLS MUST NOT be used (SHOULD disable) [S §4.1, §7.x]. The bus binding must define its own transport security `[INFERENCE]`; spec provides no rule for NATS.
- Auth: credentials out-of-band; HTTP headers per request; agent MUST authenticate each request [S §4.3, §4.4].
- AUTH_REQUIRED: agent MUST transition to `TASK_STATE_AUTH_REQUIRED`, MUST include status message, MUST obtain credentials out-of-band; client SHOULD subscribe/poll; state transition alone authorizes nothing [S §7.6.1–§7.6.4].

---

## 10. Discrepancies and gaps (verify before implementing)

| # | Issue | Sources |
|---|---|---|
| D-1 | SubscribeToTask HTTP verb: proto `GET /tasks/{id=*}:subscribe` vs spec table `POST /tasks/{id}:subscribe`. | [P] vs [S §5.3, §11.3.2] |
| D-2 | `historyLength` = 0: spec says "SHOULD omit" history; proto comment says "MUST NOT return more than N". Both fine for N=0 but differ in wording. | [S §3.2.4], [P] |
| D-3 | Spec §12.4 references "Section 3.2.2 (Error Handling)"; error section is §3.3.2. §3.2.2 is SendMessageConfiguration. Spec typo. | [S §12.4, §3.3.2] |
| D-4 | Examples in spec use `protocolVersion: "0.3"` and `A2A-Version: 0.3` while title is v1.0. Examples not normalized. Use `1.0` for v1.0. | [S §4.6.1, §14.2.1] |
| D-5 | §9.1 says PascalCase JSON-RPC method names; v0.3 spec used `category/action`. v1.0 uses PascalCase. | [S §9.1] vs [V03 §3.5.1] |
| D-6 | No explicit task state transition matrix in spec `[UNVERIFIED]` absence. | [S] (searched) |
| D-7 | Task immutability of terminal tasks not stated explicitly `[UNVERIFIED]`. | [S] |
| D-8 | The spec text on `kind` in streaming section (§3.3.1) still references `SendStreamingMessageResponse` (v0.3 name). | [S §3.3.1] |
| D-9 | `TaskStatusUpdateEvent.final` removed in v1.0 but binding-specific closure is not defined for custom bindings; bus binding must define end-of-stream. | [W1], [S §12] |

---

## 11. Checklist for the NATS/JetStream bus binding and HTTP gateway

1. Advertise one `AgentInterface` per binding; `protocolBinding` = your URI; `protocolVersion` = `1.0`.
2. Map each §7.1 operation to a bus subject `[INFERENCE]`; request/reply for unary, stream subject for SSE-equivalent.
3. Carry `A2A-Version` and `A2A-Extensions` as bus message headers; reject unsupported version with `VersionNotSupportedError` (-32009 / reason VERSION_NOT_SUPPORTED).
4. Use `StreamResponse` JSON (camelCase, member-discriminated) as stream payloads; first event must be `Task` or `Message`.
5. Preserve per-task ordering (per-task subject or sequence numbers) `[INFERENCE]`; spec requires ordered delivery (§3.5).
6. Error envelope: google.rpc.Status with ErrorInfo (domain `a2a-protocol.org`) `[INFERENCE]` for bus.
7. HTTP gateway: map §7.1 HTTP paths; push payloads use HTTP+JSON per §4.3.3; push must POST `application/a2a+json`.
8. Implement SSRF filters from §13.2 on push URLs.
9. Serve `/.well-known/agent-card.json` with ETag/Cache-Control; sign card with JWS/JCS per §8.3 when signing.
10. Accept pre-1.0 aliases (§7.2 table) only if you choose the overlap option in Appendix A; otherwise reject with MethodNotFoundError (-32601).
