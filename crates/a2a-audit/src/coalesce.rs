//! Coalescing of PEER_REJECTED events (RFC-0013 0013-047, 0013-048).
//!
//! Rejections are counted per (peer, reason) over a fixed window that opens at the first
//! rejection. When the window closes, one summary carries the total count for the window.
//! Other audit types are never coalesced; that is the caller's rule, not this module's.

use std::collections::BTreeMap;

use chrono::{DateTime, TimeDelta, Utc};

/// Length of one coalescing window (0013-047).
pub const WINDOW: TimeDelta = TimeDelta::seconds(10);

/// One closed window: total rejections for a peer and reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    /// Rejected peer.
    pub peer: String,
    /// Rejection reason.
    pub reason: String,
    /// Rejections in the window, including the first.
    pub count: u64,
}

#[derive(Debug, Clone)]
struct Window {
    opened: DateTime<Utc>,
    count: u64,
}

/// Tracks open windows keyed by (peer, reason).
#[derive(Debug, Default)]
pub struct PeerCoalescer {
    open: BTreeMap<(String, String), Window>,
}

impl PeerCoalescer {
    /// Creates an empty coalescer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Counts one rejection. The first rejection in a window opens it at `now`.
    pub fn record(&mut self, peer: &str, reason: &str, now: DateTime<Utc>) {
        self.open
            .entry((peer.to_owned(), reason.to_owned()))
            .and_modify(|window| window.count += 1)
            .or_insert(Window {
                opened: now,
                count: 1,
            });
    }

    /// Removes and returns windows that closed at or before `now`.
    pub fn take_expired(&mut self, now: DateTime<Utc>) -> Vec<Summary> {
        let expired: Vec<(String, String)> = self
            .open
            .iter()
            .filter(|(_, window)| now - window.opened >= WINDOW)
            .map(|(key, _)| key.clone())
            .collect();
        expired
            .into_iter()
            .filter_map(|key| self.open.remove(&key).map(|window| summary(key, window)))
            .collect()
    }

    /// Removes and returns every open window, regardless of age (shutdown).
    pub fn take_all(&mut self) -> Vec<Summary> {
        std::mem::take(&mut self.open)
            .into_iter()
            .map(|(key, window)| summary(key, window))
            .collect()
    }

    /// Number of open windows.
    pub fn open_windows(&self) -> usize {
        self.open.len()
    }
}

fn summary((peer, reason): (String, String), window: Window) -> Summary {
    Summary {
        peer,
        reason,
        count: window.count,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn at(second: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 1, 12, 0, 0).single().unwrap() + TimeDelta::seconds(second)
    }

    #[test]
    fn req_0013_047_hundred_rejections_in_window_yield_one_summary() {
        let mut coalescer = PeerCoalescer::new();
        for i in 0..100 {
            coalescer.record("peer-x", "bad-signature", at(i / 20));
        }
        assert!(coalescer.take_expired(at(9)).is_empty());
        let closed = coalescer.take_expired(at(10));
        assert_eq!(
            closed,
            vec![Summary {
                peer: "peer-x".to_owned(),
                reason: "bad-signature".to_owned(),
                count: 100,
            }]
        );
        assert_eq!(coalescer.open_windows(), 0);
    }

    #[test]
    fn req_0013_047_windows_are_keyed_by_peer_and_reason() {
        let mut coalescer = PeerCoalescer::new();
        coalescer.record("peer-a", "bad-signature", at(0));
        coalescer.record("peer-a", "expired", at(0));
        coalescer.record("peer-b", "bad-signature", at(0));
        coalescer.record("peer-a", "bad-signature", at(1));
        let mut closed = coalescer.take_all();
        closed.sort_by(|a, b| (&a.peer, &a.reason).cmp(&(&b.peer, &b.reason)));
        let counts: Vec<(String, String, u64)> = closed
            .into_iter()
            .map(|s| (s.peer, s.reason, s.count))
            .collect();
        assert_eq!(
            counts,
            vec![
                ("peer-a".to_owned(), "bad-signature".to_owned(), 2),
                ("peer-a".to_owned(), "expired".to_owned(), 1),
                ("peer-b".to_owned(), "bad-signature".to_owned(), 1),
            ]
        );
    }

    #[test]
    fn req_0013_047_rejection_after_window_opens_new_window() {
        let mut coalescer = PeerCoalescer::new();
        coalescer.record("peer-x", "expired", at(0));
        assert_eq!(coalescer.take_expired(at(10)).len(), 1);
        coalescer.record("peer-x", "expired", at(10));
        assert!(coalescer.take_expired(at(19)).is_empty());
        let closed = coalescer.take_expired(at(20));
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].count, 1);
    }

    #[test]
    fn req_0013_047_take_all_drains_open_windows_at_shutdown() {
        let mut coalescer = PeerCoalescer::new();
        coalescer.record("peer-x", "expired", at(0));
        let closed = coalescer.take_all();
        assert_eq!(closed.len(), 1);
        assert_eq!(coalescer.open_windows(), 0);
        assert!(coalescer.take_all().is_empty());
    }
}
