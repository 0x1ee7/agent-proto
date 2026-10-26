//! Deterministic in-process binding of the A2A-Bus seam (`bus-api`).
//!
//! Time is virtual: it moves only through `BusControl::advance`. Faults draw
//! from a generator seeded by [`MemConfig::seed`], so the same seed and the same
//! call sequence reproduce the same fault trace.

#![forbid(unsafe_code)]

mod bus;
mod config;
mod control;
mod rng;
mod state;

pub use bus::{MemBus, MemConsumer, MemRequests, MemWatch};
pub use config::{ConfigError, Grant, MemConfig, StreamConfig, m1_capabilities};
pub use control::MemControl;
