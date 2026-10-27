//! Adapters from async-nats acknowledgement and reply primitives to the
//! synchronous `bus-api` traits [`Settlement`] and [`ReplySink`].
//!
//! The `bus-api` traits are synchronous, while the NATS calls are asynchronous. Each
//! call is dispatched to the Tokio runtime that drives the connection. The
//! result of the NATS call is not observable through the trait: a failed
//! acknowledgement leaves the message unacknowledged, so the server redelivers it
//! after `ack_wait` (0010-073). The caller sees `Ok` once the dispatch is queued.

use std::sync::Arc;
use std::time::Duration;

use async_nats::Client;
use async_nats::jetstream::message::{AckKind, Acker};
use bus_api::error::BusError;
use bus_api::message::{Reply, ReplySink, Settlement};
use bytes::Bytes;

use crate::wire::{check_budget, to_header_map};

/// Runs `fut` on the current Tokio runtime. Fails with `Internal` when no runtime is
/// active, since a delivery cannot be settled without one.
pub(crate) fn dispatch<F>(fut: F) -> Result<(), BusError>
where
    F: Future<Output = ()> + Send + 'static,
{
    let handle = tokio::runtime::Handle::try_current().map_err(|_| BusError::Internal)?;
    handle.spawn(fut);
    Ok(())
}

/// Settlement of one JetStream delivery.
pub(crate) struct JetStreamSettlement {
    acker: Arc<Acker>,
}

impl JetStreamSettlement {
    pub(crate) fn new(acker: Acker) -> Self {
        Self {
            acker: Arc::new(acker),
        }
    }

    fn send(&self, kind: AckKind) -> Result<(), BusError> {
        let acker = Arc::clone(&self.acker);
        dispatch(async move {
            let _ = acker.ack_with(kind).await;
        })
    }
}

impl Settlement for JetStreamSettlement {
    fn ack(self: Box<Self>) -> Result<(), BusError> {
        self.send(AckKind::Ack)
    }

    fn nak(self: Box<Self>, delay: Duration) -> Result<(), BusError> {
        self.send(AckKind::Nak(Some(delay)))
    }

    fn term(self: Box<Self>) -> Result<(), BusError> {
        self.send(AckKind::Term)
    }

    fn in_progress(&self) -> Result<(), BusError> {
        self.send(AckKind::Progress)
    }
}

/// Reply right for one request. Replies are published to the request's reply
/// subject, which the server permits only once per request (`allow_responses`).
pub(crate) struct NatsReply {
    client: Client,
    reply_to: String,
}

impl NatsReply {
    pub(crate) fn new(client: Client, reply_to: String) -> Self {
        Self { client, reply_to }
    }
}

impl ReplySink for NatsReply {
    fn send(self: Box<Self>, reply: Reply) -> Result<(), BusError> {
        check_budget(&reply.headers, &reply.payload)?;
        let headers = to_header_map(&reply.headers);
        let payload: Bytes = reply.payload;
        let client = self.client;
        let subject = self.reply_to;
        dispatch(async move {
            let _ = client.publish_with_headers(subject, headers, payload).await;
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 0011-015: a delivery settles without a runtime as an Internal error, never a panic.
    #[test]
    fn req_0011_015_dispatch_without_runtime_is_internal_error() {
        let result = dispatch(async {});
        assert_eq!(result, Err(BusError::Internal));
    }
}
