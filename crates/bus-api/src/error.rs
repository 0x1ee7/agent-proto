//! The bus error taxonomy (RFC-0011 section 4.6).
//!
//! Errors carry only typed, non-secret fields (RFC-0011 [0011-043]). Reason
//! strings are fixed constants chosen by the binding, never caller input.

use std::time::Duration;

use thiserror::Error;

use crate::value::{Digest, Revision, StreamSeq};

/// Failure of a bus operation.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BusError {
    /// A request had no responder and the binding signals that explicitly.
    #[error("no responder for subject")]
    NoResponder,
    /// The operation did not complete before its deadline.
    #[error("operation timed out")]
    Timeout,
    /// The bus or a dependency cannot serve the request now; `reason` is a
    /// fixed upper-case token such as `RESTARTED` or `RETENTION_EXCEEDED`.
    #[error("unavailable: {reason}")]
    Unavailable { reason: &'static str },
    /// A configured limit was reached; the caller should retry after the delay.
    #[error("overloaded; retry after {retry_after:?}")]
    Overloaded { retry_after: Duration },
    /// The principal may not perform this operation. Returned identically for
    /// absent and forbidden resources (RFC-0011 [0011-044]).
    #[error("permission denied")]
    PermissionDenied,
    /// The payload exceeds the declared limit in bytes.
    #[error("payload exceeds limit of {limit} bytes")]
    PayloadTooLarge { limit: usize },
    /// The message id was already stored; `original_seq` is the stored copy.
    #[error("duplicate of stream sequence {original_seq}")]
    Duplicate { original_seq: StreamSeq },
    /// A message with the same id is being stored right now.
    #[error("duplicate append in progress")]
    DuplicateInProgress,
    /// A compare-and-set precondition failed; nothing was overwritten.
    #[error("revision conflict; current revision {current_revision}")]
    Conflict { current_revision: Revision },
    /// The addressed entry does not exist.
    #[error("entry is absent")]
    Absent,
    /// Stored content does not match its digest; the bytes are withheld.
    #[error("stored content is corrupt for digest {digest}")]
    Corrupt { digest: Digest },
    /// The capability needed for this operation is not declared.
    #[error("capability `{capability}` not declared")]
    Unsupported { capability: &'static str },
    /// A binding-internal failure with no caller-actionable detail.
    #[error("internal binding error")]
    Internal,
}
