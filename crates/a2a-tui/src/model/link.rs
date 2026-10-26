//! Console-to-directory link health (RFC-0012 0012-050).

use crate::redact;

/// Silence after which the link counts as disconnected and decisions are disabled.
pub const HEARTBEAT_TIMEOUT_MS: u64 = 5_000;
/// Silence after which the banner shows the link as degraded.
pub const DEGRADED_AFTER_MS: u64 = 2_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    Connecting,
    Healthy,
    Degraded,
    Disconnected,
}

#[derive(Debug, Default)]
pub struct Connection {
    last_ok_ms: Option<u64>,
    failure: Option<String>,
    state: Option<LinkState>,
}

impl Connection {
    /// Records a successful sync at `now_ms`.
    pub fn ok(&mut self, now_ms: u64) {
        self.last_ok_ms = Some(now_ms);
        self.failure = None;
        self.state = Some(LinkState::Healthy);
    }

    /// Records a failed sync; the state is decided by `tick` from heartbeat age.
    pub fn failed(&mut self, reason: &str) {
        self.failure = Some(redact::truncate_chars(&redact::escape(reason), 120));
    }

    /// Recomputes the state from the age of the last successful sync.
    pub fn tick(&mut self, now_ms: u64) {
        self.state = Some(self.state_at(now_ms));
    }

    /// The link state at `now_ms`.
    #[must_use]
    pub fn state_at(&self, now_ms: u64) -> LinkState {
        match self.last_ok_ms {
            None => LinkState::Connecting,
            Some(last) => match now_ms.saturating_sub(last) {
                age if age > HEARTBEAT_TIMEOUT_MS => LinkState::Disconnected,
                age if age > DEGRADED_AFTER_MS => LinkState::Degraded,
                _ => LinkState::Healthy,
            },
        }
    }

    /// Milliseconds since the last successful sync.
    #[must_use]
    pub fn age_ms(&self, now_ms: u64) -> Option<u64> {
        self.last_ok_ms.map(|last| now_ms.saturating_sub(last))
    }

    /// Decisions are allowed only while the heartbeat is fresh (0012-050).
    #[must_use]
    pub fn decisions_enabled(&self, now_ms: u64) -> bool {
        self.state_at(now_ms) != LinkState::Disconnected && self.last_ok_ms.is_some()
    }

    /// Last sync failure reason, if the most recent sync failed.
    #[must_use]
    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    /// Last state computed by `tick`.
    #[must_use]
    pub fn last_state(&self) -> LinkState {
        self.state.unwrap_or(LinkState::Connecting)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn req_0012_050_decisions_disabled_after_five_seconds_without_heartbeat() {
        let mut link = Connection::default();
        link.ok(1_000);
        assert!(link.decisions_enabled(1_000));
        assert_eq!(
            link.state_at(1_000 + DEGRADED_AFTER_MS + 1),
            LinkState::Degraded
        );
        assert!(link.decisions_enabled(1_000 + HEARTBEAT_TIMEOUT_MS));
        let late = 1_000 + HEARTBEAT_TIMEOUT_MS + 1;
        assert_eq!(link.state_at(late), LinkState::Disconnected);
        assert!(!link.decisions_enabled(late));
    }

    #[test]
    fn req_0012_050_heartbeat_restores_decisions() {
        let mut link = Connection::default();
        link.ok(0);
        link.tick(60_000);
        assert_eq!(link.last_state(), LinkState::Disconnected);
        link.ok(60_000);
        assert!(link.decisions_enabled(60_000));
    }

    #[test]
    fn req_0012_050_no_sync_yet_means_connecting_and_no_decisions() {
        let link = Connection::default();
        assert_eq!(link.state_at(10), LinkState::Connecting);
        assert!(!link.decisions_enabled(10));
    }
}
