//! SP-3 / SP-5 probe for RFC-0010 (NATS binding) on Windows.
//!
//! Run from the repo root (see spikes/nats/README.md):
//!   cargo run --manifest-path spikes/nats/probe/Cargo.toml -- <case> [args]
//!
//! Seeds are read from .spikes/nats/keys/<id>.seed and are never printed.
//! Permission denials are logged by the server (.spikes/nats/logs/server.log),
//! not returned to the client, so each case prints what the client observed.

use async_nats::jetstream::{
    self,
    consumer::{push, AckPolicy, DeliverPolicy},
    kv,
};
use async_nats::{Client, ConnectOptions, HeaderMap, StatusCode};
use futures::StreamExt;
use std::error::Error;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::time::{sleep, timeout};

type Res<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

const URL: &str = "nats://127.0.0.1:4222";
const EV: &str = "A2AB_EV_local";
const CARDS: &str = "a2ab-local-cards";
const PRES: &str = "a2ab-local-presence";
const CLAIMS: &str = "a2ab-local-claims";

fn now_ns() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
}

/// Connects as an agent identity using its NKey seed (public key is in the server config).
/// `custom_inbox`: use the RFC-0010 [0010-066] inbox prefix `_INBOX.<id>`.
async fn connect_as(id: &str, custom_inbox: bool, request_timeout: Option<Duration>) -> Res<Client> {
    let seed = std::fs::read_to_string(format!(".spikes/nats/keys/{id}.seed"))?
        .trim()
        .to_string();
    let mut opts = ConnectOptions::with_nkey(seed).request_timeout(request_timeout);
    if custom_inbox {
        opts = opts.custom_inbox_prefix(format!("_INBOX.{id}"));
    }
    Ok(opts.connect(URL).await?)
}

async fn connect(id: &str) -> Res<Client> {
    connect_as(id, true, Some(Duration::from_secs(5))).await
}

