//! A2A v1.0 HTTP+JSON-RPC driver: a `Backend` for A2A peers such as Hermes (RFC-0009, RFC-0008 4.9 and 4.11).
#![forbid(unsafe_code)]

pub mod backend;
pub mod config;

mod card;
mod client;
mod namespace;
mod redact;
mod rpc;
mod sse;

pub use backend::{ConnectError, HttpBackend, bus_card};
pub use config::{ConfigError, PeerConfig, PeerSettings, Secret};
