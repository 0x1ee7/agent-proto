//! Ordered task event stream over an ephemeral bus consumer (RFC-0002 0002-043, RFC-0005 0005-034,
//! RFC-0010 0010-050 and 0010-077).

use std::collections::{BTreeMap, VecDeque};
use std::fmt;

use a2a_core::errors::LocalError;
use a2a_core::subject;
use a2a_types::ids::{AgentId, MessageId, TaskId};
use bus_api::bus::{Bus, Consumer};
use bus_api::message::StartPosition;
use bus_api::name::{ConsumerName, StreamName, SubjectPattern};

use crate::client::Client;
use crate::error::ClientError;
use crate::event::{self, Event};

/// Buffered out-of-order events before the stream gives up on a gap and reattaches.
const GAP_LIMIT: usize = 32;
/// Consecutive attaches without progress before the stream reports the loss.
const MAX_FAILURES: u32 = 3;

/// Ordered, deduplicated events of one task.
///
/// Events are delivered in per-task sequence order exactly once. A gap is repaired by reattaching
/// and replaying from the retained event stream; when retention no longer covers the gap the
/// stream returns `LocalError::RetentionExceeded` and the caller MUST subscribe again for a fresh
/// snapshot (RFC-0002 0002-063).
pub struct EventStream<'a, B: Bus> {
    client: &'a Client<B>,
    agent: AgentId,
    task: TaskId,
    last: u64,
    buffer: BTreeMap<u64, Event>,
    ready: VecDeque<Event>,
    consumer: Option<B::Consumer>,
    first_pending: bool,
    progressed: bool,
    failures: u32,
    closing: bool,
}

impl<B: Bus> fmt::Debug for EventStream<'_, B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventStream")
            .field("agent", &self.agent)
            .field("task", &self.task)
            .field("last", &self.last)
            .field("buffered", &self.buffer.len())
            .field("closing", &self.closing)
            .finish_non_exhaustive()
    }
}

impl<'a, B: Bus> EventStream<'a, B> {
    /// Creates a stream that delivers events after sequence `after` (zero for all events).
    pub(crate) const fn new(
        client: &'a Client<B>,
        agent: AgentId,
        task: TaskId,
        after: u64,
    ) -> Self {
        Self {
            client,
            agent,
            task,
            last: after,
            buffer: BTreeMap::new(),
            ready: VecDeque::new(),
            consumer: None,
            first_pending: true,
            progressed: false,
            failures: 0,
            closing: false,
        }
    }

    /// Highest task sequence delivered so far.
    #[must_use]
    pub const fn last_seq(&self) -> u64 {
        self.last
    }

    /// Next event in sequence order, or `None` after the stream-end event was delivered.
    ///
    /// # Errors
    /// Returns `Local(RetentionExceeded)` when the retained events cannot repair a gap, and
    /// `Local(Unavailable)` after repeated consumer failures without progress.
    pub async fn next(&mut self) -> Result<Option<Event>, ClientError> {
        loop {
            if let Some(event) = self.ready.pop_front() {
                return Ok(Some(event));
            }
            if self.closing {
                return Ok(None);
            }
            if self.consumer.is_none() {
                self.attach().await?;
            }
            let Some(consumer) = self.consumer.as_mut() else {
                continue;
            };
            match consumer.next().await {
                Ok(delivery) => {
                    let decoded = event::decode(
                        &self.client.config.wire,
                        delivery.principal(),
                        delivery.subject(),
                        delivery.headers(),
                        delivery.payload(),
                    );
                    // Ordered consumers use ack policy none (RFC-0010 0010-052); settling is best
                    // effort because duplicates are filtered by task sequence.
                    let _ = delivery.ack();
                    self.accept(decoded?)?;
                }
                Err(_) => self.resync(LocalError::Unavailable)?,
            }
        }
    }

    async fn attach(&mut self) -> Result<(), ClientError> {
        let client = self.client;
        let subject =
            subject::event(&client.tenant, &self.agent, &self.task).map_err(ClientError::Name)?;
        let stream = StreamName::parse(&format!("A2AB_EV_{}", client.tenant.as_str()))
            .map_err(ClientError::Name)?;
        let consumer_name = ConsumerName::parse(&format!("cli-{}", MessageId::generate().as_str()))
            .map_err(ClientError::Name)?;
        let filter = SubjectPattern::exact(&subject);
        let consumer = client
            .bus
            .consume(
                &client.principal,
                &stream,
                &consumer_name,
                &filter,
                StartPosition::FromFirst,
            )
            .await
            .map_err(crate::error::from_bus)?;
        self.consumer = Some(consumer);
        self.buffer.clear();
        self.first_pending = true;
        self.progressed = false;
        Ok(())
    }

    fn accept(&mut self, event: Event) -> Result<(), ClientError> {
        if event.seq <= self.last {
            return Ok(());
        }
        if event.seq == self.last + 1 {
            self.first_pending = false;
            self.emit(event);
            self.flush();
            return Ok(());
        }
        // A gap at the start of an attach means the retained events no longer reach back to the
        // last delivered sequence (RFC-0010 0010-077).
        if self.first_pending {
            return Err(ClientError::Local(LocalError::RetentionExceeded));
        }
        if self.buffer.contains_key(&event.seq) {
            return Ok(());
        }
        let gap_closed_by_end = event.stream_end;
        self.buffer.insert(event.seq, event);
        if gap_closed_by_end || self.buffer.len() > GAP_LIMIT {
            self.resync(LocalError::RetentionExceeded)?;
        }
        Ok(())
    }

    fn emit(&mut self, event: Event) {
        self.last = event.seq;
        self.progressed = true;
        if event.stream_end {
            self.closing = true;
        }
        self.ready.push_back(event);
    }

    fn flush(&mut self) {
        while !self.closing {
            let Some(event) = self.buffer.remove(&(self.last + 1)) else {
                break;
            };
            self.emit(event);
        }
    }

    /// Drops the current consumer and buffered events. Gives up after `MAX_FAILURES` attaches
    /// without progress.
    fn resync(&mut self, reason: LocalError) -> Result<(), ClientError> {
        if self.progressed {
            self.failures = 0;
        } else {
            self.failures += 1;
        }
        self.consumer = None;
        self.buffer.clear();
        if self.failures >= MAX_FAILURES {
            return Err(ClientError::Local(reason));
        }
        Ok(())
    }
}
