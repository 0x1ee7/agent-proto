# gateway-http (L4, GatewayImplAgent)

## Owns
- RFC-0008 HTTP gateway: listener and TLS, Origin and Host checks, bearer auth, rate limits, JSON-RPC method mapping, peer and trust policy, operator-agent exclusion, push URL checks.
- RFC-0003 HTTP card view, RFC-0002 operation mapping, RFC-0004 card serving.
- Excludes the outbound bridge; that lives in `driver-a2a-http`.

## Provides
- `HttpGateway<B: bus_api::bus::Bus>`, `GatewayConfig`, `PeerConfig`, `Profile`, `RatePolicy`, `TlsConfig`, `ConfigError`, `StartError`.
- `router`, `serve`, `ServeError`, `TlsError`.

## Uses
- `a2a-client` for worker calls (send, stream, subscribe, task and push operations).
- `a2a-core` for envelope, subject, and header handling; `a2a-types` for ids and protobuf types; `bus-api` for the generic bus trait.
- Dev only: `bus-mem`.

## Static rules
- Generic over `bus_api::bus::Bus`. Name no concrete bus in `src/`. Depend on no `driver-*` crate.
- `#![forbid(unsafe_code)]`. No `unwrap`, `expect`, `panic`, `todo`, `unimplemented`, or `dbg_macro` outside tests.
- The gateway writes its own provenance and only lowers budget; caller-supplied provenance is never forwarded.
- Admission order: Origin and Host, route and method and Content-Type, body size, auth, agent resolve, parse, version and extensions, trust gate, rate limit, dispatch.
- The Agent Card GET is public. RPC requires authentication. The extended card and push URLs are peer-only.
- Secrets, tokens, and key material never appear in logs, errors, cards, or bus payloads.
- The gateway is never exposed as an agent. Only exposed agents appear in cards or are routed.
- Listeners bind to loopback by default. Non-loopback listeners require TLS, peers, and trusted peers.

## Tests
- `tests/requirements.rs` names each guarded requirement as `req_0008_0NN_*` with a `// [0008-0NN]` comment. Covered IDs: 001 through 006, 009 through 032, 035, 038, 043 through 046, 049 through 052, 059, 060.
- `src/limits.rs` unit tests guard 0008-023 at the limiter level.
- `tests/common/mod.rs` holds the virtual clock, fixture, and HTTP call helpers. It adds no requirements of its own.
