//! Live tests against a real `nats-server` with JetStream. All are `#[ignore]`:
//! `cargo test -p bus-nats -- --ignored`. The server binary comes from
//! `NATS_SERVER_BIN`, or the Windows spike path when unset.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeMap;
use std::time::Duration;

use a2a_conformance::bus_contract::{BusFactory, run_suite};
use a2a_conformance::driver::Clock;
use a2a_conformance::fixtures::Fixtures;
use a2a_conformance::report::Outcome;
use bus_api::bus::{Bus, Consumer, Watch};
use bus_api::error::BusError;
use bus_api::headers::Headers;
use bus_api::message::{StartPosition, WatchEvent};
use bus_api::name::{ConsumerName, Key, KeyPrefix, MsgId, StreamName, Subject, SubjectPattern};
use bus_nats::NatsBus;
use bus_nats::config::{NatsConfig, Seed};
use bus_nats::connect;
use bus_nats::layout::{PRESENCE, audit_stream, ev_stream, kv_bucket, kv_stream};
use bus_nats::provision::{SUPERVISOR, provision};
use bytes::Bytes;
use support::factory::{NatsFactory, provision_suite};
use support::server::TestServer;

/// Real-clock scale for the suite: 0.2 of nominal time.
const SCALE: f64 = 0.2;
const SUITE_SEED: u64 = 0x5eed_0011;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().expect("tokio runtime")
}

fn fixtures() -> Fixtures {
    Fixtures::new(Clock::real(SCALE).expect("scale")).expect("fixtures")
}

/// Starts a server, provisions the suite resources, and returns the server
/// alongside its fixtures.
fn start(rt: &tokio::runtime::Runtime) -> (TestServer, Fixtures) {
    let fx = fixtures();
    let server = TestServer::start(&fx).expect("nats-server");
    rt.block_on(provision_suite(&server, &fx))
        .expect("provision suite resources");
    (server, fx)
}

#[test]
#[ignore = "live: needs nats-server"]
fn req_0011_conformance_suite_real_clock() {
    let rt = runtime();
    let (server, fx) = start(&rt);
    let factory = NatsFactory::new(&server, rt.handle().clone(), fx.clock);
    // The suite busy-polls futures that use tokio timers, so this thread holds
    // the runtime context for the whole run.
    let _guard = rt.enter();
    let report = run_suite(&factory, SUITE_SEED).expect("suite runs");
    let failures: Vec<_> = report
        .results
        .iter()
        .filter(|row| row.outcome == Outcome::Fail)
        .collect();
    for row in &failures {
        eprintln!("FAIL {}: {}", row.id, row.reason);
    }
    assert!(failures.is_empty(), "{} suite rows failed", failures.len());
}

#[test]
#[ignore = "live: needs nats-server"]
fn req_0010_078_unlisted_nkey_refused() {
    let rt = runtime();
    let (server, fx) = start(&rt);
    let denied = fx.denied.name().expect("denied name").to_owned();
    let config = NatsConfig {
        url: server.url().to_owned(),
        identity: denied.clone(),
        seed: Seed::new(server.seed(&denied).expect("denied seed")),
        tenant: fx.local.clone(),
        request_timeout: fx.request_timeout,
        ack_wait: fx.ack_wait,
    };
    let result = rt.block_on(connect(config));
    assert!(result.is_err(), "unlisted NKey connected");
}

/// Connects `identity` to the test server with its NKey.
async fn connect_as(server: &TestServer, fx: &Fixtures, identity: &str) -> NatsBus {
    let config = NatsConfig {
        url: server.url().to_owned(),
        identity: identity.to_owned(),
        seed: Seed::new(server.seed(identity).expect("identity seed")),
        tenant: fx.local.clone(),
        request_timeout: fx.request_timeout,
        ack_wait: fx.ack_wait,
    };
    connect(config).await.expect("identity connects")
}

/// Provisions the local tenant's production objects as the supervisor, so the event
/// stream `A2AB_EV_<tenant>` exists with its production configuration (RFC-0010 4.5).
fn provision_local(rt: &tokio::runtime::Runtime, server: &TestServer, fx: &Fixtures) {
    rt.block_on(async {
        let supervisor = connect_as(server, fx, SUPERVISOR).await;
        provision(&supervisor)
            .await
            .expect("provision production objects");
    });
}

