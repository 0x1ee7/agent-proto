//! Mapping of async-nats errors onto `bus-api` errors. Reasons are stable
//! `&'static str` tokens; see the crate AGENTS.md.

use std::time::Duration;

use async_nats::jetstream::context::{PublishError as JsPublishError, PublishErrorKind as JsKind};
use async_nats::{PublishError, PublishErrorKind, RequestError, RequestErrorKind};
use bus_api::error::BusError;

use crate::layout::MAX_WIRE_BYTES;

/// Reason tokens carried by `BusError::Unavailable`.
pub(crate) mod reason {
    pub(crate) const JETSTREAM_API: &str = "JETSTREAM_API";
    pub(crate) const STREAM_NOT_FOUND: &str = "STREAM_NOT_FOUND";
    pub(crate) const CONSUMER_CLOSED: &str = "CONSUMER_CLOSED";
    pub(crate) const WATCH_CLOSED: &str = "WATCH_CLOSED";
    pub(crate) const SUBSCRIPTION_CLOSED: &str = "SUBSCRIPTION_CLOSED";
    pub(crate) const REQUEST_FAILED: &str = "REQUEST_FAILED";
    pub(crate) const PUBLISH_FAILED: &str = "PUBLISH_FAILED";
    pub(crate) const CONNECTION_LOST: &str = "CONNECTION_LOST";
    pub(crate) const RETENTION_EXCEEDED: &str = "RETENTION_EXCEEDED";
    pub(crate) const SEQ_NOT_YET_ASSIGNED: &str = "SEQ_NOT_YET_ASSIGNED";
    pub(crate) const SUBSCRIBE_FAILED: &str = "SUBSCRIBE_FAILED";
    pub(crate) const CLOSE_FAILED: &str = "CLOSE_FAILED";
    pub(crate) const INVALID_TTL: &str = "INVALID_TTL";
}

/// Back-off advertised when the server reports `MaxAckPending`.
pub(crate) const OVERLOAD_RETRY_AFTER: Duration = Duration::from_secs(1);

/// Maps a core-NATS publish error.
pub(crate) fn core_publish(err: &PublishError) -> BusError {
    match err.kind() {
        PublishErrorKind::MaxPayloadExceeded => BusError::PayloadTooLarge {
            limit: MAX_WIRE_BYTES,
        },
        _ => BusError::Unavailable {
            reason: reason::PUBLISH_FAILED,
        },
    }
}

/// Maps a core-NATS request error.
pub(crate) fn request(err: &RequestError) -> BusError {
    match err.kind() {
        RequestErrorKind::TimedOut => BusError::Timeout,
        RequestErrorKind::NoResponders => BusError::NoResponder,
        RequestErrorKind::MaxPayloadExceeded => BusError::PayloadTooLarge {
            limit: MAX_WIRE_BYTES,
        },
        RequestErrorKind::InvalidSubject | RequestErrorKind::Other => BusError::Unavailable {
            reason: reason::REQUEST_FAILED,
        },
    }
}

/// Maps a JetStream publish error that is not a conflict.
pub(crate) fn js_publish(err: &JsPublishError) -> BusError {
    match err.kind() {
        JsKind::StreamNotFound => BusError::Unavailable {
            reason: reason::STREAM_NOT_FOUND,
        },
        JsKind::TimedOut => BusError::Timeout,
        JsKind::MaxPayloadExceeded => BusError::PayloadTooLarge {
            limit: MAX_WIRE_BYTES,
        },
        JsKind::MaxAckPending => BusError::Overloaded {
            retry_after: OVERLOAD_RETRY_AFTER,
        },
        JsKind::BrokenPipe => BusError::Unavailable {
            reason: reason::CONNECTION_LOST,
        },
        JsKind::WrongLastMessageId | JsKind::WrongLastSequence | JsKind::Other => {
            BusError::Unavailable {
                reason: reason::PUBLISH_FAILED,
            }
        }
    }
}

/// `Unavailable` with the given reason.
pub(crate) fn unavailable(reason: &'static str) -> BusError {
    BusError::Unavailable { reason }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 0010-015: a request that exceeds max_payload is reported as PayloadTooLarge.
    #[test]
    fn req_0010_015_request_payload_error_maps_to_too_large() {
        let err = RequestError::new(RequestErrorKind::MaxPayloadExceeded);
        assert_eq!(
            request(&err),
            BusError::PayloadTooLarge {
                limit: MAX_WIRE_BYTES
            }
        );
    }

    // 0011-011: a request yields exactly one reply or a distinct error outcome.
    #[test]
    fn req_0011_011_request_outcomes_are_distinct() {
        assert_eq!(
            request(&RequestError::new(RequestErrorKind::NoResponders)),
            BusError::NoResponder
        );
        assert_eq!(
            request(&RequestError::new(RequestErrorKind::TimedOut)),
            BusError::Timeout
        );
        assert_eq!(
            request(&RequestError::new(RequestErrorKind::Other)),
            BusError::Unavailable {
                reason: reason::REQUEST_FAILED
            }
        );
    }

    #[test]
    fn js_max_ack_pending_maps_to_overloaded() {
        let err = JsPublishError::new(JsKind::MaxAckPending);
        assert_eq!(
            js_publish(&err),
            BusError::Overloaded {
                retry_after: OVERLOAD_RETRY_AFTER
            }
        );
    }

    #[test]
    fn js_stream_missing_maps_to_unavailable_not_found() {
        let err = JsPublishError::new(JsKind::StreamNotFound);
        assert_eq!(
            js_publish(&err),
            BusError::Unavailable {
                reason: reason::STREAM_NOT_FOUND
            }
        );
    }
}
