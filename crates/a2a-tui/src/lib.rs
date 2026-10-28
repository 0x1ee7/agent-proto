//! Local operator console for A2A-Bus (RFC-0012 sec 4.10).
//!
//! The library is generic over `bus_api::bus::Bus`. Only the binary names a concrete binding
//! (RFC-0001 0001-001).

pub mod app;
pub mod audit_feed;
pub mod driver;
pub mod follow;
pub mod model;
pub mod operator;
pub mod redact;
pub mod render;
pub mod ui;
