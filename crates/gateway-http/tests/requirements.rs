//! RFC-0008 requirements exercised through the gateway router, plus configuration checks.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use a2a_core::envelope::RequestBody;
use a2a_core::subject::Op;
use a2a_types::ids::AgentId;
use a2a_types::proto::a2a as pb;
use axum::http::StatusCode;
use bus_api::bus::{Bus, RequestStream};
use common::{
    Call, Fixture, REMOTE, card_value, message, peer, presence_json, send, send_reply, task,
};
use gateway_http::{ConfigError, GatewayConfig, RatePolicy, TlsConfig};
use serde_json::{Value, json};

const CARD: &str = "/agents/worker-a/.well-known/agent-card.json";
const RPC: &str = "/agents/worker-a";
const PROVENANCE: &str = "urn:a2abus:ext:provenance:1";
const SEQUENCE: &str = "urn:a2abus:ext:sequence:1";
const APPROVAL: &str = "urn:a2abus:ext:approval:grant:1";

/// A `SendMessage` body that returns at once, so no event subscription is needed.
fn send_immediately(text: &str) -> Value {
    json!({
        "jsonrpc": "2.0", "id": 1, "method": "SendMessage",
        "params": {
            "message": message(text),
            "configuration": {"returnImmediately": true}
        }
    })
}

fn rpc(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})
}

// [0008-001] The default listener is loopback on port 9910.
#[test]
fn req_0008_001_default_listener_is_loopback_on_9910() {
    let config = GatewayConfig::default();
    assert!(config.listen.ip().is_loopback());
    assert_eq!(config.listen.port(), 9910);
    assert_eq!(config.validate(), Ok(()));
}

// [0008-002] A non-loopback listener needs TLS, a peer, and a trusted peer.
#[test]
fn req_0008_002_non_loopback_listener_needs_tls_peers_and_trusted_peers() {
    let mut config = GatewayConfig {
        listen: "0.0.0.0:9910".parse().expect("addr"),
        expose: vec![AgentId::parse("worker-a").expect("id")],
        ..GatewayConfig::default()
    };
    assert_eq!(config.validate(), Err(ConfigError::NonLoopbackWithoutTls));
    config.tls = Some(TlsConfig {
        cert_path: "cert.pem".into(),
        key_path: "key.pem".into(),
    });
    assert_eq!(config.validate(), Err(ConfigError::NonLoopbackWithoutPeers));
    config.peers.push(peer("peer-a", "secret-token-a"));
    assert_eq!(
        config.validate(),
        Err(ConfigError::NonLoopbackWithoutTrustedPeers)
    );
    config.trusted_peers.insert("peer-a".to_owned());
    assert_eq!(config.validate(), Ok(()));
}

// [0008-003] The gateway agent id is never exposed.
#[test]
fn req_0008_003_gateway_agent_is_never_exposed() {
    let config = GatewayConfig {
        expose: vec![AgentId::gateway()],
        ..GatewayConfig::default()
    };
    assert_eq!(config.validate(), Err(ConfigError::ExposesGateway));
    assert!(!GatewayConfig::default().is_exposed(&AgentId::gateway()));
}

// [0008-004] A peer may only reach the agents listed for it.
#[test]
fn req_0008_004_peer_agents_must_be_exposed() {
    let config = GatewayConfig {
        peers: vec![peer("peer-a", "secret-token-a")],
        ..GatewayConfig::default()
    };
    assert_eq!(config.validate(), Err(ConfigError::PeerAgentNotExposed));
}

// [0008-009] Proxy mode refuses loopback anonymous access.
#[test]
fn req_0008_009_proxy_mode_refuses_loopback_anonymous_access() {
    let proxied = GatewayConfig {
        behind_proxy: true,
        ..GatewayConfig::default()
    };
    assert_eq!(
        proxied.validate(),
        Err(ConfigError::ProxyWithLoopbackAnonymous)
    );
    let hardened = GatewayConfig {
        behind_proxy: true,
        loopback_anonymous: false,
        ..GatewayConfig::default()
    };
    assert_eq!(hardened.validate(), Ok(()));
}

