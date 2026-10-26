//! Crate-level error for building, sealing and persisting audit events.

use crate::canon::CanonError;
use crate::correlation::CorrelationError;
use crate::schema::SchemaError;
use crate::sink::SinkError;
use crate::spool::SpoolError;

/// Errors from building, sealing and persisting audit events.
#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    #[error(transparent)]
    Schema(#[from] SchemaError),
    #[error(transparent)]
    Correlation(#[from] CorrelationError),
    #[error(transparent)]
    Canon(#[from] CanonError),
    #[error(transparent)]
    Spool(#[from] SpoolError),
    #[error(transparent)]
    Sink(#[from] SinkError),
    #[error("invalid emitter identity: {0}")]
    Identity(String),
    #[error("event type {0} is not in RFC-0013 table 4.2")]
    UnknownType(i32),
    #[error("emitter kind {kind} may not emit {event}")]
    KindNotAllowed { kind: String, event: String },
    #[error("encoding the event payload failed: {0}")]
    Encoding(#[from] serde_json::Error),
    #[error("resume point is invalid: {0}")]
    Resume(String),
    #[error("checkpoints need a configured checkpoint signer")]
    NoSigner,
    /// The spool is at its cap and refused a record that may not discard older events.
    #[error("audit spool is full; event not accepted")]
    SpoolFull,
}
