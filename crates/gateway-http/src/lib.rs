//! HTTP gateway (RFC-0008): exposes bus agents over JSON-RPC, SSE, and HTTP cards.
//!
//! The library is generic over `bus_api::bus::Bus`; only binaries name a concrete bus.
#![forbid(unsafe_code)]

mod auth;
mod cards;
mod config;
mod error;
mod gateway;
mod ingress;
mod jsonrpc;
mod limits;
mod ops;
mod server;
mod tls;

pub use config::{ConfigError, GatewayConfig, PeerConfig, Profile, RatePolicy, TlsConfig};
pub use gateway::{HttpGateway, StartError};
pub use server::{ServeError, router, serve};
pub use tls::TlsError;
