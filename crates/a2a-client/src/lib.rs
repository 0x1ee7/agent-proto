//! Requester runtime for the A2A-Bus protocol (layer L3, RFC-0001 4.2).
//!
//! The crate sends requests, decodes replies and task events, follows task streams with
//! ordered delivery, discovers agents from the card and presence stores, and builds budget and
//! approval messages. It speaks only through [`bus_api::bus::Bus`].

#![forbid(unsafe_code)]

pub mod approval;
pub mod budget;
pub mod client;
pub mod config;
pub mod directory;
pub mod error;
pub mod event;
pub mod outbound;
pub mod presence;
pub mod signing;
pub mod stream;

pub use client::{Client, Outcome, SendReply, StreamStart, Unary};
pub use config::{CallOptions, Config, SigningHook, VerifyHook};
pub use directory::{CardRecord, Directory, Listing, Query};
pub use error::{ApprovalError, ClientError};
pub use event::Event;
pub use outbound::{Outbound, text};
pub use presence::{Presence, PresenceState};
