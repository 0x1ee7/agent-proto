//! N3 (RFC-0014 0014-023): an unauthenticated request to the gateway is refused.
//!
//! In-process substitute: the real gateway router from `gateway-http` answers requests sent
//! through `tower::oneshot`. The source address is non-loopback, so the loopback-anonymous
//! rule (0008-022) does not apply and every request must carry valid credentials.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use a2a_client::Config as ClientConfig;
use a2a_core::envelope::Clock;
use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use gateway_http::{GatewayConfig, HttpGateway, PeerConfig, router};
use tower::ServiceExt;

use super::rig::{Rig, count_commands, rig, try_block_on};

/// A minimal `SendMessage` JSON-RPC body; authentication runs before it is parsed.
const SEND: &str = r#"{"jsonrpc":"2.0","id":1,"method":"SendMessage","params":{}}"#;

/// Documentation-range (TEST-NET-3) address, not loopback.
const SOURCE: IpAddr = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9));

/// Host header for the default loopback listener (`127.0.0.1:9910`, 0008-001, 0008-014).
const HOST: &str = "127.0.0.1:9910";

/// Gateway-local `UNAUTHORIZED` code (0008-047).
const UNAUTHORIZED_CODE: &str = "-32050";

/// Sends a request with no credentials and one with a bad token; both must get 401 with the
/// `UNAUTHORIZED` error, and the worker must start no backend turn. The rig's worker is not
/// pumped here, so admission is checked by the 401 itself rather than by bus traffic.
pub(crate) fn n3_unauthenticated_refused() -> Result<(), String> {
    let rig: Rig = rig()?;
    let mut config = GatewayConfig::default();
    config.peers.push(
        PeerConfig::new("peer", "correct-token", Vec::new()).map_err(|e| format!("peer: {e:?}"))?,
    );
    let gateway = HttpGateway::new(
        Arc::clone(&rig.bus),
        config,
        ClientConfig::default(),
        Arc::clone(&rig.clock) as Arc<dyn Clock>,
    )
    .map_err(|e| format!("gateway: {e:?}"))?;
    let app = router(gateway);
    for authorization in [None, Some("Bearer wrong-token")] {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::HOST, HOST);
        if let Some(value) = authorization {
            builder = builder.header(header::AUTHORIZATION, value);
        }
        let mut request = builder
            .body(Body::from(SEND))
            .map_err(|e| format!("request: {e}"))?;
        request
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::new(SOURCE, 40_000)));
        let response =
            try_block_on(app.clone().oneshot(request))?.map_err(|e| format!("oneshot: {e}"))?;
        if response.status() != StatusCode::UNAUTHORIZED {
            return Err(format!(
                "status {} for credentials {authorization:?}, want 401",
                response.status()
            ));
        }
        let body = try_block_on(to_bytes(response.into_body(), usize::MAX))?
            .map_err(|e| format!("body: {e}"))?;
        if !String::from_utf8_lossy(&body).contains(UNAUTHORIZED_CODE) {
            return Err(format!("body lacks UNAUTHORIZED ({UNAUTHORIZED_CODE})"));
        }
    }
    let starts = count_commands(&rig.mock, |_| true);
    if starts != 0 {
        return Err(format!(
            "{starts} backend commands after refused requests, want 0"
        ));
    }
    Ok(())
}
