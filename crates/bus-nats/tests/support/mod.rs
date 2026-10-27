//! Shared harness for the live tests: a throwaway `nats-server` and the
//! NATS-backed conformance factory.

#![allow(clippy::unwrap_used, clippy::expect_used)]

pub mod factory;
pub mod server;
