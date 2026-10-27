//! A2A-Bus worker (RFC-0001 section 4.2, layer L3).
//!
//! The worker serves the RPC subjects of one agent, runs tasks through a
//! [`Backend`](backend::Backend), and publishes events and audit records. It
//! depends on `bus-api` only; a concrete bus binding is injected by the
//! executable (RFC-0001 rule 0001-001).
//!
//! Modules:
//!
//! - [`backend`]: the `Backend` seam (RFC-0009). Stable public API.
//! - `mock` and `backend_contract`: scriptable mock and contract checks, behind
//!   the `testkit` feature (also compiled for this crate's tests).

#![forbid(unsafe_code)]

pub mod audit;
pub mod backend;
pub mod config;
mod delegation;
pub mod error;
pub mod presence;
pub mod rpc;
pub mod sanitize;
mod signing;
pub mod store;
pub mod wire;
pub mod worker;
pub mod workspace;
pub use delegation::DelegationConfig;

#[cfg(any(test, feature = "testkit"))]
pub mod backend_contract;

#[cfg(any(test, feature = "testkit"))]
pub mod mock;
#[cfg(test)]
mod tests;
