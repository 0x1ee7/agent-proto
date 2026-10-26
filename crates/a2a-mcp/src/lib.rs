//! Peer-call MCP shim and worker delegation IPC (RFC-0015, layer L2).
//!
//! The crate has no bus dependency and holds no bus or provider credential
//! (RFC-0015 [0015-002]). Modules:
//!
//! - [`ipc`]: IPC protocol types, framing, handshake messages and argument
//!   validation shared by the shim and the worker's delegation endpoint.
//! - [`tools`]: the four tool schemas, argument parsing and result shaping.
//! - [`mcp`]: JSON-RPC 2.0 / MCP frame parsing and response building.
//! - `server`, `client`, `endpoint`, `ipc_server` (feature `runtime`): the
//!   stdio MCP server, the shim's IPC client with the grant handshake, the
//!   named pipe / Unix socket transport, and the worker-side IPC endpoint server.
//! - `testkit` (feature `testkit`): an in-process fake worker endpoint.
//!
//! The `ipc`, `tools` and `mcp` modules are runtime-agnostic.

#![forbid(unsafe_code)]

pub mod ipc;
pub mod mcp;
pub mod redact;
pub mod tools;

#[cfg(feature = "runtime")]
pub mod client;
#[cfg(feature = "runtime")]
pub mod endpoint;
#[cfg(feature = "runtime")]
pub mod ipc_server;
#[cfg(feature = "runtime")]
pub mod server;

#[cfg(feature = "testkit")]
pub mod testkit;

/// Name of the MCP server advertised in `initialize` (RFC-0015 [0015-020]).
pub const SERVER_NAME: &str = "a2abus";
/// MCP protocol revision pinned by RFC-0015 [0015-010].
pub const PROTOCOL_VERSION: &str = "2025-06-18";
/// Environment variable carrying the grant credential (RFC-0015 [0015-084]).
pub const GRANT_ENV: &str = "A2ABUS_MCP_GRANT";
