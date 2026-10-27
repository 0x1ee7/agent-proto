//! Inbound requests (RFC-0010 section 4.2): a core subscription whose replies go to
//! the requester's inbox.

use std::fmt;

use async_nats::{Client, Subscriber};
use bus_api::bus::RequestStream;
use bus_api::error::BusError;
use bus_api::message::{ReplyHandle, Request};
use bus_api::name::Subject;
use futures_util::StreamExt;

use crate::errors::{self, reason};
use crate::principal::principal_from_inbox;
use crate::settle::NatsReply;
use crate::wire::inbound_headers;

/// Stream of requests served on one subject.
pub struct NatsRequests {
    subscriber: Subscriber,
    client: Client,
}

impl fmt::Debug for NatsRequests {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NatsRequests").finish_non_exhaustive()
    }
}

impl NatsRequests {
    pub(crate) fn new(subscriber: Subscriber, client: Client) -> Self {
        Self { subscriber, client }
    }
}

impl RequestStream for NatsRequests {
    async fn next(&mut self) -> Result<Request, BusError> {
        loop {
            let msg = self
                .subscriber
                .next()
                .await
                .ok_or_else(|| errors::unavailable(reason::SUBSCRIPTION_CLOSED))?;
            // A request without a reply inbox cannot be answered, and one whose inbox
            // does not carry a principal cannot be attributed (0010-078); neither is
            // delivered.
            let Some(reply_to) = msg.reply else {
                continue;
            };
            let Some(principal) = principal_from_inbox(reply_to.as_str()) else {
                continue;
            };
            let Some(headers) = inbound_headers(msg.headers.as_ref()) else {
                continue;
            };
            let Ok(subject) = Subject::parse(msg.subject.as_str()) else {
                continue;
            };
            let sink = NatsReply::new(self.client.clone(), reply_to.to_string());
            return Ok(Request {
                subject,
                principal: Some(principal),
                headers,
                payload: msg.payload,
                reply: ReplyHandle::new(Box::new(sink)),
            });
        }
    }
}
