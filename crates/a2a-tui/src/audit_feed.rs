//! Audit tail feed (RFC-0013, RFC-0011). The console reads audit records from one durable
//! consumer; the binary chooses the stream, consumer, and subject filter names.

use std::fmt;

use a2a_types::proto::a2abus::AuditEvent;
use bus_api::bus::{Bus, Consumer};
use bus_api::error::BusError;
use bus_api::message::StartPosition;
use bus_api::name::{ConsumerName, Principal, StreamName, SubjectPattern};
use prost::Message;

/// Failure reading one audit record.
#[derive(Debug, thiserror::Error)]
pub enum FeedError {
    /// The consumer failed; the feed may recover on retry.
    #[error("bus: {0}")]
    Bus(#[from] BusError),
    /// One record did not decode; it was terminated and the feed continues.
    #[error("malformed audit record: {0}")]
    Malformed(#[from] prost::DecodeError),
}

/// Source of audit records for the console.
pub trait AuditFeed: Send {
    /// Waits for the next audit record and settles its delivery.
    fn next(&mut self) -> impl Future<Output = Result<AuditEvent, FeedError>> + Send;
}

/// Audit feed backed by one durable consumer of a bus binding.
pub struct BusAuditFeed<C> {
    consumer: C,
}

impl<C> fmt::Debug for BusAuditFeed<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BusAuditFeed").finish_non_exhaustive()
    }
}

impl<C: Consumer> BusAuditFeed<C> {
    /// Opens the consumer on `stream` with the subject `filter`, starting from the first record.
    ///
    /// # Errors
    /// Returns the bus error when the consumer cannot be opened.
    pub async fn open<B: Bus<Consumer = C>>(
        bus: &B,
        who: &Principal,
        stream: &StreamName,
        consumer: &ConsumerName,
        filter: &SubjectPattern,
    ) -> Result<Self, BusError> {
        let consumer = bus
            .consume(who, stream, consumer, filter, StartPosition::FromFirst)
            .await?;
        Ok(Self { consumer })
    }
}

impl<C: Consumer> AuditFeed for BusAuditFeed<C> {
    async fn next(&mut self) -> Result<AuditEvent, FeedError> {
        let delivery = self.consumer.next().await?;
        match AuditEvent::decode(delivery.payload().as_ref()) {
            Ok(event) => {
                delivery.ack()?;
                Ok(event)
            }
            Err(err) => {
                delivery.term()?;
                Err(FeedError::Malformed(err))
            }
        }
    }
}