// [0008-005] CORS is off: an Origin header is refused with 403 and no CORS headers are emitted.
#[tokio::test]
async fn req_0008_005_origin_header_is_refused_without_cors() {
    let fx = Fixture::new();
    fx.seed_worker().await;
    let (_gw, app) = fx.app(fx.config()).await;
    let reply = send(
        &app,
        Call::get(CARD).header("origin", "https://evil.example"),
    )
    .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert!(
        reply
            .headers
            .keys()
            .all(|name| !name.as_str().starts_with("access-control-")),
        "no Access-Control headers"
    );
}

// [0008-006] On a loopback listener the Host header must name the bound address.
#[tokio::test]
async fn req_0008_006_foreign_host_is_refused_and_loopback_host_accepted() {
    let fx = Fixture::new();
    fx.seed_worker().await;
    let (_gw, app) = fx.app(fx.config()).await;
    let refused = send(&app, Call::get(CARD).host("evil.example")).await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    let accepted = send(&app, Call::get(CARD).host("localhost:9910")).await;
    assert_eq!(accepted.status, StatusCode::OK);
}

// [0008-010] JSON-RPC POSTs are accepted at the agent path; other sub-paths are not routed.
#[tokio::test]
async fn req_0008_010_agent_path_routes_rpc_and_extra_segments_do_not() {
    let fx = Fixture::new();
    fx.seed_worker().await;
    let (_gw, app) = fx.app(fx.config()).await;
    let routed = send(&app, Call::new("POST", RPC).body(b"not json".to_vec())).await;
    assert_eq!(
        routed.status,
        StatusCode::BAD_REQUEST,
        "parse error, routed"
    );
    let unrouted = send(
        &app,
        Call::new("POST", "/agents/worker-a/extra").body(b"not json".to_vec()),
    )
    .await;
    assert_eq!(unrouted.status, StatusCode::NOT_FOUND);
}

// [0008-011] The card is public, carries an ETag, and answers 304 on a matching If-None-Match.
#[tokio::test]
async fn req_0008_011_card_is_public_with_etag_and_conditional_get() {
    let fx = Fixture::new();
    fx.seed_worker().await;
    let (_gw, app) = fx.app(fx.config()).await;
    let first = send(&app, Call::get(CARD).from(REMOTE)).await;
    assert_eq!(first.status, StatusCode::OK, "no token needed for the card");
    let etag = first.header("etag").expect("etag header");
    let second = send(&app, Call::get(CARD).header("if-none-match", &etag)).await;
    assert_eq!(second.status, StatusCode::NOT_MODIFIED);
}

// [0008-012] The root path serves the default agent; without one it is not found.
#[tokio::test]
async fn req_0008_012_root_path_uses_default_agent_only_when_configured() {
    let fx = Fixture::new();
    fx.seed_worker().await;
    let (_gw, app) = fx.app(fx.config()).await;
    let root_card = send(&app, Call::get("/.well-known/agent-card.json")).await;
    assert_eq!(root_card.status, StatusCode::NOT_FOUND);
    let root_post = send(&app, Call::post("/", &json!({}))).await;
    assert_eq!(root_post.status, StatusCode::NOT_FOUND);
    assert!(root_post.text().contains("NO_AGENT_SELECTED"));

    let config = GatewayConfig {
        default_agent: Some(fx.worker_id.clone()),
        ..fx.config()
    };
    let (_gw, app) = fx.app(config).await;
    let root = send(&app, Call::get("/.well-known/agent-card.json")).await;
    let agent = send(&app, Call::get(CARD)).await;
    assert_eq!(root.status, StatusCode::OK);
    assert_eq!(
        root.body, agent.body,
        "root card is the default agent's card"
    );
}

// [0008-013] The card declares the bearer scheme, streaming, and push capability.
#[tokio::test]
async fn req_0008_013_card_declares_bearer_scheme_and_capabilities() {
    let fx = Fixture::new();
    fx.seed_worker().await;
    let (_gw, app) = fx.app(fx.config()).await;
    let card = send(&app, Call::get(CARD)).await.json();
    assert_eq!(
        card["securitySchemes"]["bearer"]["httpAuthSecurityScheme"]["scheme"],
        "Bearer"
    );
    assert_eq!(card["capabilities"]["streaming"], true);
    assert_eq!(card["capabilities"]["pushNotifications"], true);
    assert_eq!(card["capabilities"]["extendedAgentCard"], false);
    assert_eq!(card["name"], "Worker A");
    assert_eq!(card["version"], "1.0.0");
}

