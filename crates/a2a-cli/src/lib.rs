//! Operator command-line surface of the A2A-Bus host (RFC-0012 section 4.11).
//!
//! Everything except the binary is generic over [`bus_api::Bus`]. Only the binary names the
//! concrete NATS binding. Exit codes follow [0012-055]; `--json` prints proto-JSON of existing
//! A2A messages only [0012-056].

#![forbid(unsafe_code)]

pub mod app;
pub mod approvals;
pub mod audit;
pub mod card;
pub mod cli;
pub mod commands;
pub mod config;
pub mod error;
pub mod output;

pub use error::{CliError, Exit};