/// The event subject `a2a.v1.<tenant>.ev.<owner>.task-1`.
fn event_subject(fx: &Fixtures, owner: &str) -> Subject {
    Subject::parse(&format!("a2a.v1.{}.ev.{owner}.task-1", fx.local.as_str())).expect("subject")
}

/// 0010-060: an agent publishes only on its own event subjects. The server refuses a
/// publish on another agent's `ev.<owner>.>`, and the binding reports it as
/// `PermissionDenied`; the agent's own event subject still publishes.
#[test]
#[ignore = "live: needs nats-server"]
fn req_0010_060_agent_cannot_publish_foreign_event_subject() {
    let rt = runtime();
    let (server, fx) = start(&rt);
    provision_local(&rt, &server, &fx);
    let client = fx.client.name().expect("client name").to_owned();
    let other = fx.server.name().expect("server name").to_owned();
    rt.block_on(async {
        let bus = connect_as(&server, &fx, &client).await;
        let headers = Headers::new();
        let foreign = event_subject(&fx, &other);
        let denied = bus
            .publish(
                &fx.client,
                &foreign,
                &headers,
                Bytes::from_static(b"foreign"),
            )
            .await;
        assert_eq!(denied.err(), Some(BusError::PermissionDenied));
        let own = event_subject(&fx, &client);
        bus.publish(&fx.client, &own, &headers, Bytes::from_static(b"own"))
            .await
            .expect("own event subject publishes");
    });
}

/// 0010-060: an agent appends its own audit record. The binding decides the capture from
/// the layout, so the agent needs no `STREAM.INFO` on the audit stream it may not read.
#[test]
#[ignore = "live: needs nats-server"]
fn req_0010_060_agent_appends_own_audit_without_stream_info() {
    let rt = runtime();
    let (server, fx) = start(&rt);
    provision_local(&rt, &server, &fx);
    let client = fx.client.name().expect("client name").to_owned();
    rt.block_on(async {
        let bus = connect_as(&server, &fx, &client).await;
        let stream = StreamName::parse(&audit_stream(&fx.local)).expect("audit stream");
        let subject =
            Subject::parse(&format!("a2a.v1.{}.audit.{client}", fx.local)).expect("audit subject");
        let msg_id = MsgId::parse("req-0010-060-audit").expect("msg id");
        bus.append(
            &fx.client,
            &stream,
            &subject,
            &Headers::new(),
            Bytes::from_static(b"audit"),
            &msg_id,
        )
        .await
        .expect("own audit append");
    });
}

/// 0011-013: `append` names the stream it expects. The server refuses a subject another
/// stream captures (`Nats-Expected-Stream` mismatch), reported as `Absent`, so an audit
/// record cannot land in the event stream.
#[test]
#[ignore = "live: needs nats-server"]
fn req_0011_013_append_to_uncaptured_stream_is_absent() {
    let rt = runtime();
    let (server, fx) = start(&rt);
    provision_local(&rt, &server, &fx);
    let client = fx.client.name().expect("client name").to_owned();
    rt.block_on(async {
        let bus = connect_as(&server, &fx, &client).await;
        let stream = StreamName::parse(&ev_stream(&fx.local)).expect("event stream");
        let subject =
            Subject::parse(&format!("a2a.v1.{}.audit.{client}", fx.local)).expect("audit subject");
        let msg_id = MsgId::parse("req-0011-013-absent").expect("msg id");
        let refused = bus
            .append(
                &fx.client,
                &stream,
                &subject,
                &Headers::new(),
                Bytes::from_static(b"audit"),
                &msg_id,
            )
            .await;
        assert_eq!(refused.err(), Some(BusError::Absent));
    });
}

