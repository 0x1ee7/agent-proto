//! The bus seam (RFC-0001 [0001-006], RFC-0011 section 4.2).
//!
//! Every operation takes the caller's [`Principal`]. Methods return
//! `impl Future + Send` instead of boxing, so bindings add no allocation of their
//! own beyond what they need.

use std::sync::Arc;
use std::time::Duration;

use a2a_types::ids::TenantId;
use bytes::Bytes;

use crate::capabilities::Capabilities;
use crate::error::BusError;
use crate::headers::Headers;
use crate::message::{Delivery, Entry, PublishAck, Reply, Request, StartPosition, WatchEvent};
use crate::name::{
    Bucket, ConsumerName, Key, KeyPrefix, MsgId, Principal, StreamName, Subject, SubjectPattern,
};
use crate::value::{Digest, Revision};

/// The transport-neutral bus. Implemented by every binding.
pub trait Bus: Send + Sync {
    /// Stream of requests received by a `serve` registration.
    type Requests: RequestStream;
    /// Durable consumer handle.
    type Consumer: Consumer;
    /// KV watch handle.
    type Watch: Watch;

    /// Declared capabilities. Stable for the lifetime of the binding.
    fn capabilities(&self) -> &Capabilities;

    /// Publishes a message. Stored in a durable stream when the subject is
    /// captured; otherwise core delivery.
    fn publish(
        &self,
        who: &Principal,
        subject: &Subject,
        headers: &Headers,
        payload: Bytes,
    ) -> impl Future<Output = Result<PublishAck, BusError>> + Send;

    /// Sends a request and waits up to `timeout` for the single reply.
    fn request(
        &self,
        who: &Principal,
        subject: &Subject,
        headers: &Headers,
        payload: Bytes,
        timeout: Duration,
    ) -> impl Future<Output = Result<Reply, BusError>> + Send;

    /// Registers as a responder for requests matching `pattern`.
    fn serve(
        &self,
        who: &Principal,
        pattern: &SubjectPattern,
    ) -> impl Future<Output = Result<Self::Requests, BusError>> + Send;

    /// Appends to a durable stream. A repeated `msg_id` inside the dedupe
    /// window fails with [`BusError::Duplicate`].
    fn append(
        &self,
        who: &Principal,
        stream: &StreamName,
        subject: &Subject,
        headers: &Headers,
        payload: Bytes,
        msg_id: &MsgId,
    ) -> impl Future<Output = Result<PublishAck, BusError>> + Send;

    /// Opens or resumes the durable consumer `consumer` on `stream`. `start`
    /// applies only when the consumer is created.
    fn consume(
        &self,
        who: &Principal,
        stream: &StreamName,
        consumer: &ConsumerName,
        filter: &SubjectPattern,
        start: StartPosition,
    ) -> impl Future<Output = Result<Self::Consumer, BusError>> + Send;

    /// Writes `key` unconditionally. Returns the new revision.
    fn kv_put(
        &self,
        who: &Principal,
        bucket: &Bucket,
        key: &Key,
        value: Bytes,
        ttl: Option<Duration>,
    ) -> impl Future<Output = Result<Revision, BusError>> + Send;

    /// Reads the live entry for `key`; `Absent` when there is none.
    fn kv_get(
        &self,
        who: &Principal,
        bucket: &Bucket,
        key: &Key,
    ) -> impl Future<Output = Result<Entry, BusError>> + Send;

    /// Writes `key` only when no live entry exists. Returns the new revision.
    fn kv_create(
        &self,
        who: &Principal,
        bucket: &Bucket,
        key: &Key,
        value: Bytes,
        ttl: Option<Duration>,
    ) -> impl Future<Output = Result<Revision, BusError>> + Send;

    /// Writes `key` only when its live revision equals `expected`.
    fn kv_update(
        &self,
        who: &Principal,
        bucket: &Bucket,
        key: &Key,
        value: Bytes,
        expected: Revision,
        ttl: Option<Duration>,
    ) -> impl Future<Output = Result<Revision, BusError>> + Send;

    /// Deletes `key`. When `expected` is set the live revision must match.
    /// Returns the delete marker's revision.
    fn kv_delete(
        &self,
        who: &Principal,
        bucket: &Bucket,
        key: &Key,
        expected: Option<Revision>,
    ) -> impl Future<Output = Result<Revision, BusError>> + Send;

    /// Watches keys under `prefix`: snapshot, then [`WatchEvent::SnapshotEnd`],
    /// then live changes.
    fn kv_watch(
        &self,
        who: &Principal,
        bucket: &Bucket,
        prefix: &KeyPrefix,
    ) -> impl Future<Output = Result<Self::Watch, BusError>> + Send;

    /// Stores content addressed by its SHA-256 digest in `tenant`'s namespace.
    fn blob_put(
        &self,
        who: &Principal,
        tenant: &TenantId,
        bytes: Bytes,
    ) -> impl Future<Output = Result<Digest, BusError>> + Send;

    /// Reads content by digest. Verifies the digest before returning.
    fn blob_get(
        &self,
        who: &Principal,
        tenant: &TenantId,
        digest: &Digest,
    ) -> impl Future<Output = Result<Bytes, BusError>> + Send;

