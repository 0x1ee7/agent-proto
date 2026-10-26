//! Caller-side helpers for the append path (RFC-0011 [0011-051], [0011-053]).

use std::num::NonZeroU32;

use bytes::Bytes;

use crate::bus::Bus;
use crate::error::BusError;
use crate::headers::Headers;
use crate::message::PublishAck;
use crate::name::{MsgId, Principal, StreamName, Subject};

/// Maps a duplicate outcome to success (RFC-0011 [0011-051]).
///
/// A repeated append with the same message id is not a failure: the caller
/// asked for the message to be stored, and it is stored. The result names the
/// original stream and sequence.
///
/// # Errors
/// Passes every other error through unchanged.
pub fn fold_duplicate(
    stream: &StreamName,
    outcome: Result<PublishAck, BusError>,
) -> Result<PublishAck, BusError> {
    match outcome {
        Err(BusError::Duplicate { original_seq }) => Ok(PublishAck {
            stream: Some(stream.clone()),
            seq: Some(original_seq),
        }),
        other => other,
    }
}

/// Arguments of one append, grouped to keep call sites readable.
#[derive(Debug)]
pub struct Append<'a> {
    /// Target stream.
    pub stream: &'a StreamName,
    /// Subject of the message.
    pub subject: &'a Subject,
    /// Message headers.
    pub headers: &'a Headers,
    /// Message payload.
    pub payload: Bytes,
    /// Message id reused across every retry of this append.
    pub msg_id: &'a MsgId,
}

/// Appends with retries that reuse one message id (RFC-0011 [0011-053]).
///
/// `Timeout` and `Unavailable` are retried up to `attempts` times in total with
/// the same `msg_id`, so a retry after a lost acknowledgement cannot store the
/// message twice. A duplicate on any attempt counts as success via
/// [`fold_duplicate`].
///
/// # Errors
/// Returns the last error once attempts are exhausted, or any non-retryable
/// error immediately.
pub async fn append_stored<B: Bus>(
    bus: &B,
    who: &Principal,
    append: Append<'_>,
    attempts: NonZeroU32,
) -> Result<PublishAck, BusError> {
    let mut remaining = attempts.get();
    loop {
        let outcome = bus
            .append(
                who,
                append.stream,
                append.subject,
                append.headers,
                append.payload.clone(),
                append.msg_id,
            )
            .await;
        match outcome {
            Err(BusError::Timeout | BusError::Unavailable { .. }) if remaining > 1 => {
                remaining -= 1;
            }
            other => return fold_duplicate(append.stream, other),
        }
    }
}

#[cfg(test)]
#[allow(clippy::panic, reason = "test setup failures abort the test")]
mod tests {
    use super::*;
    use crate::value::StreamSeq;

    fn stream() -> Result<StreamName, crate::name::InvalidName> {
        StreamName::parse("SUITE")
    }

    // [0011-051] a duplicate append is reported as success naming the stored copy.
    #[test]
    fn req_0011_051_duplicate_folds_to_stored_ack() {
        let Ok(name) = stream() else {
            panic!("SUITE is a valid stream name");
        };
        let folded = fold_duplicate(
            &name,
            Err(BusError::Duplicate {
                original_seq: StreamSeq::new(7),
            }),
        );
        assert_eq!(
            folded,
            Ok(PublishAck {
                stream: Some(name.clone()),
                seq: Some(StreamSeq::new(7)),
            })
        );
    }

    // [0011-051] only duplicates fold; other failures stay failures.
    #[test]
    fn req_0011_051_non_duplicate_errors_are_not_folded() {
        let Ok(name) = stream() else {
            panic!("SUITE is a valid stream name");
        };
        assert_eq!(
            fold_duplicate(&name, Err(BusError::Timeout)),
            Err(BusError::Timeout)
        );
        assert_eq!(
            fold_duplicate(&name, Err(BusError::DuplicateInProgress)),
            Err(BusError::DuplicateInProgress)
        );
    }
}
