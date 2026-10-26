//! Bearer authentication (RFC-0008 0008-021, 0008-022, 0008-024).
//!
//! Peer tokens are stored as SHA-256 digests and compared in constant time. A presented
//! token that does not match never falls back to loopback anonymous access.

use std::net::IpAddr;

use http::HeaderMap;
use http::header::{AUTHORIZATION, HeaderValue};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::config::{GatewayConfig, LOOPBACK_ORIGIN};

/// SHA-256 digest of a bearer token.
#[must_use]
pub fn token_digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// Authenticated identity of one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caller {
    /// Unauthenticated request from a loopback socket (0008-022).
    Loopback,
    /// Bearer-authenticated peer, by index into `GatewayConfig::peers`.
    Peer(usize),
}

impl Caller {
    /// Effective origin: the peer name, or `loopback`.
    #[must_use]
    pub fn origin<'a>(&self, config: &'a GatewayConfig) -> &'a str {
        match self {
            Self::Loopback => LOOPBACK_ORIGIN,
            Self::Peer(index) => config
                .peers
                .get(*index)
                .map_or(LOOPBACK_ORIGIN, |p| p.name()),
        }
    }
}

/// Why authentication failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    /// No `Authorization` header and anonymous loopback is not available.
    Missing,
    /// Malformed header, several headers, or a token that matches no peer.
    Invalid,
}

/// Authenticates the request headers against the configured peers.
///
/// # Errors
/// Returns `AuthError::Missing` when no credentials were presented and anonymous access is not
/// allowed for this socket, and `AuthError::Invalid` otherwise for a presented bad credential.
pub fn authenticate(
    config: &GatewayConfig,
    headers: &HeaderMap,
    source: IpAddr,
) -> Result<Caller, AuthError> {
    let mut values = headers.get_all(AUTHORIZATION).iter();
    let Some(first) = values.next() else {
        return if config.loopback_anonymous && source.is_loopback() {
            Ok(Caller::Loopback)
        } else {
            Err(AuthError::Missing)
        };
    };
    if values.next().is_some() {
        return Err(AuthError::Invalid);
    }
    let token = bearer_token(first).ok_or(AuthError::Invalid)?;
    let digest = token_digest(token);
    let mut found = None;
    for (index, peer) in config.peers.iter().enumerate() {
        let hit = bool::from(peer.digest.as_slice().ct_eq(digest.as_slice()));
        if hit && found.is_none() {
            found = Some(index);
        }
    }
    found.map(Caller::Peer).ok_or(AuthError::Invalid)
}

fn bearer_token(value: &HeaderValue) -> Option<&str> {
    let text = value.to_str().ok()?;
    let (scheme, token) = text.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    if token.is_empty() || token.contains(char::is_whitespace) {
        None
    } else {
        Some(token)
    }
}
