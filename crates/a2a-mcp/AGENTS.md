# a2a-mcp

Layer L2 (RFC-0001). MCP stdio shim `a2abus-mcp-shim` and the IPC protocol shared with the worker's delegation endpoint (RFC-0015).

## Owns
- MCP stdio protocol for the four `a2a_*` tools (RFC-0015 section 4.1–4.5, 4.9): JSON-RPC framing, `initialize`, `tools/list`, `tools/call`, cancellation, result shaping.
- The IPC wire protocol: frame format, hello/auth handshake, request/response shapes, IPC reasons, and the shim-side client (`client`) and worker-side listener (`ipc_server`).
- The shim binary `a2abus-mcp-shim` (`src/main.rs`): argv and environment handling only.

## Provides
- Library modules: `ipc`, `mcp`, `tools`, `redact` (runtime-agnostic); `client`, `endpoint`, `server`, `ipc_server` (feature `runtime`); `testkit` (feature `testkit`).
- `redact::Redactor` trait; the library never depends on a concrete redactor.
- `ipc_server::GrantTable` trait and `ServerRequest` queue: the worker supplies grant authentication and answers requests.

## Uses
- `base64`, `serde`, `serde_json`, `thiserror`, `tokio` (optional, `runtime`).
- Nothing from `bus-*`, `a2a-security`, `a2a-client`, `a2a-types`. The worker consumes this crate, never the reverse.

## Rules
- The shim holds no bus or provider credential. The grant credential comes only from the `A2ABUS_MCP_GRANT` environment variable (RFC-0015 [0015-090], [0015-081]); it never appears on argv, in logs, or in results.
- stdout carries only MCP messages. Diagnostics go to stderr only.
- Every frame is at most 262144 bytes; a zero length is refused ([0015-040]). Every MCP line is at most 1 MiB; overflow returns JSON-RPC `-32600` with a null id.
- `deadlineMs` and `timeoutMs` are 1..=3600000 ([0015-044], [0015-021]).
- Tool failures are `isError: true` inside a successful JSON-RPC result. Protocol errors use JSON-RPC error objects only for parse, invalid request, unknown method, and invalid params.
- Result text is capped at 65536 bytes on a UTF-8 boundary before forwarding ([0015-032]).
- Idempotency key is `<sanitized MCP id>:<session slot>` ([0015-047], [0015-048]).
- IPC error messages are fixed short English sentences ([0015-045]); no paths, stack traces, or credentials.
- Pending calls survive reconnect and are resent with their original ids; fatal handshake reasons (`IPC_VERSION_UNSUPPORTED`, `UNAUTHENTICATED`, `GRANT_REVOKED`) are not retried.
- Local UNAVAILABLE and DEADLINE_EXCEEDED are produced only when the connection is lost or the local timer fires without a worker reply.
- No `unsafe`. No `unwrap`/`expect`/`panic` outside tests. Test modules carry `#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]`.
- Windows and Linux parity: endpoint transport is named pipe on Windows and Unix socket (mode 0600) elsewhere.

## Requirement IDs guarded
RFC-0015 [0015-010], [0015-013], [0015-014], [0015-016], [0015-020], [0015-021], [0015-022], [0015-032], [0015-040], [0015-041], [0015-042], [0015-043], [0015-044], [0015-045], [0015-046], [0015-047], [0015-048] (shim side). Tests name these IDs.
