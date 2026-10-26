//! The transport-neutral bus seam for A2A-Bus (RFC-0001 [0001-006], RFC-0011).
//!
//! This crate defines the [`bus::Bus`] trait, its value types, the error
//! taxonomy, capability declarations with the `a2a-bus-m1` profile check, and
//! the simulation control trait. It contains no transport code.

#![forbid(unsafe_code)]

pub mod bus;
pub mod capabilities;
pub mod control;
pub mod error;
pub mod headers;
pub mod helpers;
pub mod message;
pub mod name;
pub mod value;
