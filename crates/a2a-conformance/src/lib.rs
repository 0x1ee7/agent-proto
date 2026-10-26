//! Conformance suite for A2A-Bus bindings (RFC-0011).
//!
//! The suite is generic over a [`bus_contract::BusFactory`]. It runs fully
//! in-process against any binding that exposes a virtual clock through
//! [`bus_api::control::BusControl`]; no network access is performed.

#![forbid(unsafe_code)]

pub mod bus_contract;
pub mod driver;
pub mod fixtures;
pub mod report;
