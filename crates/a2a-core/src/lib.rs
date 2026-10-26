//! A2A-Bus protocol binding (RFC-0002): subjects, headers, envelopes and errors.
//!
//! Layer L2 (RFC-0001). Depends only on `a2a-types` and `bus-api`; performs no I/O
//! except through the `bus-api` seam.

#![forbid(unsafe_code)]

pub mod envelope;
pub mod errors;
pub mod headers;
pub mod subject;
pub mod approval;
pub mod budget;
pub mod task;
