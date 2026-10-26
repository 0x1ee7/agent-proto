//! A2A-Bus wire and domain types (RFC-0001 L0, RFC-0002, RFC-0003, RFC-0005, RFC-0006).
//!
//! `proto` holds generated protobuf types. The other modules add validated identifiers, the
//! vocabulary constants, the task lifecycle rules, the failure reason carrier, and the
//! extension helpers.

#![forbid(unsafe_code)]

pub mod extension;
pub mod failure;
pub mod ids;
pub mod lifecycle;
pub mod proto;
pub mod vocab;
