//! Fixed-window request counters (RFC-0008 0008-022, 0008-023).

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::config::RatePolicy;

#[derive(Debug, Clone, Copy)]
struct Bucket {
    start: Instant,
    count: u32,
}

/// Fixed-window counter keyed by origin or source address.
#[derive(Debug)]
pub struct RateLimiter {
    policy: RatePolicy,
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl RateLimiter {
    /// Creates a limiter enforcing `policy` independently per key.
    #[must_use]
    pub fn new(policy: RatePolicy) -> Self {
        Self {
            policy,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Records one event for `key`.
    ///
    /// # Errors
    /// Returns the time to wait until the window resets when the key is already at its limit.
    pub fn hit(&self, key: &str, now: Instant) -> Result<(), Duration> {
        let mut buckets = self.lock();
        let bucket = Self::current(&mut buckets, key, now, self.policy.window);
        if bucket.count >= self.policy.limit {
            return Err(self.reset_in(bucket.start, now));
        }
        bucket.count += 1;
        Ok(())
    }

    /// Checks whether `key` is already at its limit without recording an event.
    ///
    /// # Errors
    /// Returns the time to wait until the window resets when the key is at its limit.
    pub fn check(&self, key: &str, now: Instant) -> Result<(), Duration> {
        let mut buckets = self.lock();
        let bucket = Self::current(&mut buckets, key, now, self.policy.window);
        if bucket.count >= self.policy.limit {
            Err(self.reset_in(bucket.start, now))
        } else {
            Ok(())
        }
    }

    fn reset_in(&self, start: Instant, now: Instant) -> Duration {
        (start + self.policy.window).saturating_duration_since(now)
    }

    fn current<'a>(
        buckets: &'a mut HashMap<String, Bucket>,
        key: &str,
        now: Instant,
        window: Duration,
    ) -> &'a mut Bucket {
        let bucket = buckets.entry(key.to_owned()).or_insert(Bucket {
            start: now,
            count: 0,
        });
        if now.saturating_duration_since(bucket.start) >= window {
            bucket.start = now;
            bucket.count = 0;
        }
        bucket
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Bucket>> {
        match self.buckets.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn policy(limit: u32) -> RatePolicy {
        RatePolicy {
            limit,
            window: Duration::from_secs(60),
        }
    }

    /// [0008-023] A key at its limit is refused until its window resets; other keys are unaffected.
    #[test]
    fn req_0008_023_limiter_refuses_at_limit_until_window_resets() {
        let limiter = RateLimiter::new(policy(2));
        let start = Instant::now();
        assert!(limiter.hit("a", start).is_ok());
        assert!(limiter.hit("a", start).is_ok());
        let wait = limiter
            .hit("a", start + Duration::from_secs(10))
            .expect_err("at limit");
        assert_eq!(wait, Duration::from_secs(50));
        assert!(limiter.hit("b", start).is_ok(), "keys are independent");
        assert!(
            limiter.hit("a", start + Duration::from_secs(60)).is_ok(),
            "window reset"
        );
    }

    /// [0008-023] A check reads the counter without recording an event.
    #[test]
    fn req_0008_023_check_does_not_record_an_event() {
        let limiter = RateLimiter::new(policy(1));
        let now = Instant::now();
        assert!(limiter.check("a", now).is_ok());
        assert!(limiter.check("a", now).is_ok(), "check records nothing");
        assert!(limiter.hit("a", now).is_ok());
        assert!(limiter.check("a", now).is_err());
    }
}