// [0008-014] The card lists exactly one JSONRPC interface at the public URL, with no tenant.
#[tokio::test]
async fn req_0008_014_card_has_one_jsonrpc_interface_without_tenant() {
    let fx = Fixture::new();
    fx.seed_worker().await;
    let (_gw, app) = fx.app(fx.config()).await;
    let card = send(&app, Call::get(CARD)).await.json();
    let interfaces = card["supportedInterfaces"].as_array().expect("interfaces");
    assert_eq!(interfaces.len(), 1);
    assert_eq!(
        interfaces[0]["url"],
        "http://127.0.0.1:9910/agents/worker-a"
    );
    assert_eq!(interfaces[0]["protocolBinding"], "JSONRPC");
    assert!(interfaces[0].get("tenant").is_none());
}

// [0008-015] The card carries no bus addresses or subjects.
#[tokio::test]
async fn req_0008_015_card_exposes_no_bus_addresses() {
    let fx = Fixture::new();
    fx.seed_worker().await;
    let (_gw, app) = fx.app(fx.config()).await;
    let body = send(&app, Call::get(CARD)).await.text();
    assert!(!body.contains("a2a.v1"), "no bus subject: {body}");
    assert!(!body.contains("nats"), "no bus URL: {body}");
    assert!(!body.contains(".rpc."), "no rpc subject: {body}");
}

// [0008-016] The derived card is unsigned: a bus card's signatures are removed.
#[tokio::test]
async fn req_0008_016_card_drops_bus_signatures() {
    let fx = Fixture::new();
    let mut card = card_value("1.0.0", None);
    card["signatures"] = json!([{"protected": "eyJ", "signature": "AAAA"}]);
    fx.kv_put(
        "cards",
        "worker-a",
        &serde_json::to_vec(&card).expect("json"),
    )
    .await;
    fx.kv_put(
        "presence",
        "worker-a",
        &presence_json("PRESENCE_STATE_READY"),
    )
    .await;
    let (_gw, app) = fx.app(fx.config()).await;
    let served = send(&app, Call::get(CARD)).await.json();
    assert!(served.get("signatures").is_none(), "signatures removed");
}

