//! Messages, deliveries, KV entries and watch events.

use std::fmt;
use std::time::Duration;

use bytes::Bytes;

use crate::error::BusError;
use crate::headers::Headers;
use crate::name::{Key, StreamName, Subject};
use crate::value::{Revision, StreamSeq};

/// Result of a publish or append.
///
/// `stream` and `seq` are `Some` only when the message was stored in a durable
/// stream (publish on a captured subject, or append).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishAck {
    /// Stream that stored the message, if any.
    pub stream: Option<StreamName>,
    /// Sequence assigned by the stream, if stored.
    pub seq: Option<StreamSeq>,
}

/// A reply to a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    /// Reply headers.
    pub headers: Headers,
    /// Reply payload.
    pub payload: Bytes,
}

/// Binding-side sink that receives the single reply of a request.
pub trait ReplySink: Send {
    /// Delivers the reply to the waiting requester. Replies to requesters that
    /// already gave up are discarded.
    ///
    /// # Errors
    /// Returns [`BusError::Unavailable`] when the bus no longer serves replies.
    fn send(self: Box<Self>, reply: Reply) -> Result<(), BusError>;
}

/// The one-shot right to reply to a request. Replying consumes the handle, so
/// a second reply does not type-check (RFC-0011 [0011-012]).
pub struct ReplyHandle {
    sink: Box<dyn ReplySink>,
}

impl ReplyHandle {
    /// Wraps a binding sink. Used by bindings to create requests.
    #[must_use]
    pub fn new(sink: Box<dyn ReplySink>) -> Self {
        Self { sink }
    }

    /// Sends the reply and consumes the handle.
    ///
    /// # Errors
    /// Propagates the binding's send error.
    pub fn reply(self, headers: Headers, payload: Bytes) -> Result<(), BusError> {
        self.sink.send(Reply { headers, payload })
    }
}

impl fmt::Debug for ReplyHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ReplyHandle(..)")
    }
}

/// A request received by a server.
#[derive(Debug)]
pub struct Request {
    /// Subject the requester addressed.
    pub subject: Subject,
    /// Request headers.
    pub headers: Headers,
    /// Request payload.
    pub payload: Bytes,
    /// The single reply right for this request.
    pub reply: ReplyHandle,
}

/// Binding-side settlement of one delivery.
pub trait Settlement: Send {
    /// Marks the message done; it is never redelivered.
    ///
    /// # Errors
    /// Returns [`BusError::Unavailable`] when the consumer no longer exists
    /// (for example after a restart); the message is then redelivered.
    fn ack(self: Box<Self>) -> Result<(), BusError>;
    /// Asks for redelivery after `delay`.
    ///
    /// # Errors
    /// As for [`Settlement::ack`].
    fn nak(self: Box<Self>, delay: Duration) -> Result<(), BusError>;
    /// Stops redelivery of this message.
    ///
    /// # Errors
    /// As for [`Settlement::ack`].
    fn term(self: Box<Self>) -> Result<(), BusError>;
    /// Extends the ack deadline without settling.
    ///
    /// # Errors
    /// As for [`Settlement::ack`].
    fn in_progress(&self) -> Result<(), BusError>;
}

/// A message handed to a consumer. Exactly one of `ack`, `nak` or `term`
/// settles it (RFC-0011 [0011-015]).
pub struct Delivery {
    subject: Subject,
    headers: Headers,
    payload: Bytes,
    stream_seq: StreamSeq,
    attempt: u32,
    settlement: Box<dyn Settlement>,
}

impl Delivery {
    /// Builds a delivery. Used by bindings.
    #[must_use]
    pub fn new(
        subject: Subject,
        headers: Headers,
        payload: Bytes,
        stream_seq: StreamSeq,
        attempt: u32,
        settlement: Box<dyn Settlement>,
    ) -> Self {
        Self {
            subject,
            headers,
            payload,
            stream_seq,
            attempt,
            settlement,
        }
    }

    /// Subject the message was published on.
    #[must_use]
    pub const fn subject(&self) -> &Subject {
        &self.subject
    }

    /// Message headers.
    #[must_use]
    pub const fn headers(&self) -> &Headers {
        &self.headers
    }

    /// Message payload.
    #[must_use]
    pub const fn payload(&self) -> &Bytes {
        &self.payload
    }

    /// Stream sequence of the message.
    #[must_use]
    pub const fn stream_seq(&self) -> StreamSeq {
        self.stream_seq
    }

    /// Delivery attempt, starting at 1.
    #[must_use]
    pub const fn attempt(&self) -> u32 {
        self.attempt
    }

    /// Acknowledges the message.
    ///
    /// # Errors
    /// See [`Settlement::ack`].
    pub fn ack(self) -> Result<(), BusError> {
        self.settlement.ack()
    }

    /// Requests redelivery after `delay`.
    ///
    /// # Errors
    /// See [`Settlement::nak`].
    pub fn nak(self, delay: Duration) -> Result<(), BusError> {
        self.settlement.nak(delay)
    }

    /// Terminates the message.
    ///
    /// # Errors
    /// See [`Settlement::term`].
    pub fn term(self) -> Result<(), BusError> {
        self.settlement.term()
    }

    /// Extends the ack deadline while work continues.
    ///
    /// # Errors
    /// See [`Settlement::in_progress`].
    pub fn in_progress(&self) -> Result<(), BusError> {
        self.settlement.in_progress()
    }
}

impl fmt::Debug for Delivery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Delivery")
            .field("subject", &self.subject)
            .field("stream_seq", &self.stream_seq)
            .field("attempt", &self.attempt)
            .finish_non_exhaustive()
    }
}

/// Where a new consumer starts (RFC-0011 [0011-014]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartPosition {
    /// Start at this sequence; fails with `Unavailable{RETENTION_EXCEEDED}` if
    /// it is no longer retained.
    FromSeq(StreamSeq),
    /// Start at the oldest retained message.
    FromFirst,
    /// Start after the newest message.
    NewOnly,
}

/// A KV entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The key.
    pub key: Key,
    /// The stored value.
    pub value: Bytes,
    /// Revision of the write that produced this value.
    pub revision: Revision,
}

/// One event of a KV watch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEvent {
    /// A key was written, or is part of the initial snapshot.
    Put(Entry),
    /// A key was deleted or expired.
    Delete {
        /// The removed key.
        key: Key,
        /// Revision of the delete marker.
        revision: Revision,
    },
    /// End of the initial snapshot. Delivered exactly once, even when the
    /// snapshot is empty (RFC-0011 [0011-026]).
    SnapshotEnd,
}
