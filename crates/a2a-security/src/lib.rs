//! Identity, signing, trust and capability checks for A2A-Bus (RFC-0007).
//!
//! Layer L2 (RFC-0001 §4.2). Depends only on `a2a-types`. Wire refusals are
//! [`BusError`]s whose [`BusError::to_status`] is the `google.rpc.Status` of [0007-140].
//! Local failures are [`SecurityError`]s and are never sent to peers as-is.

#![forbid(unsafe_code)]

mod canon;
pub mod card;
pub mod clock;
pub mod decision;
pub mod delegation;
pub mod envelope;
pub mod error;
pub mod frame;
pub mod identity;
pub mod mode;
pub mod provenance;
pub mod redact;
pub mod replay;
pub mod storage;
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod testkit;
pub mod trust;
