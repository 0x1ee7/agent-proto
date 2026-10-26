//! One bus binding shared through `Arc` by two users (the worker and a client).

use std::fmt::Debug;
use std::sync::Arc;
use std::time::Duration;

use a2a_types::ids::TenantId;
use bus_api::bus::{Bus, Consumer};
use bus_api::headers::Headers;
use bus_api::message::StartPosition;
use bus_api::name::{ConsumerName, Principal, StreamName, Subject, SubjectPattern};
use bus_mem::{Grant, MemBus, MemConfig, StreamConfig, m1_capabilities};
use bytes::Bytes;

fn check<T, E: Debug>(result: Result<T, E>) -> Result<T, String> {
    result.map_err(|error| format!("{error:?}"))
}

/// [0011-014] A consumer sees messages published through another handle to the same
/// bus, in publish order. [0011-092] Each delivery names the principal that published it.
#[tokio::test]
async fn req_0011_014_shared_handle_publishes_are_visible_to_other_user() -> Result<(), String> {
    let tenant = check(TenantId::parse("t1"))?;
    let alice = check(Principal::named("alice"))?;
    let bob = check(Principal::named("bob"))?;
    let stream = check(StreamName::parse("SHARED"))?;
    let pattern = check(SubjectPattern::parse("a2a.v1.t1.>"))?;
    let from_worker = check(Subject::parse("a2a.v1.t1.worker"))?;
    let from_client = check(Subject::parse("a2a.v1.t1.client"))?;
    let reader = check(ConsumerName::parse("reader"))?;

    let grant = |who: &Principal| Grant {
        principal: who.clone(),
        tenant: tenant.clone(),
        publish: vec![pattern.clone()],
        serve: Vec::new(),
        streams: vec![stream.clone()],
    };
    let config = MemConfig {
        seed: 1,
        capabilities: m1_capabilities(),
        streams: vec![StreamConfig {
            name: stream.clone(),
            subjects: vec![pattern.clone()],
            retention_messages: None,
        }],
        kv_buckets: Vec::new(),
        grants: vec![grant(&alice), grant(&bob)],
        ack_wait: Duration::from_secs(5),
        overload_retry_after: Duration::from_secs(1),
        blob_tenant_quota: 1 << 20,
    };
    let bus = Arc::new(check(MemBus::new(config))?);
    let worker = Arc::clone(&bus);
    let client = Arc::clone(&bus);

    check(
        worker
            .publish(
                &alice,
                &from_worker,
                &Headers::new(),
                Bytes::from_static(b"from worker"),
            )
            .await,
    )?;
    check(
        client
            .publish(
                &bob,
                &from_client,
                &Headers::new(),
                Bytes::from_static(b"from client"),
            )
            .await,
    )?;

    let mut consumer = check(
        worker
            .consume(&alice, &stream, &reader, &pattern, StartPosition::FromFirst)
            .await,
    )?;

    let first = check(consumer.next().await)?;
    assert_eq!(first.payload(), &Bytes::from_static(b"from worker"));
    assert_eq!(first.principal().and_then(Principal::name), Some("alice"));
    check(first.ack())?;

    let second = check(consumer.next().await)?;
    assert_eq!(second.payload(), &Bytes::from_static(b"from client"));
    assert_eq!(second.principal().and_then(Principal::name), Some("bob"));
    check(second.ack())?;
    Ok(())
}
