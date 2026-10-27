//! NATS JetStream binding of `bus-api::Bus` (RFC-0010 and RFC-0011, M1 profile).
//!
//! One [`NatsBus`] is one NATS connection authenticated by one NKey identity. Every
//! operation takes the caller's [`bus_api::name::Principal`], which must equal the
//! connection identity; any other caller gets `PermissionDenied`.
//!
//! The crate also renders the server permission sets of RFC-0010 section 4.8
//! ([`permissions`]) and provisions the streams, buckets and archiver consumer
//! of sections 4.5-4.7 ([`provision`]).

#![forbid(unsafe_code)]

pub mod blob;
pub mod config;
pub mod errors;
pub mod kv;
pub mod layout;
pub mod permissions;
pub mod principal;
pub mod provision;
pub mod settle;
pub mod stream;
pub mod wire;

mod bus;
mod connection;
mod denial;
mod request;

pub use connection::{ConnectError, NatsBus, connect};
pub use request::NatsRequests;