/// 0010-078 and 0011-040: a stream delivery's principal is the owner token of its
/// subject. A client-stamped `a2a-principal` header names someone else and is ignored;
/// it passes through to the consumer as an ordinary header.
#[test]
#[ignore = "live: needs nats-server"]
fn req_0011_040_forged_principal_header_is_ignored() {
    const FORGED_HEADER: &str = "a2a-principal";
    let rt = runtime();
    let (server, fx) = start(&rt);
    provision_local(&rt, &server, &fx);
    let client = fx.client.name().expect("client name").to_owned();
    let other = fx.server.name().expect("server name").to_owned();
    rt.block_on(async {
        let bus = connect_as(&server, &fx, &client).await;
        let mut forged = Headers::new();
        forged.insert(FORGED_HEADER, &other).expect("header");
        let subject = event_subject(&fx, &client);
        bus.publish(&fx.client, &subject, &forged, Bytes::from_static(b"event"))
            .await
            .expect("own event subject publishes");
        let stream = StreamName::parse(&ev_stream(&fx.local)).expect("stream name");
        let name = ConsumerName::parse("forged-check").expect("consumer name");
        let filter = SubjectPattern::parse(&format!("a2a.v1.{}.ev.{client}.>", fx.local.as_str()))
            .expect("filter");
        let mut consumer = bus
            .consume(
                &fx.client,
                &stream,
                &name,
                &filter,
                StartPosition::FromFirst,
            )
            .await
            .expect("consumer");
        let delivery = consumer.next().await.expect("delivery");
        assert_eq!(
            delivery.principal().and_then(|p| p.name()),
            Some(client.as_str())
        );
        assert_eq!(delivery.headers().get(FORGED_HEADER), Some(other.as_str()));
    });
}

#[test]
#[ignore = "live: needs nats-server"]
fn req_0010_024_provision_is_idempotent() {
    let rt = runtime();
    let (server, fx) = start(&rt);
    let config = NatsConfig {
        url: server.url().to_owned(),
        identity: SUPERVISOR.to_owned(),
        seed: Seed::new(server.seed(SUPERVISOR).expect("supervisor seed")),
        tenant: fx.local.clone(),
        request_timeout: fx.request_timeout,
        ack_wait: fx.ack_wait,
    };
    rt.block_on(async {
        let bus = connect(config).await.expect("supervisor connects");
        provision(&bus).await.expect("first provision");
        provision(&bus).await.expect("second provision");
    });
}

/// 0010-034: the snapshot ends with exactly one `SnapshotEnd`, every key
/// written before the watch appears in the snapshot, and every key written
/// around the watch appears exactly once across snapshot and live events.
#[test]
#[ignore = "live: needs nats-server"]
fn req_0010_034_watch_snapshot_then_live_without_gap() {
    const RACE_KEYS: usize = 20;
    const PRE_KEYS: usize = 5;
    let rt = runtime();
    let (server, fx) = start(&rt);
    let factory = NatsFactory::new(&server, rt.handle().clone(), fx.clock);
    let (bus, _control) = factory.create(&fx, SUITE_SEED, None).expect("bus");
    let bucket = fx.bucket_local.clone();
    let client = fx.client.clone();
    let prefix = KeyPrefix::parse("").expect("empty prefix");

    rt.block_on(async {
        for i in 0..PRE_KEYS {
            let key = Key::parse(&format!("pre{i}")).expect("key");
            bus.kv_put(&client, &bucket, &key, Bytes::from_static(b"pre"), None)
                .await
                .expect("pre put");
        }
        let writes = async {
            for i in 0..RACE_KEYS {
                let key = Key::parse(&format!("race{i}")).expect("key");
                bus.kv_put(&client, &bucket, &key, Bytes::from_static(b"race"), None)
                    .await
                    .expect("race put");
            }
        };
        let reads = async {
            let mut watch = bus
                .kv_watch(&client, &bucket, &prefix)
                .await
                .expect("watch");
            let mut seen: BTreeMap<String, u32> = BTreeMap::new();
            let mut snapshot_ends = 0usize;
            let mut snapshot_done = false;
            let total = PRE_KEYS + RACE_KEYS;
            while seen.len() < total || !snapshot_done {
                let event = tokio::time::timeout(Duration::from_secs(20), watch.next())
                    .await
                    .expect("watch event within 20 s")
                    .expect("watch next");
                assert!(
                    !matches!(event, WatchEvent::Delete { .. }),
                    "unexpected delete event"
                );
                match event {
                    WatchEvent::Put(entry) => {
                        *seen.entry(entry.key.as_str().to_owned()).or_default() += 1;
                    }
                    WatchEvent::SnapshotEnd => {
                        snapshot_ends += 1;
                        snapshot_done = true;
                        for i in 0..PRE_KEYS {
                            assert!(
                                seen.contains_key(&format!("pre{i}")),
                                "pre{i} missing from snapshot"
                            );
                        }
                    }
                    WatchEvent::Delete { .. } => {}
                }
            }
            assert_eq!(snapshot_ends, 1, "exactly one SnapshotEnd");
            assert_eq!(seen.len(), total, "every key delivered");
            for (key, count) in &seen {
                assert_eq!(*count, 1, "{key} delivered {count} times");
            }
        };
        tokio::join!(writes, reads);
    });
}