    /// Closes the binding. Unsettled deliveries stay pending for redelivery.
    fn close(&self) -> impl Future<Output = Result<(), BusError>> + Send;
}

/// A shared handle is a binding: `Arc<B>` forwards every operation to the inner bus.
/// One connection can then serve several users, such as a worker and a client in one
/// process.
impl<B: Bus + ?Sized> Bus for Arc<B> {
    type Requests = B::Requests;
    type Consumer = B::Consumer;
    type Watch = B::Watch;

    fn capabilities(&self) -> &Capabilities {
        (**self).capabilities()
    }

    fn publish(
        &self,
        who: &Principal,
        subject: &Subject,
        headers: &Headers,
        payload: Bytes,
    ) -> impl Future<Output = Result<PublishAck, BusError>> + Send {
        (**self).publish(who, subject, headers, payload)
    }

    fn request(
        &self,
        who: &Principal,
        subject: &Subject,
        headers: &Headers,
        payload: Bytes,
        timeout: Duration,
    ) -> impl Future<Output = Result<Reply, BusError>> + Send {
        (**self).request(who, subject, headers, payload, timeout)
    }

    fn serve(
        &self,
        who: &Principal,
        pattern: &SubjectPattern,
    ) -> impl Future<Output = Result<Self::Requests, BusError>> + Send {
        (**self).serve(who, pattern)
    }

    fn append(
        &self,
        who: &Principal,
        stream: &StreamName,
        subject: &Subject,
        headers: &Headers,
        payload: Bytes,
        msg_id: &MsgId,
    ) -> impl Future<Output = Result<PublishAck, BusError>> + Send {
        (**self).append(who, stream, subject, headers, payload, msg_id)
    }

    fn consume(
        &self,
        who: &Principal,
        stream: &StreamName,
        consumer: &ConsumerName,
        filter: &SubjectPattern,
        start: StartPosition,
    ) -> impl Future<Output = Result<Self::Consumer, BusError>> + Send {
        (**self).consume(who, stream, consumer, filter, start)
    }

    fn kv_put(
        &self,
        who: &Principal,
        bucket: &Bucket,
        key: &Key,
        value: Bytes,
        ttl: Option<Duration>,
    ) -> impl Future<Output = Result<Revision, BusError>> + Send {
        (**self).kv_put(who, bucket, key, value, ttl)
    }

    fn kv_get(
        &self,
        who: &Principal,
        bucket: &Bucket,
        key: &Key,
    ) -> impl Future<Output = Result<Entry, BusError>> + Send {
        (**self).kv_get(who, bucket, key)
    }

    fn kv_create(
        &self,
        who: &Principal,
        bucket: &Bucket,
        key: &Key,
        value: Bytes,
        ttl: Option<Duration>,
    ) -> impl Future<Output = Result<Revision, BusError>> + Send {
        (**self).kv_create(who, bucket, key, value, ttl)
    }

    fn kv_update(
        &self,
        who: &Principal,
        bucket: &Bucket,
        key: &Key,
        value: Bytes,
        expected: Revision,
        ttl: Option<Duration>,
    ) -> impl Future<Output = Result<Revision, BusError>> + Send {
        (**self).kv_update(who, bucket, key, value, expected, ttl)
    }

    fn kv_delete(
        &self,
        who: &Principal,
        bucket: &Bucket,
        key: &Key,
        expected: Option<Revision>,
    ) -> impl Future<Output = Result<Revision, BusError>> + Send {
        (**self).kv_delete(who, bucket, key, expected)
    }

    fn kv_watch(
        &self,
        who: &Principal,
        bucket: &Bucket,
        prefix: &KeyPrefix,
    ) -> impl Future<Output = Result<Self::Watch, BusError>> + Send {
        (**self).kv_watch(who, bucket, prefix)
    }

    fn blob_put(
        &self,
        who: &Principal,
        tenant: &TenantId,
        bytes: Bytes,
    ) -> impl Future<Output = Result<Digest, BusError>> + Send {
        (**self).blob_put(who, tenant, bytes)
    }

    fn blob_get(
        &self,
        who: &Principal,
        tenant: &TenantId,
        digest: &Digest,
    ) -> impl Future<Output = Result<Bytes, BusError>> + Send {
        (**self).blob_get(who, tenant, digest)
    }

    fn close(&self) -> impl Future<Output = Result<(), BusError>> + Send {
        (**self).close()
    }
}

/// Requests received through [`Bus::serve`].
pub trait RequestStream: Send {
    /// Waits for the next request.
    fn next(&mut self) -> impl Future<Output = Result<Request, BusError>> + Send;
}

/// A durable consumer.
pub trait Consumer: Send {
    /// Waits for the next deliverable message.
    fn next(&mut self) -> impl Future<Output = Result<Delivery, BusError>> + Send;
}

/// A KV watch.
pub trait Watch: Send {
    /// Waits for the next event.
    fn next(&mut self) -> impl Future<Output = Result<WatchEvent, BusError>> + Send;
}
