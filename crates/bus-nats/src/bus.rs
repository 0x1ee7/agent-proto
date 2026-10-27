//! The `Bus` implementation over one NATS connection (RFC-0011 section 4.2).
//! Every operation first checks the caller's principal (0010-078).

use std::sync::atomic::Ordering;
use std::time::Duration;

use a2a_types::ids::TenantId;
use bus_api::bus::Bus;
use bus_api::capabilities::Capabilities;
use bus_api::error::BusError;
use bus_api::headers::Headers;
use bus_api::message::{PublishAck, Reply, StartPosition};
use bus_api::name::{
    Bucket, ConsumerName, Key, KeyPrefix, MsgId, Principal, StreamName, Subject, SubjectPattern,
};
use bus_api::value::{Digest, Revision};
use bytes::Bytes;

use crate::blob;
use crate::connection::NatsBus;
use crate::denial::guard;
use crate::errors::{self, reason};
use crate::kv;
use crate::request::NatsRequests;
use crate::stream::{self, NatsConsumer};
use crate::wire::{check_budget, inbound_headers, to_header_map};

impl Bus for NatsBus {
    type Requests = NatsRequests;
    type Consumer = NatsConsumer;
    type Watch = kv::NatsWatch;

    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    async fn publish(
        &self,
        who: &Principal,
        subject: &Subject,
        headers: &Headers,
        payload: Bytes,
    ) -> Result<PublishAck, BusError> {
        self.check_who(who)?;
        stream::publish(self, subject, headers, payload).await
    }

    async fn request(
        &self,
        who: &Principal,
        subject: &Subject,
        headers: &Headers,
        payload: Bytes,
        timeout: Duration,
    ) -> Result<Reply, BusError> {
        self.check_who(who)?;
        check_budget(headers, &payload)?;
        let request = async_nats::Request::new()
            .headers(to_header_map(headers))
            .payload(payload)
            .timeout(Some(timeout));
        let pending = self
            .client()
            .send_request(subject.as_str().to_owned(), request);
        let timed = tokio::time::timeout(timeout, pending);
        let message = guard(self.denials(), |s: &str| s == subject.as_str(), timed)
            .await?
            .map_err(|_| BusError::Timeout)?
            .map_err(|err| errors::request(&err))?;
        let headers = inbound_headers(message.headers.as_ref())
            .ok_or_else(|| errors::unavailable(reason::REQUEST_FAILED))?;
        Ok(Reply {
            headers,
            payload: message.payload,
        })
    }

    async fn serve(
        &self,
        who: &Principal,
        pattern: &SubjectPattern,
    ) -> Result<Self::Requests, BusError> {
        self.check_who(who)?;
        let client = self.client();
        let subscriber = client
            .subscribe(pattern.as_str().to_owned())
            .await
            .map_err(|_| errors::unavailable(reason::SUBSCRIBE_FAILED))?;
        // Subscribing only queues the SUB; the flush makes the server register it before a
        // request from another connection can be routed, so no request sees `NoResponder`.
        client
            .flush()
            .await
            .map_err(|_| errors::unavailable(reason::SUBSCRIBE_FAILED))?;
        Ok(NatsRequests::new(subscriber, client.clone()))
    }

    async fn append(
        &self,
        who: &Principal,
        stream_name: &StreamName,
        subject: &Subject,
        headers: &Headers,
        payload: Bytes,
        msg_id: &MsgId,
    ) -> Result<PublishAck, BusError> {
        self.check_who(who)?;
        stream::append(self, stream_name, subject, headers, payload, msg_id).await
    }

    async fn consume(
        &self,
        who: &Principal,
        stream_name: &StreamName,
        consumer: &ConsumerName,
        filter: &SubjectPattern,
        start: StartPosition,
    ) -> Result<Self::Consumer, BusError> {
        self.check_who(who)?;
        stream::consume(self, stream_name, consumer, filter, start).await
    }

    async fn kv_put(
        &self,
        who: &Principal,
        bucket: &Bucket,
        key: &Key,
        value: Bytes,
        ttl: Option<Duration>,
    ) -> Result<Revision, BusError> {
        self.check_who(who)?;
        kv::put(self, bucket, key, value, ttl).await
    }

    async fn kv_get(
        &self,
        who: &Principal,
        bucket: &Bucket,
        key: &Key,
    ) -> Result<bus_api::message::Entry, BusError> {
        self.check_who(who)?;
        kv::get(self, bucket, key).await
    }

    async fn kv_create(
        &self,
        who: &Principal,
        bucket: &Bucket,
        key: &Key,
        value: Bytes,
        ttl: Option<Duration>,
    ) -> Result<Revision, BusError> {
        self.check_who(who)?;
        kv::create(self, bucket, key, value, ttl).await
    }

    async fn kv_update(
        &self,
        who: &Principal,
        bucket: &Bucket,
        key: &Key,
        value: Bytes,
        expected: Revision,
        ttl: Option<Duration>,
    ) -> Result<Revision, BusError> {
        self.check_who(who)?;
        kv::update(self, bucket, key, value, expected, ttl).await
    }

    async fn kv_delete(
        &self,
        who: &Principal,
        bucket: &Bucket,
        key: &Key,
        expected: Option<Revision>,
    ) -> Result<Revision, BusError> {
        self.check_who(who)?;
        kv::delete(self, bucket, key, expected).await
    }

    async fn kv_watch(
        &self,
        who: &Principal,
        bucket: &Bucket,
        prefix: &KeyPrefix,
    ) -> Result<Self::Watch, BusError> {
        self.check_who(who)?;
        kv::watch(self, bucket, prefix).await
    }

    async fn blob_put(
        &self,
        who: &Principal,
        tenant: &TenantId,
        bytes: Bytes,
    ) -> Result<Digest, BusError> {
        self.check_who(who)?;
        blob::put(self, tenant, bytes).await
    }

    async fn blob_get(
        &self,
        who: &Principal,
        tenant: &TenantId,
        digest: &Digest,
    ) -> Result<Bytes, BusError> {
        self.check_who(who)?;
        blob::get(self, tenant, digest).await
    }

    async fn close(&self) -> Result<(), BusError> {
        self.closed().store(true, Ordering::SeqCst);
        self.client()
            .drain()
            .await
            .map_err(|_| errors::unavailable(reason::CLOSE_FAILED))
    }
}
