//! `a2ad`: the A2A-Bus supervisor (RFC-0012).
//!
//! Module ownership: `config`, `supervisor`, `platform`, `creds`, `broker`, `cli`, `host`
//! and the crate root belong to SupervisorCore. `worker_host`, `gateway_host` and `health`
//! belong to SupervisorHosts.

#![deny(unsafe_code)]

pub mod broker;
pub mod cli;
pub mod config;
pub mod creds;
pub mod gateway_host;
pub mod health;
pub mod host;
mod hosts;
#[allow(unsafe_code)]
pub mod platform;
pub mod supervisor;
pub mod worker_host;
