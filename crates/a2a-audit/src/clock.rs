//! Injectable UTC clock and RFC 3339 millisecond timestamps (RFC-0013 0013-005).

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};

/// Source of wall-clock time. Injected so rotation, retention and coalescing are testable.
pub trait Clock: Send + Sync + fmt::Debug {
    fn now(&self) -> DateTime<Utc>;
}

/// Reads the operating system clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        DateTime::<Utc>::from(SystemTime::now())
    }
}

/// Clock whose time is set by the caller. Used by tests and deterministic replays.
#[derive(Debug, Clone)]
pub struct ManualClock {
    now: Arc<Mutex<DateTime<Utc>>>,
}

impl ManualClock {
    pub fn new(start: DateTime<Utc>) -> Self {
        Self {
            now: Arc::new(Mutex::new(start)),
        }
    }

    pub fn set(&self, time: DateTime<Utc>) {
        *self.lock() = time;
    }

    pub fn advance(&self, by: Duration) {
        let delta = TimeDelta::from_std(by).unwrap_or(TimeDelta::MAX);
        let mut guard = self.lock();
        let next = guard.checked_add_signed(delta).unwrap_or(*guard);
        *guard = next;
    }

    fn lock(&self) -> MutexGuard<'_, DateTime<Utc>> {
        self.now.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.lock()
    }
}

/// RFC 3339 UTC with millisecond precision and a trailing `Z`: `YYYY-MM-DDTHH:MM:SS.mmmZ`.
pub fn format_timestamp(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// True when `value` is exactly the audit timestamp form.
pub fn is_audit_timestamp(value: &str) -> bool {
    match DateTime::parse_from_rfc3339(value) {
        Ok(parsed) => format_timestamp(parsed.with_timezone(&Utc)) == value,
        Err(_) => false,
    }
}

/// UTC calendar date as `yyyymmdd`, used in rotated file names.
pub fn date_stamp(time: DateTime<Utc>) -> String {
    time.format("%Y%m%d").to_string()
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn formats_millisecond_utc_with_z() {
        let time = Utc
            .with_ymd_and_hms(2026, 3, 1, 9, 14, 3)
            .single()
            .unwrap_or_default()
            + TimeDelta::milliseconds(218);
        assert_eq!(format_timestamp(time), "2026-03-01T09:14:03.218Z");
    }

    #[test]
    fn rejects_offsets_and_missing_millis() {
        assert!(is_audit_timestamp("2026-03-01T09:14:03.218Z"));
        assert!(!is_audit_timestamp("2026-03-01T09:14:03Z"));
        assert!(!is_audit_timestamp("2026-03-01T09:14:03.218+00:00"));
    }

    #[test]
    fn manual_clock_advances_deterministically() {
        let clock = ManualClock::new(
            Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
                .single()
                .unwrap_or_default(),
        );
        clock.advance(Duration::from_secs(90));
        assert_eq!(date_stamp(clock.now()), "20260101");
        assert_eq!(format_timestamp(clock.now()), "2026-01-01T00:01:30.000Z");
    }
}
