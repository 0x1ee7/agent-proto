//! Time source and the strict timestamp formats of RFC-0007 ([0007-034], [0007-022]).

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use time::format_description::well_known::Rfc3339;
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time};

use crate::error::SecurityError;

/// Source of wall-clock milliseconds since the Unix epoch.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
}

/// The system clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(elapsed) => i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX),
            Err(before) => i64::try_from(before.duration().as_millis()).map_or(i64::MIN, |ms| -ms),
        }
    }
}

/// A clock that only moves when told to. For tests and deterministic replays.
#[derive(Debug)]
pub struct ManualClock(AtomicI64);

impl ManualClock {
    #[must_use]
    pub const fn new(now_ms: i64) -> Self {
        Self(AtomicI64::new(now_ms))
    }

    pub fn set(&self, now_ms: i64) {
        self.0.store(now_ms, Ordering::SeqCst);
    }

    pub fn advance(&self, delta_ms: i64) {
        self.0.fetch_add(delta_ms, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// Formats milliseconds as `YYYY-MM-DDTHH:MM:SS.mmmZ` ([0007-034] `A2AB-Ts`).
pub fn format_ts_ms(ms: i64) -> Result<String, SecurityError> {
    let nanos = i128::from(ms) * 1_000_000;
    let dt = OffsetDateTime::from_unix_timestamp_nanos(nanos)
        .map_err(|_| SecurityError::Invalid("timestamp out of range"))?;
    if !(0..=9999).contains(&dt.year()) {
        return Err(SecurityError::Invalid("timestamp year outside 0000-9999"));
    }
    Ok(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        dt.year(),
        u8::from(dt.month()),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
        dt.millisecond(),
    ))
}

/// Parses exactly the `YYYY-MM-DDTHH:MM:SS.mmmZ` shape. Anything else is `None`.
#[must_use]
pub fn parse_ts_ms(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() != 24 {
        return None;
    }
    let shape_ok = bytes.iter().enumerate().all(|(i, &c)| match i {
        4 | 7 => c == b'-',
        10 => c == b'T',
        13 | 16 => c == b':',
        19 => c == b'.',
        23 => c == b'Z',
        _ => c.is_ascii_digit(),
    });
    if !shape_ok {
        return None;
    }
    let field = |from: usize, to: usize| text.get(from..to)?.parse::<u32>().ok();
    let year = i32::try_from(field(0, 4)?).ok()?;
    let month = Month::try_from(u8::try_from(field(5, 7)?).ok()?).ok()?;
    let day = u8::try_from(field(8, 10)?).ok()?;
    let hour = u8::try_from(field(11, 13)?).ok()?;
    let minute = u8::try_from(field(14, 16)?).ok()?;
    let second = u8::try_from(field(17, 19)?).ok()?;
    let millisecond = u16::try_from(field(20, 23)?).ok()?;
    let date = Date::from_calendar_date(year, month, day).ok()?;
    let time = Time::from_hms_milli(hour, minute, second, millisecond).ok()?;
    let instant = PrimitiveDateTime::new(date, time).assume_utc();
    i64::try_from(instant.unix_timestamp_nanos() / 1_000_000).ok()
}

/// Parses an RFC 3339 UTC instant (`Z` suffix required) into milliseconds.
#[must_use]
pub fn parse_rfc3339_utc_ms(text: &str) -> Option<i64> {
    if !text.ends_with('Z') {
        return None;
    }
    let instant = OffsetDateTime::parse(text, &Rfc3339).ok()?;
    i64::try_from(instant.unix_timestamp_nanos() / 1_000_000).ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn req_0007_034_ts_round_trips_at_millisecond_precision() {
        let ms = 1_767_225_600_123;
        let text = format_ts_ms(ms).expect("in range");
        assert_eq!(text, "2026-01-01T00:00:00.123Z");
        assert_eq!(parse_ts_ms(&text), Some(ms));
    }

    #[test]
    fn req_0007_034_ts_parser_rejects_every_non_canonical_shape() {
        for bad in [
            "2026-01-01T00:00:00Z",
            "2026-01-01T00:00:00.12Z",
            "2026-01-01T00:00:00.1234Z",
            "2026-01-01T00:00:00.123+00:00",
            "2026-01-01 00:00:00.123Z",
            "2026-13-01T00:00:00.123Z",
            "2026-02-30T00:00:00.123Z",
            "2026-01-01T24:00:00.123Z",
            "+026-01-01T00:00:00.123Z",
            "",
        ] {
            assert_eq!(parse_ts_ms(bad), None, "accepted {bad:?}");
        }
    }

    #[test]
    fn req_0007_034_ts_formatter_rejects_out_of_range_years() {
        assert!(format_ts_ms(i64::MAX).is_err());
        assert!(format_ts_ms(i64::MIN).is_err());
    }

    #[test]
    fn req_0007_022_rfc3339_requires_utc_suffix() {
        assert_eq!(parse_rfc3339_utc_ms("1970-01-01T00:00:01Z"), Some(1000));
        assert_eq!(parse_rfc3339_utc_ms("1970-01-01T00:00:01+00:00"), None);
        assert_eq!(parse_rfc3339_utc_ms("not a time"), None);
    }

    #[test]
    fn req_0007_034_manual_clock_moves_only_when_told() {
        let clock = ManualClock::new(10);
        assert_eq!(clock.now_ms(), 10);
        clock.advance(5);
        assert_eq!(clock.now_ms(), 15);
        clock.set(3);
        assert_eq!(clock.now_ms(), 3);
    }
}
