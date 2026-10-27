//! Connection configuration. The NKey seed is held in [`Seed`], whose `Debug`
//! output is redacted; the seed is never printed, logged or serialised (0010-071).

use std::fmt;
use std::time::Duration;

use a2a_types::ids::TenantId;

/// An NKey user seed. It has no `Display` and a redacted `Debug`.
#[derive(Clone)]
pub struct Seed(String);

impl Seed {
    /// Wraps a seed string. The caller owns its secrecy.
    #[must_use]
    pub fn new(seed: impl Into<String>) -> Self {
        Self(seed.into())
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Seed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Seed(<redacted>)")
    }
}

/// Everything [`crate::connect`] needs.
#[derive(Debug, Clone)]
pub struct NatsConfig {
    /// Server URL, for example `nats://127.0.0.1:4222`.
    pub url: String,
    /// The identity this connection authenticates as. Must match the NKey user.
    pub identity: String,
    /// NKey user seed of `identity`.
    pub seed: Seed,
    /// The single tenant this binding serves (M1: `local`).
    pub tenant: TenantId,
    /// Client-side deadline for connection establishment and API calls.
    pub request_timeout: Duration,
    /// Consumer `ack_wait` of the push consumers created by [`crate::NatsBus::consume`].
    /// Production default is [`crate::layout::ACK_WAIT`] (30 s); test harnesses may shorten it.
    pub ack_wait: Duration,
}
