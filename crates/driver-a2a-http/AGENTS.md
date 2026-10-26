# driver-a2a-http

Layer L4 driver (RFC-0001 section 4.2, RFC-0009 4.6, 4.10 to 4.12). An A2A v1.0 HTTP+JSON-RPC client implementing `a2a_worker::backend::Backend` for peers such as Hermes (RFC-0008 4.9 and 4.11).

## Owns

- `config`: peer configuration, validation, bearer secret handling, base URL and loopback policy.
- `card`: peer card fetch, selection of the RPC URL and streaming flag, and the bus-facing sanitized card.
- `client`: HTTP transport, A2A-Version header, bearer attachment, unary calls, streaming reads with idle timeout.
- `rpc`: JSON-RPC envelopes, A2A task parsing, status and code to failure-class mapping.
- `sse`: server-sent event decoder.
- `namespace`: peer contextId, messageId, and requestId derivation.
- `redact`: removal of the bearer secret and bearer text from anything that leaves the driver.
- `backend`: `HttpBackend`, the `Backend` implementation (turn driving, polling, cancel, shutdown).

## Provides

- `backend::{HttpBackend, ConnectError, bus_card}`.
- `config::{PeerConfig, PeerSettings, Secret, ConfigError}`.

## Uses

- `a2a-types` (AgentId, vocabulary constants).
- `a2a-worker` `backend` module (the seam). Dev only: `testkit` feature for `backend_contract`.
- Third-party: reqwest (rustls), tokio, serde_json, url, uuid, sha2, thiserror.

## Rules

- No bus, store, or other workspace crate dependency (0001-002).
- The bearer is sent only over https or loopback http (0008-069, 0009-052). It is never sent to a card-selected URL that fails that policy, and never printed, logged, or put in an event.
- Failure mapping uses HTTP status and JSON-RPC code only. `error.data` and ErrorInfo are never required.
- Every turn ends with exactly one terminal event. Reason tokens come only from the producing protocol (0009-072).
- Process and transport cleanup: no child processes. Turn tasks run on the backend's own runtime; `shutdown` bounds their wait by the grace period.
- No `unsafe`. No `unwrap`, `expect`, `panic`, `todo!`, or `unimplemented!` outside tests.
- ASCII only in source. No shell is spawned.

## Requirement IDs guarded

- 0008-069: bearer only over https or loopback http. Guarded in `config` and `client` tests.
- 0008-071: no 1.0 JSONRPC interface is a version mismatch; the turn proceeds. Guarded in `card` and integration tests.
- 0008-072: bus agent id comes from configuration. Guarded in `config` tests.
- 0008-078: error code classes; reply timeout and unreachable peer mapping. Guarded in `rpc` and integration tests.
- 0008-097: peer contextId namespaced per origin. Guarded in `namespace` tests.
- 0008-098: error without `error.data` accepted; non-object body malformed. Guarded in `rpc` and integration tests.
- 0008-099: sanitized bus card. Guarded in `card` tests.
- 0009-020: second Start while open is busy; cancel before a task id. Guarded in integration tests.
- 0009-040: A2A-Version header on requests. Guarded in the unary integration test.
- 0009-041: card selection of the JSONRPC 1.0 interface. Guarded in `card` tests.
- 0009-042: tenant sent in params when configured. Guarded in `client` tests.
- 0009-043: SendMessage and SendStreamingMessage paths; messageId shape. Guarded in integration tests.
- 0009-045: resend after connection failure reuses messageId; retries bounded. Guarded in `backend` tests.
- 0009-046: foreign contextId fails the turn; reply text and bare task parsing. Guarded in integration and `rpc` tests.
- 0009-047: state mapping, INPUT_REQUIRED, CANCELED after working. Guarded in `rpc` and integration tests.
- 0009-049: polling, and streaming reattach with SubscribeToTask. Guarded in integration tests.
- 0009-050: lost task maps to BACKEND_LOST_TASK. Guarded in integration tests.
- 0009-051: 401, 403, -32050 peer auth failure; 429 and -32051 refused. Guarded in integration tests.
- 0009-052: refuse bearer over cleartext remote. Guarded in `config` tests.
- 0009-053: default timeout 330 s; timeout cancels remote and fails. Guarded in `config` and integration tests.
- 0009-070: token and bearer text redacted in Debug and details. Guarded in `config` and `redact` tests.
- 0009-078: follow-up reuses the namespaced contextId. Guarded in `namespace` tests.