fn show<T: std::fmt::Debug, E: std::fmt::Display>(
    r: Result<Result<T, E>, tokio::time::error::Elapsed>,
) -> String {
    match r {
        Err(_) => "timeout".into(),
        Ok(Ok(v)) => format!("ok {v:?}"),
        Ok(Err(e)) => format!("err: {e}"),
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Res {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let case = args.first().map(String::as_str).unwrap_or("");
    match case {
        "inbox-default" => inbox_default().await,
        "rpc" => rpc().await,
        "rpc-expiry" => rpc_expiry().await,
        "dedup" => dedup().await,
        "ttl" => ttl().await,
        "fence" => fence().await,
        "payload" => payload().await,
        "subjlen" => subjlen().await,
        "advisory" => advisory().await,
        "ttl-manual" => ttl_manual().await,
        "watch" => watch().await,
        "heartbeat" => heartbeat().await,
        #[cfg(windows)]
        "job" => job(args.get(1).map(String::as_str).unwrap_or(""), args.get(2).map(String::as_str).unwrap_or("")).await,
        #[cfg(not(windows))]
        "job" => Err("job case is Windows-only".into()),
        _ => Err(format!("usage: probe <inbox-default|rpc|rpc-expiry|dedup|ttl|ttl-manual|fence|payload|watch|heartbeat|subjlen|advisory|job> (got '{case}')").into()),
    }
}

/// [0010-066] default inbox prefix: client subscribes `_INBOX.<nuid>.*`, which the
/// omp read permissions do not cover. Expect a request that never completes.
async fn inbox_default() -> Res {
    let c = connect_as("omp", false, Some(Duration::from_secs(4))).await?;
    let r = c.request("a2a.v1.local.rpc.omp.none", "x".into()).await;
    println!("inbox-default: request with default inbox => {}", match r {
        Ok(m) => format!("ok {:?}", m.payload),
        Err(e) => format!("err: {e}"),
    });
    Ok(())
}

/// [0010-066] custom inbox; [0010-061] a second reply to one request is denied (allow_responses max:1).
/// Also: no-responders (503) when no agent subscribes to the target rpc subject.
async fn rpc() -> Res {
    let omp = connect("omp").await?;
    let tui = connect("tui").await?;
    let mut reqs = omp.subscribe("a2a.v1.local.rpc.omp.*").await?;
    let responder = omp.clone();
    let task = tokio::spawn(async move {
        if let Some(m) = reqs.next().await {
            let reply = m.reply.clone().expect("request has reply subject");
            let r1 = responder.publish(reply.clone(), "pong-1".into()).await;
            let _ = responder.flush().await;
            let r2 = responder.publish(reply, "pong-2".into()).await;
            let f2 = responder.flush().await;
            println!("rpc: responder first reply sent={} second reply sent={} flush2={:?}", r1.is_ok(), r2.is_ok(), f2.is_ok());
        }
    });
    let r = timeout(Duration::from_secs(4), tui.request("a2a.v1.local.rpc.omp.ping", "ping".into())).await;
    println!("rpc: tui->omp request => {}", show(r));
    let nr = tui.request("a2a.v1.local.rpc.claude.nobody", "x".into()).await;
    println!("rpc: request with no subscriber => {}", match nr {
        Ok(m) => format!("ok {:?}", m.payload),
        Err(e) => format!("err: {e}"),
    });
    let _ = timeout(Duration::from_secs(3), task).await;
    sleep(Duration::from_millis(500)).await;
    Ok(())
}

/// Section 4.11 allow_responses expires 2m after the request is received. Reply at +125 s
/// must be denied (the server logs a Publish Violation on the _INBOX reply subject).
/// Run in background; takes about 2m15s.
async fn rpc_expiry() -> Res {
    let omp = connect("omp").await?;
    let tui = connect_as("tui", true, Some(Duration::from_secs(135))).await?;
    let mut reqs = omp.subscribe("a2a.v1.local.rpc.omp.exp").await?;
    let responder = omp.clone();
    tokio::spawn(async move {
        if let Some(m) = reqs.next().await {
            let reply = m.reply.clone().expect("request has reply subject");
            sleep(Duration::from_secs(125)).await;
            let r = responder.publish(reply, "late".into()).await;
            let _ = responder.flush().await;
            println!("rpc-expiry: late reply client-side sent={}", r.is_ok());
        }
    });
    let r = tui.request("a2a.v1.local.rpc.omp.exp", "x".into()).await;
    println!("rpc-expiry: request answered at +125s => {}", match r {
        Ok(m) => format!("ok {:?}", m.payload),
        Err(e) => format!("err: {e}"),
    });
    Ok(())
}

/// [0010-026] EV stream duplicate window: same Nats-Msg-Id => duplicate=true, same seq.
async fn dedup() -> Res {
    let js = jetstream::new(connect("omp").await?);
    let id = format!("dedup-{}", now_ns());
    let mut h = HeaderMap::new();
    h.insert("Nats-Msg-Id", id.as_str());
    let a1 = js.publish_with_headers("a2a.v1.local.ev.omp.dedup", h.clone(), "a".into()).await?.await?;
    let a2 = js.publish_with_headers("a2a.v1.local.ev.omp.dedup", h, "b".into()).await?.await?;
    let mut h3 = HeaderMap::new();
    h3.insert("Nats-Msg-Id", format!("{id}-other").as_str());
    let a3 = js.publish_with_headers("a2a.v1.local.ev.omp.dedup", h3, "c".into()).await?.await?;
    println!("dedup: first seq={} dup={}", a1.sequence, a1.duplicate);
    println!("dedup: same-id seq={} dup={}", a2.sequence, a2.duplicate);
    println!("dedup: other-id seq={} dup={}", a3.sequence, a3.duplicate);
    Ok(())
}

/// [0010-031] per-key TTL on presence (limit markers enabled). Case A: a fresh key (never written)
/// created with create_with_ttl. Case B: a key whose previous entry was purged. async-nats 0.50.0
/// create_with_ttl falls back to a plain update() after a purge/delete, which drops Nats-TTL.
/// Expired key must stop returning a value; the server keeps a delete marker for the marker TTL.
async fn ttl() -> Res {
    let js = jetstream::new(connect("omp").await?);
    let pres = js.get_key_value(PRES).await?;
    // Case A uses the admin identity: omp may only write its own presence key (AgentUser), so a
    // fresh key name is denied for omp and the publish is dropped (no ack => timeout).
    let admin = jetstream::new(connect_as("admin", true, Some(Duration::from_secs(5))).await?);
    let apres = admin.get_key_value(PRES).await?;
    let fresh = format!("ttl-fresh-{}", now_ns());
    let r = apres.create_with_ttl(fresh.as_str(), "online".into(), Duration::from_secs(3)).await;
    println!("ttl: A fresh key (admin) create_with_ttl(3s) => {}", match &r { Ok(rev) => format!("ok rev={rev}"), Err(e) => format!("err: {e}") });
    sleep(Duration::from_secs(1)).await;
    let e1 = apres.entry(fresh.as_str()).await?;
    println!("ttl: A entry at +1s => {}", e1.map(|e| format!("op={:?} rev={}", e.operation, e.revision)).unwrap_or("none".into()));
    sleep(Duration::from_secs(6)).await;
    let e2 = apres.entry(fresh.as_str()).await?;
    println!("ttl: A entry at +7s => {}", e2.map(|e| format!("op={:?} rev={}", e.operation, e.revision)).unwrap_or("none".into()));

    let _ = pres.purge("omp").await;
    let r = pres.create_with_ttl("omp", "online".into(), Duration::from_secs(3)).await;
    println!("ttl: B purged key 'omp' create_with_ttl(3s) => {}", match &r { Ok(rev) => format!("ok rev={rev}"), Err(e) => format!("err: {e}") });
    sleep(Duration::from_secs(7)).await;
    let e3 = pres.entry("omp").await?;
    println!("ttl: B entry at +7s => {}", e3.map(|e| format!("op={:?} rev={}", e.operation, e.revision)).unwrap_or("none".into()));
    Ok(())
}

/// [0010-032/033] fencing on cards (key `omp`): create-on-existing and stale-revision update must fail.
async fn fence() -> Res {
    let js = jetstream::new(connect("omp").await?);
    let cards = js.get_key_value(CARDS).await?;
    let rev = cards.put("omp", r#"{"id":"omp","n":1}"#.into()).await?;
    println!("fence: put rev={rev}");
    let c = cards.create("omp", "x".into()).await;
    println!("fence: create on existing key => {}", match c { Ok(r) => format!("ok rev={r}"), Err(e) => format!("err: {e}") });
    let bad = cards.update("omp", "y".into(), rev + 1000).await;
    println!("fence: update with wrong revision => {}", match bad { Ok(r) => format!("ok rev={r}"), Err(e) => format!("err: {e}") });
    let good = cards.update("omp", "z".into(), rev).await;
    println!("fence: update with current revision => {}", match good { Ok(r) => format!("ok rev={r}"), Err(e) => format!("err: {e}") });
    let stale = cards.update("omp", "w".into(), rev).await;
    println!("fence: update with stale revision => {}", match stale { Ok(r) => format!("ok rev={r}"), Err(e) => format!("err: {e}") });
    Ok(())
}

/// [0010-014] max_payload = 1 MiB; [0010-015] sender budget. Core publish to a non-stream subject (no JetStream
/// limit involved). Each case uses a fresh connection because the server closes the
/// connection on a payload violation. Header block size is approximate (value + 21 bytes).
async fn payload() -> Res {
    let cases: [(&str, usize, usize); 4] = [
        ("body=1048575 no-headers", 1_048_575, 0),
        ("body=1048576 no-headers", 1_048_576, 0),
        ("body=1048000 headers~621", 1_048_000, 600),
        ("body=1046000 headers~1621", 1_046_000, 1_600),
    ];
    for (label, body, hdr) in cases {
        let c = connect("omp").await?;
        let max = c.server_info().max_payload;
        let payload = vec![b'x'; body];
        let sent = if hdr == 0 {
            c.publish("a2a.v1.local.rpc.omp.payload", payload.into()).await.map_err(|e| e.to_string())
        } else {
            let mut h = HeaderMap::new();
            h.insert("X-Pad", "a".repeat(hdr).as_str());
            c.publish_with_headers("a2a.v1.local.rpc.omp.payload", h, payload.into()).await.map_err(|e| e.to_string())
        };
        match sent {
            Err(e) => println!("payload: {label} max_payload={max} publish_err={e}"),
            Ok(()) => {
                let f = c.flush().await;
                println!("payload: {label} max_payload={max} flush_ok={}", f.is_ok());
            }
        }
        sleep(Duration::from_millis(300)).await;
    }
    Ok(())
}

/// [0010-034] initial-snapshot marker. Non-empty bucket (cards): which entry carries seen_current.
/// Empty bucket: a throwaway bucket created with admin credentials (deleted afterwards), and whether
/// any entry is delivered at all.
async fn watch() -> Res {
    let c = connect("omp").await?;
    let js = jetstream::new(c.clone());
    let cards = js.get_key_value(CARDS).await?;
    let mut w = cards.watch_all().await?;
    let mut n = 0;
    loop {
        match timeout(Duration::from_secs(5), w.next()).await {
            Err(_) => {
                println!("watch: cards next() timed out after 5s");
                break;
            }
            Ok(None) => {
                println!("watch: cards stream ended");
                break;
            }
            Ok(Some(Err(e))) => {
                println!("watch: cards error: {e}");
                break;
            }
            Ok(Some(Ok(e))) => {
                n += 1;
                println!("watch: cards entry key={} op={:?} delta={} seen_current={}", e.key, e.operation, e.delta, e.seen_current);
                if e.seen_current {
                    break;
                }
            }
        }
    }
    println!("watch: cards entries before/at marker={n}");
    match cards.get("omp").await {
        Ok(Some(b)) => println!("watch-diag: get(omp) value_bytes={}", b.len()),
        Ok(None) => println!("watch-diag: get(omp) none"),
        Err(e) => println!("watch-diag: get(omp) err: {e}"),
    }
    let deliver = c.new_inbox();
    let mut sub = c.subscribe(deliver.clone()).await?;
    let cfg = push::Config {
        deliver_subject: deliver,
        deliver_policy: DeliverPolicy::All,
        filter_subject: "$KV.a2ab-local-cards.>".into(),
        ack_policy: AckPolicy::None,
        ..Default::default()
    };
    match js.create_consumer_on_stream(cfg, "KV_a2ab-local-cards").await {
        Err(e) => println!("watch-diag: plain push consumer create err: {e}"),
        Ok(cons) => {
            let mut got = 0;
            while let Ok(Some(m)) = timeout(Duration::from_secs(3), sub.next()).await {
                if m.status.is_none() {
                    got += 1;
                }
            }
            println!("watch-diag: plain push consumer (DeliverPolicy::All) delivered={got}");
            let _ = js.get_stream("KV_a2ab-local-cards").await?.delete_consumer(&cons.cached_info().name).await;
        }
    }
    let d2 = c.new_inbox();
    let mut sub2 = c.subscribe(d2.clone()).await?;
    let cfg2 = push::Config {
        deliver_subject: d2,
        deliver_policy: DeliverPolicy::LastPerSubject,
        filter_subject: "$KV.a2ab-local-cards.>".into(),
        ack_policy: AckPolicy::None,
        ..Default::default()
    };
    match js.create_consumer_on_stream(cfg2, "KV_a2ab-local-cards").await {
        Err(e) => println!("watch-diag: LastPerSubject push consumer create err: {e}"),
        Ok(cons) => {
            let mut got = 0;
            while let Ok(Some(m)) = timeout(Duration::from_secs(3), sub2.next()).await {
                if m.status.is_none() {
                    got += 1;
                }
            }
            println!("watch-diag: plain push consumer (DeliverPolicy::LastPerSubject) delivered={got}");
            let _ = js.get_stream("KV_a2ab-local-cards").await?.delete_consumer(&cons.cached_info().name).await;
        }
    }
    let adm2 = jetstream::new(connect_as("admin", true, Some(Duration::from_secs(5))).await?);
    let ac = adm2.get_key_value(CARDS).await?;
    let mut aw = ac.watch_all().await?;
    let first = match timeout(Duration::from_secs(5), aw.next()).await {
        Err(_) => "timeout".to_string(),
        Ok(None) => "end".to_string(),
        Ok(Some(Err(e))) => format!("err {e}"),
        Ok(Some(Ok(e))) => format!("entry key={} seen_current={}", e.key, e.seen_current),
    };
    println!("watch-diag: admin watch_all first={first}");
    let d3 = c.new_inbox();
    let mut sub3 = c.subscribe(d3.clone()).await?;
    let cfg3 = push::Config {
        deliver_subject: d3,
        deliver_policy: DeliverPolicy::LastPerSubject,
        filter_subjects: vec!["$KV.a2ab-local-cards.>".into()],
        replay_policy: async_nats::jetstream::consumer::ReplayPolicy::Instant,
        flow_control: true,
        idle_heartbeat: Duration::from_secs(5),
        ack_policy: AckPolicy::None,
        ..Default::default()
    };
    match js.create_consumer_on_stream(cfg3, "KV_a2ab-local-cards").await {
        Err(e) => println!("watch-diag: watch-config push consumer create err: {e}"),
        Ok(cons) => {
            let mut got = 0;
            while let Ok(Some(m)) = timeout(Duration::from_secs(3), sub3.next()).await {
                if m.status.is_none() {
                    got += 1;
                }
            }
            println!("watch-diag: watch-config push consumer (filter_subjects, Instant, FC, HB5s) delivered={got}");
            let _ = js.get_stream("KV_a2ab-local-cards").await?.delete_consumer(&cons.cached_info().name).await;
        }
    }
    for delay_ms in [0u64, 1000] {
        let d4 = c.new_inbox();
        let cfg4 = push::Config {
            deliver_subject: d4.clone(),
            deliver_policy: DeliverPolicy::LastPerSubject,
            filter_subject: "$KV.a2ab-local-cards.>".into(),
            ack_policy: AckPolicy::None,
            ..Default::default()
        };
        match js.create_consumer_on_stream(cfg4, "KV_a2ab-local-cards").await {
            Err(e) => println!("watch-diag: create-then-subscribe create err: {e}"),
            Ok(cons) => {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                let mut sub4 = c.subscribe(d4).await?;
                let mut got = 0;
                while let Ok(Some(m)) = timeout(Duration::from_secs(3), sub4.next()).await {
                    if m.status.is_none() {
                        if let Some(r) = &m.reply {
                            let s = r.to_string();
                            let t: Vec<&str> = s.split('.').collect();
                            println!("watch-diag: delay={delay_ms}ms stream_seq={} pending={}", t[t.len() - 4], t[t.len() - 1]);
                        }
                        got += 1;
                    }
                }
                println!("watch-diag: create-then-subscribe delay={delay_ms}ms delivered={got}");
                let _ = js.get_stream("KV_a2ab-local-cards").await?.delete_consumer(&cons.cached_info().name).await;
            }
        }
    }

    let admin = jetstream::new(connect_as("admin", true, Some(Duration::from_secs(5))).await?);
    let empty = admin
        .create_key_value(kv::Config { bucket: "a2ab-spike-empty".into(), history: 1, ..Default::default() })
        .await?;
    let mut w2 = empty.watch_all().await?;
    match timeout(Duration::from_secs(5), w2.next()).await {
        Err(_) => println!("watch: empty bucket => no entry (no initial marker) within 5s"),
        Ok(x) => println!("watch: empty bucket => {:?}", x.map(|r| r.map(|e| (e.key, e.seen_current)).map_err(|e| e.to_string()))),
    }
    drop(w2);
    admin.delete_key_value("a2ab-spike-empty").await?;
    Ok(())
}

/// Spike checklist item: ordered/push consumer idle heartbeat behaviour. Counts heartbeat status
/// messages on an idle EV consumer with a 2s idle_heartbeat.
async fn heartbeat() -> Res {
    let c = connect("omp").await?;
    let js = jetstream::new(c.clone());
    let deliver = c.new_inbox();
    let mut sub = c.subscribe(deliver.clone()).await?;
    let cfg = push::Config {
        deliver_subject: deliver.clone(),
        idle_heartbeat: Duration::from_secs(2),
        flow_control: true,
        filter_subject: "a2a.v1.local.ev.omp.>".into(),
        deliver_policy: DeliverPolicy::New,
        ack_policy: AckPolicy::None,
        ..Default::default()
    };
    let consumer = js.create_consumer_on_stream(cfg, EV).await?;
    let name = consumer.cached_info().name.clone();
    let t0 = Instant::now();
    let mut beats = Vec::new();
    let deadline = t0 + Duration::from_secs(7);
    while let Ok(Some(m)) = timeout(deadline.saturating_duration_since(Instant::now()), sub.next()).await {
        if m.status == Some(StatusCode::IDLE_HEARTBEAT) {
            beats.push(t0.elapsed().as_secs_f32());
        }
    }
    println!("heartbeat: idle_heartbeat=2s heartbeats_in_7s={} at_s={:?}", beats.len(), beats);
    js.get_stream(EV).await?.delete_consumer(&name).await?;
    Ok(())
}

/// [0010-031/032] workaround check: async-nats `create_with_ttl` drops Nats-TTL on a purged key (see `ttl`).
/// The admin identity puts the key after a purge with an explicit `Nats-TTL: 3s` header and the RFC fence
/// `Nats-Expected-Last-Subject-Sequence` = purge revision, then checks that the key expires.
async fn ttl_manual() -> Res {
    let admin = jetstream::new(connect_as("admin", true, Some(Duration::from_secs(5))).await?);
    let apres = admin.get_key_value(PRES).await?;
    let key = format!("ttl-manual-{}", now_ns());
    apres.create(key.as_str(), "online".into()).await?;
    apres.purge(key.as_str()).await?;
    let purge_rev = apres.entry(key.as_str()).await?.map(|e| e.revision).unwrap_or(0);
    let rv = purge_rev.to_string();
    let mut h = HeaderMap::new();
    h.insert("Nats-TTL", "3s");
    h.insert("Nats-Expected-Last-Subject-Sequence", rv.as_str());
    let ack = admin
        .publish_with_headers(format!("$KV.{PRES}.{key}"), h, "online".into())
        .await?
        .await?;
    println!("ttl-manual: put after purge with explicit Nats-TTL ok seq={} purge_rev={purge_rev}", ack.sequence);
    let e1 = apres.entry(key.as_str()).await?;
    println!("ttl-manual: +0s op={:?}", e1.map(|e| e.operation));
    sleep(Duration::from_secs(7)).await;
    let e2 = apres.entry(key.as_str()).await?;
    println!("ttl-manual: +7s op={:?}", e2.map(|e| e.operation));
    Ok(())
}

/// [0010-020] subject length. Admin identity (publish and subscribe on all subjects) publishes and receives on
/// subjects of increasing length, to tell the server's own limit from the RFC's 255-byte cap.
async fn subjlen() -> Res {
    let c = connect_as("admin", false, Some(Duration::from_secs(5))).await?;
    for len in [130usize, 255, 256, 300, 1000] {
        let prefix = "a2a.v1.local.rpc.admin.";
        let subj = format!("{prefix}{}", "x".repeat(len - prefix.len()));
        let mut sub = c.subscribe(subj.clone()).await?;
        c.publish(subj.clone(), "hi".into()).await?;
        c.flush().await?;
        let got = timeout(Duration::from_secs(3), sub.next()).await;
        println!("subjlen: subject_bytes={} delivered={}", subj.len(), matches!(got, Ok(Some(_))));
    }
    Ok(())
}

/// [0010-058] / open item 7: max-deliveries advisory subject. Admin creates a throwaway stream
/// A2AB_SPIKE_ADV with a push consumer (max_deliver 1, ack_wait 1s, explicit ack). The message is not acked,
/// so the server must emit an advisory instead of redelivering. Subscribes to $JS.EVENT.ADVISORY.> first.
async fn advisory() -> Res {
    let c = connect_as("admin", false, Some(Duration::from_secs(5))).await?;
    let js = jetstream::new(c.clone());
    let mut adv = c.subscribe("$JS.EVENT.ADVISORY.>").await?;
    let _ = js.delete_stream("A2AB_SPIKE_ADV").await;
    js.create_stream(jetstream::stream::Config {
        name: "A2AB_SPIKE_ADV".into(),
        subjects: vec!["a2a.spike.adv.>".into()],
        ..Default::default()
    })
    .await?;
    let d = c.new_inbox();
    let mut ds = c.subscribe(d.clone()).await?;
    let _cons = js
        .create_consumer_on_stream(
            push::Config {
                deliver_subject: d,
                durable_name: Some("adv".into()),
                ack_policy: AckPolicy::Explicit,
                max_deliver: 1,
                ack_wait: Duration::from_secs(1),
                ..Default::default()
            },
            "A2AB_SPIKE_ADV",
        )
        .await?;
    js.publish("a2a.spike.adv.x", "m".into()).await?.await?;
    let got = timeout(Duration::from_secs(3), ds.next()).await;
    println!("advisory: first delivery received={}", matches!(got, Ok(Some(_))));
    sleep(Duration::from_secs(4)).await;
    let mut n = 0;
    while let Ok(Some(m)) = timeout(Duration::from_secs(3), adv.next()).await {
        println!("advisory: subject={} payload_bytes={}", m.subject, m.payload.len());
        n += 1;
        if n >= 10 {
            break;
        }
    }
    println!("advisory: advisories_received={n}");
    let _ = js.delete_stream("A2AB_SPIKE_ADV").await;
    Ok(())
}

#[cfg(windows)]
async fn job(mode: &str, store: &str) -> Res {
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Console::{GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
        JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    let srv = ".spikes/nats/bin/srv/nats-server-v2.15.2-windows-amd64/nats-server.exe";
    let dir = ".spikes/nats/q1";
    std::fs::create_dir_all(dir)?;
    let conf = format!("{dir}/nats-q1.conf");
    std::fs::write(
        &conf,
        format!("listen: 127.0.0.1:4223\njetstream {{\n  store_dir: \"{store}\"\n  max_memory_store: 64MB\n  max_file_store: 256MB\n}}\n"),
    )?;
    let log = format!("{dir}/{mode}.log");
    let pid = format!("{dir}/{mode}.pid");

    let mut cmd = Command::new(srv);
    cmd.args(["-c", &conf, "-l", &log, "-P", &pid]);
    if mode == "ctrlbreak" {
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;

    let job = if mode == "contained" {
        // SAFETY: plain Win32 calls with valid, locally owned handles; the job handle is closed below.
        unsafe {
            let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let set = SetInformationJobObject(
                h,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            let assigned = AssignProcessToJobObject(h, child.as_raw_handle() as _);
            println!("job: CreateJobObject ok={} KILL_ON_JOB_CLOSE set={} assigned={}", !h.is_null(), set != 0, assigned != 0);
            Some(h)
        }
    } else {
        None
    };

    let t0 = Instant::now();
    let addr = "127.0.0.1:4223".parse()?;
    let up = loop {
        if std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
            break true;
        }
        if t0.elapsed() > Duration::from_secs(20) || child.try_wait()?.is_some() {
            break false;
        }
        sleep(Duration::from_millis(200)).await;
    };
    println!("{mode}: pid={} listening={up} after={:?} store={store}", child.id(), t0.elapsed());
    if up {
        let nc = async_nats::connect("nats://127.0.0.1:4223").await?;
        let js = jetstream::new(nc);
        let s = js
            .create_stream(async_nats::jetstream::stream::Config {
                name: "Q1".into(),
                subjects: vec!["q1.>".into()],
                storage: async_nats::jetstream::stream::StorageType::File,
                ..Default::default()
            })
            .await?;
        let ack = js.publish("q1.x", "hello".into()).await?.await?;
        println!("{mode}: file-store stream Q1 created; publish seq={} state_messages={}", ack.sequence, s.cached_info().state.messages);
    }

    match mode {
        "contained" => {
            // SAFETY: the handle came from CreateJobObjectW above and is closed exactly once.
            unsafe { CloseHandle(job.expect("job handle")) };
            let st = wait_exit(&mut child, Duration::from_secs(15)).await;
            println!("contained: job handle closed while probe alive => child_exit={st:?} port_open={}", port_open());
        }
        "kill" => {
            child.kill()?;
            let st = child.wait()?;
            println!("kill: TerminateProcess => exit={:?} port_open={}", st.code(), port_open());
        }
        "ctrlbreak" => {
            // SAFETY: signals only the child's process group created with CREATE_NEW_PROCESS_GROUP.
            let ok = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, child.id()) };
            let st = wait_exit(&mut child, Duration::from_secs(15)).await;
            println!("ctrlbreak: GenerateConsoleCtrlEvent ok={} child_exit={st:?} port_open={}", ok != 0, port_open());
        }
        other => return Err(format!("unknown job mode '{other}' (contained|kill|ctrlbreak)").into()),
    }
    if mode != "contained" {
        let _ = child.try_wait();
    }
    Ok(())
}

#[cfg(windows)]
async fn wait_exit(child: &mut std::process::Child, max: Duration) -> Option<i32> {
    let t0 = Instant::now();
    while t0.elapsed() < max {
        if let Ok(Some(st)) = child.try_wait() {
            return st.code();
        }
        sleep(Duration::from_millis(100)).await;
    }
    None
}

#[cfg(windows)]
fn port_open() -> bool {
    std::net::TcpStream::connect_timeout(&"127.0.0.1:4223".parse().unwrap(), Duration::from_millis(300)).is_ok()
}
