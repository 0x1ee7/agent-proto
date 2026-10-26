#![forbid(unsafe_code)]
//! ACP stdio driver for omp and claude-agent-acp (RFC-0009).
//!
//! The driver implements [`a2a_worker::backend::Backend`]. One actor thread
//! owns all state; the public [`AcpDriver`] handle sends commands to it.

mod acp;
mod config;
mod core;
mod driver;
mod frame;
mod mode;
mod process;
mod redact;

#[cfg(test)]
mod fake;
#[cfg(test)]
mod live;
#[cfg(test)]
mod tests;

pub use config::{
    AcpDriverConfig, ConfigError, DEFAULT_CANCEL_GRACE, DEFAULT_PERMISSION_TIMEOUT,
    DEFAULT_SHUTDOWN_GRACE, DEFAULT_TURN_IDLE_TIMEOUT, LaunchSpec, Profile,
};
pub use driver::AcpDriver;
