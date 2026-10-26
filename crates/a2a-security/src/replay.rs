//! Bounded in-memory replay cache for signature nonces and decision request ids ([0007-041]).
//!
//! Entries live until `now + window`. A duplicate and a saturated cache both reject: a
//! receiver that cannot remember a nonce cannot prove it is fresh, so it fails closed.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// Remembers `(kid, nonce)` pairs for the replay window ([0007-041]).
#[derive(Debug)]
pub struct ReplayCache {
    capacity: usize,
    entries: Mutex<HashMap<String, i64>>,
}

impl ReplayCache {
    /// Creates an empty cache that holds at most `capacity` live entries.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Reports whether `key` is recorded and still live at `now_ms`.
    #[must_use]
    pub fn contains(&self, key: &str, now_ms: i64) -> bool {
        self.lock()
            .get(key)
            .is_some_and(|expires| now_ms <= *expires)
    }

    /// Records `key` until `now_ms + window_ms`.
    ///
    /// Returns `false` when `key` is already live or when the cache is full of live entries.
    /// Expired entries are pruned only when the cache is at capacity.
    #[must_use]
    pub fn record(&self, key: &str, now_ms: i64, window_ms: i64) -> bool {
        let mut entries = self.lock();
        if entries.get(key).is_some_and(|expires| now_ms <= *expires) {
            return false;
        }
        if entries.len() >= self.capacity {
            entries.retain(|_, expires| now_ms <= *expires);
            if entries.len() >= self.capacity {
                return false;
            }
        }
        entries.insert(key.to_owned(), now_ms.saturating_add(window_ms));
        true
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, i64>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::ReplayCache;

    #[test]
    fn req_0007_041_repeat_inside_window_is_rejected() {
        let cache = ReplayCache::new(8);
        assert!(cache.record("agent-a:AAAAAAAAAAA nonce", 1_000, 600_000));
        assert!(cache.contains("agent-a:AAAAAAAAAAA nonce", 1_000));
        assert!(!cache.record("agent-a:AAAAAAAAAAA nonce", 2_000, 600_000));
    }

    #[test]
    fn req_0007_041_entry_is_live_through_its_last_millisecond_only() {
        let cache = ReplayCache::new(8);
        assert!(cache.record("k", 1_000, 600_000));
        assert!(cache.contains("k", 601_000));
        assert!(!cache.contains("k", 601_001));
        assert!(cache.record("k", 601_001, 600_000));
    }

    #[test]
    fn req_0007_041_distinct_keys_do_not_collide() {
        let cache = ReplayCache::new(8);
        assert!(cache.record("kid-a nonce-1", 0, 1_000));
        assert!(cache.record("kid-a nonce-2", 0, 1_000));
        assert!(cache.record("kid-b nonce-1", 0, 1_000));
    }

    #[test]
    fn req_0007_041_full_cache_fails_closed_until_entries_expire() {
        let cache = ReplayCache::new(2);
        assert!(cache.record("a", 0, 1_000));
        assert!(cache.record("b", 0, 10_000));
        assert!(!cache.record("c", 500, 1_000));
        assert!(cache.record("c", 1_001, 1_000));
        assert!(!cache.contains("a", 1_001));
        assert!(cache.contains("b", 1_001));
    }

    #[test]
    fn req_0007_041_zero_capacity_rejects_every_record() {
        let cache = ReplayCache::new(0);
        assert!(!cache.record("a", 0, 1_000));
        assert!(!cache.contains("a", 0));
    }
}