// [0008-017] An exposed agent without a bus card, or an unexposed agent, is 404 (not 503).
#[tokio::test]
async fn req_0008_017_unexposed_agent_card_is_not_found() {
    let fx = Fixture::new();
    fx.seed_worker().await;
    fx.kv_put("cards", "other", &common::card_json("1.0.0"))
        .await;
    let (_gw, app) = fx.app(fx.config()).await;
    let unexposed = send(&app, Call::get("/agents/other/.well-known/agent-card.json")).await;
    assert_eq!(unexposed.status, StatusCode::NOT_FOUND);
    let missing = send(
        &app,
        Call::get("/agents/missing/.well-known/agent-card.json"),
    )
    .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

// [0008-018] Cards come from the in-memory cache: the bus being closed does not affect them.
#[tokio::test]
async fn req_0008_018_card_is_served_from_cache_when_bus_is_closed() {
    let fx = Fixture::new();
    fx.seed_worker().await;
    let (_gw, app) = fx.app(fx.config()).await;
    fx.bus.close().await.expect("close bus");
    let reply = send(&app, Call::get(CARD)).await;
    assert_eq!(reply.status, StatusCode::OK);
}

// [0008-019] GetExtendedAgentCard without credentials is 401 with a bearer challenge.
#[tokio::test]
async fn req_0008_019_extended_card_without_credentials_is_unauthorized() {
    let fx = Fixture::new();
    fx.seed_worker().await;
    let (_gw, app) = fx.app(fx.config()).await;
    let reply = send(
        &app,
        Call::post(RPC, &rpc("GetExtendedAgentCard", json!({}))),
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert!(
        reply
            .header("www-authenticate")
            .is_some_and(|value| value.starts_with("Bearer")),
        "bearer challenge"
    );
}

// [0008-019] A peer whose agent card has no extended card gets an error, not a card.
#[tokio::test]
async fn req_0008_019_extended_card_for_peer_without_extended_card_is_an_error() {
    let fx = Fixture::new();
    fx.seed_worker().await;
    let config = GatewayConfig {
        peers: vec![peer("peer-a", "secret-token-a")],
        trusted_peers: ["peer-a".to_owned()].into_iter().collect(),
        ..fx.config()
    };
    let (_gw, app) = fx.app(config).await;
    let reply = send(
        &app,
        Call::post(RPC, &rpc("GetExtendedAgentCard", json!({})))
            .from(REMOTE)
            .bearer("secret-token-a"),
    )
    .await;
    assert!(reply.json().get("result").is_none(), "no extended card");
    assert!(reply.json().get("error").is_some(), "error object");
}

// [0008-020] Unauthenticated requests fail before parsing; a malformed body publishes nothing.
#[tokio::test]
async fn req_0008_020_unauthenticated_malformed_request_is_401_and_not_published() {
    let fx = Fixture::new();
    fx.seed_worker().await;
    let config = GatewayConfig {
        peers: vec![peer("peer-a", "secret-token-a")],
        trusted_peers: ["peer-a".to_owned()].into_iter().collect(),
        ..fx.config()
    };
    let (_gw, app) = fx.app(config).await;
    let mut requests = fx.serve(Op::Send).await;
    let reply = send(
        &app,
        Call::new("POST", RPC)
            .from(REMOTE)
            .body(b"{not json".to_vec()),
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    let published = tokio::time::timeout(Duration::from_millis(150), requests.next()).await;
    assert!(published.is_err(), "nothing reached the worker");
}

// [0008-021] A wrong peer token is 401 with a bearer challenge.
#[tokio::test]
async fn req_0008_021_wrong_peer_token_is_401_with_challenge() {
    let fx = Fixture::new();
    fx.seed_worker().await;
    let config = GatewayConfig {
        peers: vec![peer("peer-a", "secret-token-a")],
        trusted_peers: ["peer-a".to_owned()].into_iter().collect(),
        ..fx.config()
    };
    let (_gw, app) = fx.app(config).await;
    let reply = send(
        &app,
        Call::post(RPC, &send_immediately("hi"))
            .from(REMOTE)
            .bearer("not-the-token"),
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert!(reply.header("www-authenticate").is_some());
}

// [0008-022] A loopback socket with a token it does not hold is refused, not anonymous.
#[tokio::test]
async fn req_0008_022_loopback_invalid_token_is_refused() {
    let fx = Fixture::new();
    fx.seed_worker().await;
    let (_gw, app) = fx.app(fx.config()).await;
    let reply = send(
        &app,
        Call::post(RPC, &send_immediately("hi")).bearer("nope"),
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert!(reply.header("www-authenticate").is_some());
}

// [0008-023] Failed authentications are rate limited per source address: 429 with Retry-After.
#[tokio::test]
async fn req_0008_023_failed_auth_is_rate_limited_per_source() {
    let fx = Fixture::new();
    fx.seed_worker().await;
    let config = GatewayConfig {
        peers: vec![peer("peer-a", "secret-token-a")],
        trusted_peers: ["peer-a".to_owned()].into_iter().collect(),
        failed_auth_rate: RatePolicy {
            limit: 1,
            window: Duration::from_secs(60),
        },
        ..fx.config()
    };
    let (_gw, app) = fx.app(config).await;
    let first = send(
        &app,
        Call::post(RPC, &send_immediately("hi"))
            .from(REMOTE)
            .bearer("wrong"),
    )
    .await;
    assert_eq!(first.status, StatusCode::UNAUTHORIZED);
    let second = send(
        &app,
        Call::post(RPC, &send_immediately("hi"))
            .from(REMOTE)
            .bearer("wrong"),
    )
    .await;
    assert_eq!(second.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(second.header("retry-after").is_some(), "Retry-After set");
}

// [0008-024] A valid peer not on the trusted list is 403 PEER_NOT_TRUSTED (-32052).
#[tokio::test]
async fn req_0008_024_untrusted_peer_is_forbidden() {
    let fx = Fixture::new();
    fx.seed_worker().await;
    let config = GatewayConfig {
        peers: vec![peer("peer-a", "secret-token-a")],
        ..fx.config()
    };
    let (_gw, app) = fx.app(config).await;
    let reply = send(
        &app,
        Call::post(RPC, &send_immediately("hi"))
            .from(REMOTE)
            .bearer("secret-token-a"),
    )
    .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(reply.error_code(), -32052);
    assert_eq!(reply.error_reason(), "PEER_NOT_TRUSTED");
}

// [0008-025] A non-JSON Content-Type is 415 with -32600 UNSUPPORTED_MEDIA_TYPE.
#[tokio::test]
async fn req_0008_025_non_json_content_type_is_415() {
    let fx = Fixture::new();
    fx.seed_worker().await;
    let (_gw, app) = fx.app(fx.config()).await;
    let reply = send(
        &app,
        Call::post(RPC, &send_immediately("hi")).content_type(Some("text/plain")),
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(reply.error_code(), -32600);
    assert_eq!(reply.error_reason(), "UNSUPPORTED_MEDIA_TYPE");
}

// [0008-026] A body over 1 MiB is 413, rejected before parsing.
#[tokio::test]
async fn req_0008_026_oversized_body_is_413() {
    let fx = Fixture::new();
    fx.seed_worker().await;
    let (_gw, app) = fx.app(fx.config()).await;
    let body = vec![b' '; (1 << 20) + 1];
    let reply = send(&app, Call::new("POST", RPC).body(body)).await;
    assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
}

// [0008-027] An absent or unsupported A2A-Version is -32009.
#[tokio::test]
async fn req_0008_027_absent_or_unsupported_version_is_32009() {
    let fx = Fixture::new();
    fx.seed_worker().await;
    let (_gw, app) = fx.app(fx.config()).await;
    let absent = send(&app, Call::post(RPC, &send_immediately("hi")).version(None)).await;
    assert_eq!(absent.error_code(), -32009);
    let unsupported = send(
        &app,
        Call::post(RPC, &send_immediately("hi")).version(Some("0.3")),
    )
    .await;
    assert_eq!(unsupported.error_code(), -32009);
}

// [0008-027] A-Version 1.0 and 1.0.0 are both served.
#[tokio::test]
async fn req_0008_027_versions_1_0_and_1_0_0_are_served() {
    let fx = Fixture::new();
    fx.seed_worker().await;
    let (_gw, app) = fx.app(fx.config()).await;
    let mut requests = fx.serve(Op::Get).await;
    let get = |version: &str| {
        Call::post(RPC, &rpc("GetTask", json!({"id": "t-1"}))).version(Some(version))
    };
    let worker = async {
        for _ in 0..2 {
            let inbound = fx.next_inbound(&mut requests).await;
            fx.reply(inbound, &task("t-1", pb::TaskState::Working));
        }
    };
    let (first, second, ()) =
        tokio::join!(send(&app, get("1.0")), send(&app, get("1.0.0")), worker);
    for reply in [first, second] {
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.json()["result"]["id"], "t-1");
    }
}

// [0008-028] A required bus extension that the request does not list is -32008.
#[tokio::test]
async fn req_0008_028_missing_required_extension_is_32008() {
    let fx = Fixture::new();
    let caps = json!({"extensions": [{"uri": "urn:example:required", "required": true}]});
    fx.kv_put(
        "cards",
        "worker-a",
        &serde_json::to_vec(&card_value("1.0.0", Some(caps))).expect("json"),
    )
    .await;
    fx.kv_put(
        "presence",
        "worker-a",
        &presence_json("PRESENCE_STATE_READY"),
    )
    .await;
    let (_gw, app) = fx.app(fx.config()).await;
    let reply = send(&app, Call::post(RPC, &send_immediately("hi"))).await;
    assert_eq!(reply.error_code(), -32008);
}

// [0008-029] A pre-1.0 method alias routes to the canonical operation.
#[tokio::test]
async fn req_0008_029_method_alias_routes_to_canonical_operation() {
    let fx = Fixture::new();
    fx.seed_worker().await;
    let (_gw, app) = fx.app(fx.config()).await;
    let mut requests = fx.serve(Op::Get).await;
    let worker = async {
        let inbound = fx.next_inbound(&mut requests).await;
        assert!(matches!(inbound.body, RequestBody::GetTask(_)));
        fx.reply(inbound, &task("t-9", pb::TaskState::Completed));
    };
    let (reply, ()) = tokio::join!(
        send(
            &app,
            Call::post(RPC, &rpc("tasks/get", json!({"id": "t-9"})))
        ),
        worker
    );
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json()["result"]["id"], "t-9");
}

// [0008-030] A pre-1.0 parameter shape under an alias is -32602, and nothing is published.
#[tokio::test]
async fn req_0008_030_pre_1_0_shape_under_alias_is_invalid_params() {
    let fx = Fixture::new();
    fx.seed_worker().await;
    let (_gw, app) = fx.app(fx.config()).await;
    let legacy = json!({
        "message": {
            "kind": "message", "messageId": "m-1", "role": "user",
            "parts": [{"kind": "text", "text": "hi"}]
        }
    });
    let reply = send(&app, Call::post(RPC, &rpc("message/send", legacy))).await;
    assert_eq!(reply.error_code(), -32602);
}

// [0008-031] Approval extension metadata on an inbound message is refused.
#[tokio::test]
async fn req_0008_031_approval_metadata_is_invalid_params() {
    let fx = Fixture::new();
    fx.seed_worker().await;
    let (_gw, app) = fx.app(fx.config()).await;
    let mut msg = message("hi");
    msg["metadata"] = json!({APPROVAL: {"decision": "approve"}});
    let body = rpc(
        "SendMessage",
        json!({"message": msg, "configuration": {"returnImmediately": true}}),
    );
    let reply = send(&app, Call::post(RPC, &body)).await;
    assert_eq!(reply.error_code(), -32602);
}

// [0008-032] The provenance key is written by the gateway; a caller's value is replaced.
#[tokio::test]
async fn req_0008_032_provenance_is_written_by_the_gateway() {
    let fx = Fixture::new();
    fx.seed_worker().await;
    let (_gw, app) = fx.app(fx.config()).await;
    let mut requests = fx.serve(Op::Send).await;
    let mut msg = message("hi");
    msg["metadata"] = json!({PROVENANCE: {"forged": true}});
    let body = rpc(
        "SendMessage",
        json!({"message": msg, "configuration": {"returnImmediately": true}}),
    );
    let worker = async {
        let inbound = fx.next_inbound(&mut requests).await;
        let seen = match &inbound.body {
            RequestBody::SendMessage(request) => {
                format!("{:?}", request.message.as_ref().expect("message").metadata)
            }
            _ => String::new(),
        };
        fx.reply(inbound, &send_reply(task("t-1", pb::TaskState::Working)));
        seen
    };
    let (reply, seen) = tokio::join!(send(&app, Call::post(RPC, &body)), worker);
    assert_eq!(reply.status, StatusCode::OK);
    assert!(seen.contains(PROVENANCE), "gateway provenance present");
    assert!(!seen.contains("forged"), "caller provenance replaced");
}

// [0008-035] The gateway does not rewrite contextId in either direction.
#[tokio::test]
async fn req_0008_035_context_id_is_passed_through_unchanged() {
    let fx = Fixture::new();
    fx.seed_worker().await;
    let (_gw, app) = fx.app(fx.config()).await;
    let mut requests = fx.serve(Op::Send).await;
    let mut msg = message("hi");
    msg["contextId"] = json!("ctx-caller");
    let body = rpc(
        "SendMessage",
        json!({"message": msg, "configuration": {"returnImmediately": true}}),
    );
    let worker = async {
        let inbound = fx.next_inbound(&mut requests).await;
        let context = match &inbound.body {
            RequestBody::SendMessage(request) => request
                .message
                .as_ref()
                .expect("message")
                .context_id
                .clone(),
            _ => String::new(),
        };
        fx.reply(inbound, &send_reply(task("t-1", pb::TaskState::Working)));
        context
    };
    let (reply, context) = tokio::join!(send(&app, Call::post(RPC, &body)), worker);
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(context, "ctx-caller");
}

// [0008-038] ListTasks pageSize outside 1..=100 is -32602.
#[tokio::test]
async fn req_0008_038_list_page_size_out_of_range_is_invalid_params() {
    let fx = Fixture::new();
    fx.seed_worker().await;
    let (_gw, app) = fx.app(fx.config()).await;
    for size in [0, 101] {
        let body = rpc("ListTasks", json!({"pageSize": size}));
        let reply = send(&app, Call::post(RPC, &body)).await;
        assert_eq!(reply.error_code(), -32602, "pageSize {size}");
    }
}

// [0008-043] Integers in extension metadata must be exact doubles (at most 2^53).
#[tokio::test]
async fn req_0008_043_inexact_metadata_integer_is_invalid_params() {
    let fx = Fixture::new();
    fx.seed_worker().await;
    let (_gw, app) = fx.app(fx.config()).await;
    let mut msg = message("hi");
    msg["metadata"] = json!({"urn:example:count": 9_007_199_254_740_993_u64});
    let body = rpc(
        "SendMessage",
        json!({"message": msg, "configuration": {"returnImmediately": true}}),
    );
    let reply = send(&app, Call::post(RPC, &body)).await;
    assert_eq!(reply.error_code(), -32602);
}

// [0008-044] The sequence key is removed from every egress metadata map; other keys survive.
#[tokio::test]
async fn req_0008_044_sequence_key_is_removed_on_egress() {
    let fx = Fixture::new();
    fx.seed_worker().await;
    let (_gw, app) = fx.app(fx.config()).await;
    let mut requests = fx.serve(Op::Get).await;
    let metadata: pbjson_types::Struct = serde_json::from_value(json!({
        SEQUENCE: 5,
        "keep": "visible"
    }))
    .expect("struct");
    let mut reply_task = task("t-1", pb::TaskState::Working);
    reply_task.metadata = Some(metadata);
    let worker = async {
        let inbound = fx.next_inbound(&mut requests).await;
        fx.reply(inbound, &reply_task);
    };
    let (reply, ()) = tokio::join!(
        send(&app, Call::post(RPC, &rpc("GetTask", json!({"id": "t-1"})))),
        worker
    );
    assert_eq!(reply.status, StatusCode::OK);
    let text = reply.text();
    assert!(!text.contains(SEQUENCE), "sequence key removed: {text}");
    assert!(text.contains("visible"), "other metadata kept: {text}");
}

// [0008-045] Protocol errors carry an ErrorInfo detail with the reason and domain.
#[tokio::test]
async fn req_0008_045_error_body_carries_error_info() {
    let fx = Fixture::new();
    fx.seed_worker().await;
    let (_gw, app) = fx.app(fx.config()).await;
    let reply = send(&app, Call::post(RPC, &send_immediately("hi")).version(None)).await;
    let detail = &reply.json()["error"]["data"][0];
    assert_eq!(detail["@type"], "type.googleapis.com/google.rpc.ErrorInfo");
    assert!(detail["reason"].as_str().is_some_and(|r| !r.is_empty()));
    assert!(detail["domain"].as_str().is_some_and(|d| !d.is_empty()));
}

// [0008-046] Domain errors use HTTP 200; a JSON parse failure uses HTTP 400.
#[tokio::test]
async fn req_0008_046_domain_errors_are_http_200_and_parse_errors_400() {
    let fx = Fixture::new();
    fx.seed_worker().await;
    let (_gw, app) = fx.app(fx.config()).await;
    let domain = send(&app, Call::post(RPC, &send_immediately("hi")).version(None)).await;
    assert_eq!(domain.status, StatusCode::OK);
    assert_eq!(domain.error_code(), -32009);
    let parse = send(&app, Call::new("POST", RPC).body(b"{".to_vec())).await;
    assert_eq!(parse.status, StatusCode::BAD_REQUEST);
    assert_eq!(parse.error_code(), -32700);
}

// [0008-049] When no worker answers the agent, the reply is 503 with Retry-After.
#[tokio::test]
async fn req_0008_049_no_responder_is_503_with_retry_after() {
    let fx = Fixture::new();
    fx.seed_worker().await;
    let (_gw, app) = fx.app(fx.config()).await;
    let reply = send(&app, Call::post(RPC, &send_immediately("hi"))).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(reply.header("retry-after").is_some(), "Retry-After set");
}

// [0008-050] A blocking SendMessage whose initial task is terminal returns that task.
#[tokio::test]
async fn req_0008_050_blocking_send_returns_the_task() {
    let fx = Fixture::new();
    fx.seed_worker().await;
    let (_gw, app) = fx.app(fx.config()).await;
    let mut requests = fx.serve(Op::Send).await;
    let body = rpc("SendMessage", json!({"message": message("hi")}));
    let worker = async {
        let inbound = fx.next_inbound(&mut requests).await;
        fx.reply(inbound, &send_reply(task("t-1", pb::TaskState::Completed)));
    };
    let (reply, ()) = tokio::join!(send(&app, Call::post(RPC, &body)), worker);
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json()["result"]["task"]["id"], "t-1");
}

// [0008-051] With returnImmediately, the initial task is returned as soon as it arrives.
#[tokio::test]
async fn req_0008_051_return_immediately_returns_the_initial_task() {
    let fx = Fixture::new();
    fx.seed_worker().await;
    let (_gw, app) = fx.app(fx.config()).await;
    let mut requests = fx.serve(Op::Send).await;
    let worker = async {
        let inbound = fx.next_inbound(&mut requests).await;
        fx.reply(inbound, &send_reply(task("t-2", pb::TaskState::Working)));
    };
    let (reply, ()) = tokio::join!(send(&app, Call::post(RPC, &send_immediately("hi"))), worker);
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json()["result"]["task"]["id"], "t-2");
}

// [0008-052] The blocking wait stays under the 120 s client timeout.
#[test]
fn req_0008_052_default_blocking_wait_is_under_client_timeout() {
    assert!(GatewayConfig::default().blocking_wait < Duration::from_secs(120));
}

// [0008-059] Push credentials are refused with -32602 PUSH_CREDENTIALS_NOT_SUPPORTED.
#[tokio::test]
async fn req_0008_059_push_credentials_are_refused() {
    let fx = Fixture::new();
    fx.seed_worker().await;
    let (_gw, app) = fx.app(fx.config()).await;
    let body = rpc(
        "CreateTaskPushNotificationConfig",
        json!({
            "taskId": "t-1",
            "url": "https://hooks.example/notify",
            "authentication": {"scheme": "Bearer", "credentials": "s3cret"}
        }),
    );
    let reply = send(&app, Call::post(RPC, &body)).await;
    assert_eq!(reply.error_code(), -32602);
    assert_eq!(reply.error_reason(), "PUSH_CREDENTIALS_NOT_SUPPORTED");
}

// [0008-060] A push URL outside the peer's allow-list is refused, whatever its scheme.
#[tokio::test]
async fn req_0008_060_push_url_outside_allow_list_is_refused() {
    let fx = Fixture::new();
    fx.seed_worker().await;
    let (_gw, app) = fx.app(fx.config()).await;
    for url in [
        "https://hooks.example/notify",
        "http://hooks.example/notify",
    ] {
        let body = rpc(
            "CreateTaskPushNotificationConfig",
            json!({"taskId": "t-1", "url": url}),
        );
        let reply = send(&app, Call::post(RPC, &body)).await;
        assert_eq!(reply.error_code(), -32602, "{url}");
        assert_eq!(reply.error_reason(), "PUSH_URL_NOT_PERMITTED", "{url}");
    }
}

// [0008-060] A plain-http push URL is refused even for a loopback host without push_loopback_http.
#[tokio::test]
async fn req_0008_060_plain_http_push_to_loopback_needs_policy() {
    let fx = Fixture::new();
    fx.seed_worker().await;
    let (_gw, app) = fx.app(fx.config()).await;
    let body = rpc(
        "CreateTaskPushNotificationConfig",
        json!({"taskId": "t-1", "url": "http://127.0.0.1:8080/hook"}),
    );
    let reply = send(&app, Call::post(RPC, &body)).await;
    assert_eq!(reply.error_reason(), "PUSH_URL_NOT_PERMITTED");
}