/// 0010-031/032/033: an expired TTL key is absent, `create` re-creates it, and
/// an update with the returned revision refreshes it.
#[test]
#[ignore = "live: needs nats-server"]
fn req_0010_031_ttl_expiry_then_create_recreates() {
    let rt = runtime();
    let (server, fx) = start(&rt);
    let factory = NatsFactory::new(&server, rt.handle().clone(), fx.clock);
    let (bus, _control) = factory.create(&fx, SUITE_SEED, None).expect("bus");
    let bucket = fx.bucket_local.clone();
    let client = fx.client.clone();
    let key = Key::parse("lease").expect("key");
    let ttl = Some(Duration::from_secs(1));

    rt.block_on(async {
        let first = bus
            .kv_create(&client, &bucket, &key, Bytes::from_static(b"v1"), ttl)
            .await
            .expect("first create");
        let live = bus
            .kv_create(&client, &bucket, &key, Bytes::from_static(b"v2"), ttl)
            .await;
        assert!(live.is_err(), "create on a live key must fail");

        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(
            matches!(
                bus.kv_get(&client, &bucket, &key).await,
                Err(BusError::Absent)
            ),
            "expired key must be absent"
        );

        let second = bus
            .kv_create(&client, &bucket, &key, Bytes::from_static(b"v3"), ttl)
            .await
            .expect("create after expiry");
        assert!(second > first, "re-created revision must advance");

        let third = bus
            .kv_update(
                &client,
                &bucket,
                &key,
                Bytes::from_static(b"v4"),
                second,
                None,
            )
            .await
            .expect("update with returned revision");
        let entry = bus
            .kv_get(&client, &bucket, &key)
            .await
            .expect("get after update");
        assert_eq!(entry.value, Bytes::from_static(b"v4"));
        assert_eq!(entry.revision, third);
    });
}

#[test]
#[ignore = "live: needs nats-server"]
fn req_0010_031_existing_presence_bucket_gains_message_ttl() {
    let rt = runtime();
    let (server, fx) = start(&rt);
    let presence = kv_bucket(&fx.local, PRESENCE);
    rt.block_on(async {
        let seed = server.seed(SUPERVISOR).expect("supervisor seed");
        let client = async_nats::ConnectOptions::with_nkey(seed)
            .name(SUPERVISOR)
            .custom_inbox_prefix(format!("_INBOX.{SUPERVISOR}"))
            .connect(server.url())
            .await
            .expect("supervisor client");
        let js = async_nats::jetstream::new(client);
        // The fixtures do not create the presence bucket; absence is fine.
        let _ = js.delete_stream(kv_stream(&presence)).await;
        js.create_key_value(async_nats::jetstream::kv::Config {
            bucket: presence.clone(),
            history: 1,
            storage: async_nats::jetstream::stream::StorageType::File,
            num_replicas: 1,
            ..Default::default()
        })
        .await
        .expect("legacy presence bucket");
        let legacy = js
            .get_stream(kv_stream(&presence))
            .await
            .expect("legacy stream");
        assert!(!legacy.cached_info().config.allow_message_ttl);

        let supervisor = connect_as(&server, &fx, SUPERVISOR).await;
        provision(&supervisor).await.expect("provision reconciles");

        let stream = js
            .get_stream(kv_stream(&presence))
            .await
            .expect("presence stream");
        let config = &stream.cached_info().config;
        assert!(config.allow_message_ttl, "presence must honour Nats-TTL");
        assert_eq!(
            config.subject_delete_marker_ttl,
            Some(Duration::from_secs(60))
        );
    });
}
