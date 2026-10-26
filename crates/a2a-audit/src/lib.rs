//! RFC-0013 audit: sealed events with a per-emitter hash chain, redaction, a crash-safe spool,
//! a rotating JSONL sink and the verification routine behind `a2s audit verify`.
//!
//! The crate has no bus, security or HTTP dependency. Callers supply the clock, the checkpoint
//! signer and the publish callback.
#![forbid(unsafe_code)]

pub mod canon;
pub mod chain;
pub mod clock;
pub mod coalesce;
pub mod correlation;
pub mod emitter;
pub mod error;
pub mod event;
pub mod health;
pub mod metrics;
pub mod pipeline;
pub mod redact;
pub mod schema;
pub mod sink;
pub mod spool;
pub mod testkit;
pub mod verify;

mod fsutil;

pub use emitter::{Emitter, EmitterIdentity};
pub use error::AuditError;
pub use event::EventBuilder;
pub use pipeline::{Pipeline, PipelineConfig, Publisher};
pub use sink::{AuditSink, JsonlSink, SinkOutcome};
pub use spool::Spool;
