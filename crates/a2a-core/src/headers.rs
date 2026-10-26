//! Bus header names, value grammar and typed header fields (RFC-0002 section 4.4).
//!
//! Headers are plain data here. Signature fields (`A2AB-Nonce`, `A2AB-Kid`, `A2AB-Sig`) are carried
//! verbatim; their meaning belongs to RFC-0007 and `a2a-security`.

use std::fmt;
use std::ops::Range;

use a2a_types::ids::{AgentId, MessageId};
use bus_api::headers::Headers;

use crate::errors::WireError;

/// Message identifier of a request, event or reply (UUIDv7, RFC-0002 [0002-031]).
pub const MSG_ID: &str = "A2AB-Msg-Id";
/// Message identifier of the message this one answers (RFC-0002 [0002-031]).
pub const CAUSATION_ID: &str = "A2AB-Causation-Id";
/// Agent id of the sender (RFC-0002 [0002-011]).
pub const SENDER: &str = "A2AB-Sender";
/// Original requester, honored only from the gateway identity (RFC-0002 [0002-014]).
pub const ORIGIN: &str = "A2AB-Origin";
/// Sender wall-clock time, RFC 3339 UTC with milliseconds (RFC-0002 [0002-070]).
pub const TS: &str = "A2AB-Ts";
/// Relative time-to-live in milliseconds (RFC-0002 [0002-070]).
pub const TTL_MS: &str = "A2AB-Ttl-Ms";
/// Replay nonce, plain data (RFC-0007).
pub const NONCE: &str = "A2AB-Nonce";
/// Key identifier, plain data (RFC-0007).
pub const KID: &str = "A2AB-Kid";
/// Signature, plain data (RFC-0007).
pub const SIG: &str = "A2AB-Sig";
/// Protobuf message name of the body (RFC-0002 [0002-036]).
pub const PAYLOAD_TYPE: &str = "A2AB-Payload-Type";
/// Subscription resume position, u64 decimal (RFC-0002 [0002-061]).
pub const AFTER_SEQ: &str = "A2AB-After-Seq";
/// Highest sequence the task has reached, u64 decimal (RFC-0002 [0002-050]).
pub const LAST_SEQ: &str = "A2AB-Last-Seq";
/// Set to `true` on the final event of a stream (RFC-0002 [0002-044]).
pub const STREAM_END: &str = "A2AB-Stream-End";
/// Prefix of carried service parameters (RFC-0002 [0002-082]).
pub const SP_PREFIX: &str = "A2AB-SP-";
/// Protocol version (A2A service parameter `Version`, RFC-0002 [0002-080]).
pub const A2A_VERSION: &str = "A2A-Version";
/// Activated extensions (A2A service parameter `Extensions`, RFC-0002 [0002-081]).
pub const A2A_EXTENSIONS: &str = "A2A-Extensions";
/// W3C trace context.
pub const TRACEPARENT: &str = "traceparent";
/// W3C trace state.
pub const TRACESTATE: &str = "tracestate";
/// Media type of every body (RFC-0002 [0002-035]).
pub const CONTENT_TYPE: &str = "Content-Type";
/// Credential header; forbidden on the bus (RFC-0002 [0002-013]).
pub const AUTHORIZATION: &str = "Authorization";

/// Media type of binary protobuf bodies.
pub const PROTOBUF_CONTENT_TYPE: &str = "application/x-protobuf";
/// Media type of JSON bodies; accepted only with the debug option (RFC-0010).
pub const JSON_CONTENT_TYPE: &str = "application/json";

/// Largest accepted header value in bytes (RFC-0002 [0002-033]).
pub const MAX_VALUE_LEN: usize = 1024;
/// Smallest accepted `A2AB-Ttl-Ms` (RFC-0002 [0002-070]).
pub const MIN_TTL_MS: u64 = 1;
/// Largest accepted `A2AB-Ttl-Ms` (RFC-0002 [0002-070]).
pub const MAX_TTL_MS: u64 = 3_600_000;
/// Default allowance for sender clock skew and stale checks (RFC-0002 [0002-072], open issue O-8).
pub const DEFAULT_SKEW_MS: u64 = 30_000;

/// Why a header failed validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Missing,
    Invalid,
    Forbidden,
}

/// A header that is absent, malformed or not allowed on the bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderError {
    field: String,
    kind: Kind,
}

impl HeaderError {
    fn new(field: &str, kind: Kind) -> Self {
        Self {
            field: field.to_owned(),
            kind,
        }
    }

    /// The header name that failed.
    #[must_use]
    pub fn field(&self) -> &str {
        &self.field
    }

    /// Short English description of the failure, used in `BadRequest` details.
    #[must_use]
    pub fn description(&self) -> &'static str {
        match self.kind {
            Kind::Missing => "required header is missing",
            Kind::Invalid => "header value is invalid",
            Kind::Forbidden => "header is not allowed on the bus",
        }
    }
}

impl fmt::Display for HeaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.field, self.description())
    }
}

impl std::error::Error for HeaderError {}

/// Validates one header value against the grammar of RFC-0002 [0002-033]: printable ASCII
/// (0x20 to 0x7E), at most [`MAX_VALUE_LEN`] bytes, and no newline or NUL.
///
/// # Errors
/// Returns [`HeaderError`] naming `field` when the value breaks the grammar.
pub fn validate_value(field: &str, value: &str) -> Result<(), HeaderError> {
    if value.len() > MAX_VALUE_LEN || !value.bytes().all(|byte| (0x20..=0x7E).contains(&byte)) {
        return Err(HeaderError::new(field, Kind::Invalid));
    }
    Ok(())
}

/// Validates a `traceparent` value per the W3C Trace Context format (version 00 is exactly
/// 55 bytes; no all-zero ids; version `ff` is invalid).
///
/// # Errors
/// Returns [`HeaderError`] when the value is not a valid traceparent.
pub fn validate_traceparent(value: &str) -> Result<(), HeaderError> {
    validate_value(TRACEPARENT, value)?;
    let invalid = || HeaderError::new(TRACEPARENT, Kind::Invalid);
    let bytes = value.as_bytes();
    if bytes.len() < 55 || (bytes.len() > 55 && bytes[55] != b'-') {
        return Err(invalid());
    }
    let shape = bytes[2] == b'-'
        && bytes[35] == b'-'
        && bytes[52] == b'-'
        && is_lower_hex(&bytes[0..2])
        && is_lower_hex(&bytes[3..35])
        && is_lower_hex(&bytes[36..52])
        && is_lower_hex(&bytes[53..55]);
    if !shape {
        return Err(invalid());
    }
    let version = &bytes[0..2];
    if version == b"ff" || (version == b"00" && bytes.len() != 55) {
        return Err(invalid());
    }
    if bytes[3..35].iter().all(|byte| *byte == b'0')
        || bytes[36..52].iter().all(|byte| *byte == b'0')
    {
        return Err(invalid());
    }
    Ok(())
}

/// Validates a `tracestate` value: at most 32 list members and 512 bytes (W3C Trace Context).
///
/// # Errors
/// Returns [`HeaderError`] when the value exceeds the W3C limits or breaks the grammar.
pub fn validate_tracestate(value: &str) -> Result<(), HeaderError> {
    validate_value(TRACESTATE, value)?;
    let members = value
        .split(',')
        .filter(|member| !member.trim().is_empty())
        .count();
    if value.len() > 512 || members > 32 {
        return Err(HeaderError::new(TRACESTATE, Kind::Invalid));
    }
    Ok(())
}

/// Validates every A2A-Bus header and the W3C trace headers present in `headers`
/// (RFC-0002 [0002-033]). Unknown headers are not inspected (RFC-0002 [0002-032]).
///
/// # Errors
/// Returns the first [`HeaderError`] found, in header name order.
pub fn validate_all(headers: &Headers) -> Result<(), HeaderError> {
    for (name, value) in headers.iter() {
        if name == TRACEPARENT {
            validate_traceparent(value)?;
        } else if name == TRACESTATE {
            validate_tracestate(value)?;
        } else if is_known(name) {
            validate_value(name, value)?;
        }
    }
    Ok(())
}

fn is_known(name: &str) -> bool {
    name.starts_with("A2AB-")
        || matches!(
            name,
            A2A_VERSION | A2A_EXTENSIONS | CONTENT_TYPE | AUTHORIZATION
        )
}

/// Rejects an `Authorization` header: credentials never travel on the bus (RFC-0002 [0002-013]).
///
/// # Errors
/// Returns [`HeaderError`] with [`Kind::Forbidden`] when the header is present.
pub fn reject_authorization(headers: &Headers) -> Result<(), HeaderError> {
    if headers.get(AUTHORIZATION).is_some() {
        return Err(HeaderError::new(AUTHORIZATION, Kind::Forbidden));
    }
    Ok(())
}

/// Returns the value of a required header. Absent and empty values both count as missing.
///
/// # Errors
/// Returns [`HeaderError`] with [`Kind::Missing`] when the header is absent or empty.
pub fn required<'a>(headers: &'a Headers, name: &str) -> Result<&'a str, HeaderError> {
    match headers.get(name) {
        Some(value) if !value.is_empty() => Ok(value),
        _ => Err(HeaderError::new(name, Kind::Missing)),
    }
}

/// Returns the value of an optional header, treating an empty value as absent.
#[must_use]
pub fn optional<'a>(headers: &'a Headers, name: &str) -> Option<&'a str> {
    headers.get(name).filter(|value| !value.is_empty())
}

/// Parses `A2AB-Msg-Id` or `A2AB-Causation-Id`: a canonical lowercase UUIDv7 (RFC-0002 [0002-031]).
///
/// # Errors
/// Returns [`HeaderError`] with [`Kind::Invalid`] when the value is not a UUIDv7.
pub fn parse_msg_id(field: &str, value: &str) -> Result<MessageId, HeaderError> {
    let id = MessageId::parse(value).map_err(|_| HeaderError::new(field, Kind::Invalid))?;
    if !id.is_uuidv7() {
        return Err(HeaderError::new(field, Kind::Invalid));
    }
    Ok(id)
}

/// Parses an agent id header value such as `A2AB-Sender`.
///
/// # Errors
/// Returns [`HeaderError`] with [`Kind::Invalid`] when the value breaks the agent id grammar.
pub fn parse_agent_id(field: &str, value: &str) -> Result<AgentId, HeaderError> {
    AgentId::parse(value).map_err(|_| HeaderError::new(field, Kind::Invalid))
}

/// Parses `A2AB-Ts`: RFC 3339 UTC with exactly millisecond precision and a `Z` suffix,
/// for example `2026-03-14T10:22:30.912Z`. Returns Unix milliseconds (RFC-0002 [0002-070]).
///
/// # Errors
/// Returns [`HeaderError`] with [`Kind::Invalid`] for any other form, for dates before 1970,
/// or for impossible calendar dates.
pub fn parse_ts(value: &str) -> Result<u64, HeaderError> {
    let invalid = || HeaderError::new(TS, Kind::Invalid);
    let bytes = value.as_bytes();
    let shape = bytes.len() == 24
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[19] == b'.'
        && bytes[23] == b'Z';
    if !shape {
        return Err(invalid());
    }
    let field = |range: Range<usize>| -> Option<i64> {
        let text = value.get(range)?;
        if text.bytes().all(|byte| byte.is_ascii_digit()) {
            text.parse::<i64>().ok()
        } else {
            None
        }
    };
    let (Some(year), Some(month), Some(day), Some(hour), Some(minute), Some(second), Some(millis)) = (
        field(0..4),
        field(5..7),
        field(8..10),
        field(11..13),
        field(14..16),
        field(17..19),
        field(20..23),
    ) else {
        return Err(invalid());
    };
    if year < 1970
        || !(1..=12).contains(&month)
        || day < 1
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return Err(invalid());
    }
    let days = days_from_civil(year, month, day);
    let days = u64::try_from(days).map_err(|_| invalid())?;
    let ms = days * 86_400_000
        + u64::try_from(hour * 3_600_000 + minute * 60_000 + second * 1_000 + millis)
            .map_err(|_| invalid())?;
    Ok(ms)
}

/// Formats Unix milliseconds as `A2AB-Ts` (RFC 3339 UTC, millisecond precision, `Z`).
/// Returns `None` for instants after the year 9999.
#[must_use]
pub fn format_ts(unix_ms: u64) -> Option<String> {
    let days = i64::try_from(unix_ms / 86_400_000).ok()?;
    let rest = unix_ms % 86_400_000;
    let (year, month, day) = civil_from_days(days);
    if !(0..=9999).contains(&year) {
        return None;
    }
    let hour = rest / 3_600_000;
    let minute = (rest % 3_600_000) / 60_000;
    let second = (rest % 60_000) / 1_000;
    let millis = rest % 1_000;
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z"
    ))
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if is_leap(year) {
                29
            } else {
                28
            }
        }
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let year_of_era = y - era * 400;
    let shifted_month = (month + 9) % 12;
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Inverse of [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Parses an unsigned decimal header such as `A2AB-Ttl-Ms`, `A2AB-After-Seq` or `A2AB-Last-Seq`.
/// Only ASCII digits are accepted: no sign, no whitespace, no exponent.
///
/// # Errors
/// Returns [`HeaderError`] with [`Kind::Invalid`] when the value is not a decimal in u64 range.
pub fn parse_u64(field: &str, value: &str) -> Result<u64, HeaderError> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(HeaderError::new(field, Kind::Invalid));
    }
    value
        .parse::<u64>()
        .map_err(|_| HeaderError::new(field, Kind::Invalid))
}

/// Parses `A2AB-Ttl-Ms`, an integer in `[MIN_TTL_MS, MAX_TTL_MS]` (RFC-0002 [0002-070]).
///
/// # Errors
/// Returns [`HeaderError`] with [`Kind::Invalid`] for a non-decimal value or one out of range.
pub fn parse_ttl_ms(value: &str) -> Result<u64, HeaderError> {
    let ttl = parse_u64(TTL_MS, value)?;
    if !(MIN_TTL_MS..=MAX_TTL_MS).contains(&ttl) {
        return Err(HeaderError::new(TTL_MS, Kind::Invalid));
    }
    Ok(ttl)
}

/// Reads `A2AB-Stream-End`: absent means false, `true` means true, anything else is invalid
/// (RFC-0002 [0002-044]).
///
/// # Errors
/// Returns [`HeaderError`] with [`Kind::Invalid`] for any value other than `true`.
pub fn parse_stream_end(headers: &Headers) -> Result<bool, HeaderError> {
    match headers.get(STREAM_END) {
        None => Ok(false),
        Some("true") => Ok(true),
        Some(_) => Err(HeaderError::new(STREAM_END, Kind::Invalid)),
    }
}

/// Checks the `A2A-Version` header: it must be present, non-empty and equal to
/// [`a2a_types::vocab::A2A_VERSION`] (RFC-0002 [0002-080]). There is no silent fallback.
///
/// # Errors
/// Returns [`WireError::VersionNotSupported`] for a missing, empty or different version.
pub fn check_version(value: Option<&str>) -> Result<(), WireError> {
    match value {
        Some(version) if version == a2a_types::vocab::A2A_VERSION => Ok(()),
        _ => Err(WireError::VersionNotSupported),
    }
}

/// The header name that carries service parameter `key` (RFC-0002 [0002-082]).
///
/// `Version` and `Extensions` (case-insensitive) map to `A2A-Version` and `A2A-Extensions`.
/// Every other key `K` maps to `A2AB-SP-<k>`, where `<k>` is `K` lowercased and must match
/// `[a-z0-9-]{1,48}`.
///
/// # Errors
/// Returns [`HeaderError`] when `key` cannot be represented as a header name.
pub fn service_param_header(key: &str) -> Result<String, HeaderError> {
    let field = format!("{SP_PREFIX}{key}");
    if key.eq_ignore_ascii_case("version") {
        return Ok(A2A_VERSION.to_owned());
    }
    if key.eq_ignore_ascii_case("extensions") {
        return Ok(A2A_EXTENSIONS.to_owned());
    }
    let lower = key.to_ascii_lowercase();
    let valid = (1..=48).contains(&lower.len())
        && lower
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if !valid {
        return Err(HeaderError::new(&field, Kind::Invalid));
    }
    Ok(format!("{SP_PREFIX}{lower}"))
}

/// Reads the carried service parameters from `headers` as `(key, value)` pairs, with keys
/// lowercased. `A2A-Version` and `A2A-Extensions` are parsed elsewhere and are not returned.
///
/// # Errors
/// Returns [`HeaderError`] when a `A2AB-SP-` header has an invalid key or value.
pub fn service_params(headers: &Headers) -> Result<Vec<(String, String)>, HeaderError> {
    let mut params = Vec::new();
    for (name, value) in headers.iter() {
        let Some(key) = name.strip_prefix(SP_PREFIX) else {
            continue;
        };
        let valid_key = (1..=48).contains(&key.len())
            && key
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
        if !valid_key {
            return Err(HeaderError::new(name, Kind::Invalid));
        }
        validate_value(name, value)?;
        params.push((key.to_owned(), value.to_owned()));
    }
    Ok(params)
}

fn is_lower_hex(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> Headers {
        let mut headers = Headers::new();
        for (name, value) in pairs {
            assert!(headers.insert(name, value).is_ok(), "insert {name}");
        }
        headers
    }

    const MSG: &str = "0196f3a2-0b7e-7c3d-a1f0-2d9e4b6c8a10";

    #[test]
    fn req_0002_033_printable_ascii_up_to_1024_bytes_is_accepted() {
        assert!(validate_value(SENDER, "hermes").is_ok());
        assert!(
            validate_value(SENDER, "a b~").is_ok(),
            "space and tilde are printable"
        );
        let max = "x".repeat(MAX_VALUE_LEN);
        assert!(validate_value(SENDER, &max).is_ok());
    }

    #[test]
    fn req_0002_033_value_over_1024_bytes_is_rejected() {
        let too_long = "x".repeat(MAX_VALUE_LEN + 1);
        let error = validate_value(SENDER, &too_long);
        assert_eq!(
            error.map_err(|e| e.field().to_owned()),
            Err(SENDER.to_owned())
        );
    }

    #[test]
    fn req_0002_033_control_and_non_ascii_bytes_are_rejected() {
        for bad in ["a\nb", "a\rb", "a\0b", "a\tb", "caf\u{e9}", "\u{7f}"] {
            assert!(
                validate_value(SENDER, bad).is_err(),
                "{bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn req_0002_033_traceparent_follows_w3c_format() {
        let good = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        assert!(validate_traceparent(good).is_ok());
        let short = &good[..54];
        assert!(validate_traceparent(short).is_err());
        let long_v00 = format!("{good}-extra");
        assert!(
            validate_traceparent(&long_v00).is_err(),
            "version 00 is exactly 55 bytes"
        );
        let upper = good.to_uppercase();
        assert!(
            validate_traceparent(&upper).is_err(),
            "hex must be lowercase"
        );
        let zero_trace = "00-00000000000000000000000000000000-00f067aa0ba902b7-01";
        assert!(validate_traceparent(zero_trace).is_err());
        let version_ff = "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        assert!(validate_traceparent(version_ff).is_err());
    }

    #[test]
    fn req_0002_033_tracestate_limits_members_and_length() {
        assert!(validate_tracestate("rojo=00f067aa0ba902b7,congo=t61rcWkgMzE").is_ok());
        let many = (0..33)
            .map(|i| format!("k{i}=v"))
            .collect::<Vec<_>>()
            .join(",");
        assert!(validate_tracestate(&many).is_err(), "more than 32 members");
        let long = format!("a={}", "x".repeat(511));
        assert!(validate_tracestate(&long).is_err(), "more than 512 bytes");
    }

    #[test]
    fn req_0002_032_unknown_headers_are_not_inspected() {
        let mut h = headers(&[(MSG_ID, MSG)]);
        assert!(h.insert("X-Vendor-Thing", "ok").is_ok());
        assert!(validate_all(&h).is_ok());
    }

    #[test]
    fn req_0002_033_validate_all_rejects_bad_known_header() {
        let mut h = headers(&[(MSG_ID, MSG)]);
        assert!(
            h.insert(PAYLOAD_TYPE, "bad\tvalue").is_ok(),
            "bus accepts a tab; the binding grammar does not"
        );
        assert!(validate_all(&h).is_err());
    }

    #[test]
    fn req_0002_013_authorization_header_is_forbidden() {
        let h = headers(&[(AUTHORIZATION, "Bearer x")]);
        let error = reject_authorization(&h);
        assert!(matches!(error, Err(ref e) if e.field() == AUTHORIZATION));
        assert!(reject_authorization(&headers(&[])).is_ok());
    }

    #[test]
    fn req_0002_030_required_treats_empty_as_missing() {
        let h = headers(&[(SENDER, "")]);
        assert_eq!(
            required(&h, SENDER).map_err(|e| e.field().to_owned()),
            Err(SENDER.to_owned())
        );
        assert!(required(&h, TS).is_err());
        assert_eq!(optional(&h, SENDER), None);
    }

    #[test]
    fn req_0002_031_msg_id_must_be_uuidv7() {
        assert!(parse_msg_id(MSG_ID, MSG).is_ok());
        assert!(
            parse_msg_id(MSG_ID, &MSG.to_uppercase()).is_err(),
            "lowercase only"
        );
        assert!(parse_msg_id(MSG_ID, "not-a-uuid").is_err());
        assert!(
            parse_msg_id(MSG_ID, "550e8400-e29b-41d4-a716-446655440000").is_err(),
            "UUIDv4 is not UUIDv7"
        );
    }

    #[test]
    fn req_0002_011_sender_must_be_a_valid_agent_id() {
        assert!(parse_agent_id(SENDER, "gateway").is_ok());
        assert!(parse_agent_id(SENDER, "Bad_Sender").is_err());
    }

    #[test]
    fn req_0002_070_ts_round_trips_millisecond_utc() {
        let ms = parse_ts("2026-03-14T10:22:30.912Z");
        assert!(ms.is_ok());
        let ms = ms.unwrap_or_default();
        assert_eq!(format_ts(ms).as_deref(), Some("2026-03-14T10:22:30.912Z"));
        assert_eq!(parse_ts("1970-01-01T00:00:00.000Z").ok(), Some(0));
        assert_eq!(
            parse_ts("2001-09-09T01:46:40.000Z").ok(),
            Some(1_000_000_000_000)
        );
        assert_eq!(
            format_ts(1_000_000_000_123).as_deref(),
            Some("2001-09-09T01:46:40.123Z")
        );
    }

    #[test]
    fn req_0002_070_ts_rejects_other_forms_and_impossible_dates() {
        for bad in [
            "2026-03-14T10:22:30Z",
            "2026-03-14T10:22:30.912+00:00",
            "2026-03-14t10:22:30.912Z",
            "2026-02-29T00:00:00.000Z",
            "2026-13-01T00:00:00.000Z",
            "2026-03-14T24:00:00.000Z",
            "2026-03-14T10:60:00.000Z",
            "2026-03-14T10:22:60.000Z",
            "1969-12-31T23:59:59.999Z",
            "2026-03-14T10:22:30.91aZ",
            "",
        ] {
            assert!(parse_ts(bad).is_err(), "{bad} must be rejected");
        }
    }

    #[test]
    fn req_0002_070_ts_accepts_leap_day_in_leap_year() {
        assert!(parse_ts("2028-02-29T00:00:00.000Z").is_ok());
        assert!(parse_ts("2000-02-29T00:00:00.000Z").is_ok());
        assert!(
            parse_ts("1900-02-29T00:00:00.000Z").is_err(),
            "1900 is not a leap year"
        );
    }

    #[test]
    fn req_0002_070_ttl_is_an_integer_in_range() {
        assert_eq!(parse_ttl_ms("1").ok(), Some(1));
        assert_eq!(parse_ttl_ms("300000").ok(), Some(300_000));
        assert_eq!(parse_ttl_ms("3600000").ok(), Some(3_600_000));
        assert!(parse_ttl_ms("0").is_err());
        assert!(parse_ttl_ms("3600001").is_err());
        assert!(parse_ttl_ms("-1").is_err());
        assert!(parse_ttl_ms("+5").is_err());
        assert!(parse_ttl_ms("1.5").is_err());
        assert!(parse_ttl_ms("").is_err());
        assert!(parse_ttl_ms("99999999999999999999999").is_err());
    }

    #[test]
    fn req_0002_061_seq_headers_are_u64_decimal() {
        assert_eq!(parse_u64(AFTER_SEQ, "2").ok(), Some(2));
        assert_eq!(
            parse_u64(AFTER_SEQ, "18446744073709551615").ok(),
            Some(u64::MAX)
        );
        assert!(
            parse_u64(AFTER_SEQ, "18446744073709551616").is_err(),
            "overflow"
        );
        assert!(parse_u64(AFTER_SEQ, " 2").is_err());
        assert!(parse_u64(AFTER_SEQ, "0x2").is_err());
    }

    #[test]
    fn req_0002_044_stream_end_is_only_true() {
        assert_eq!(parse_stream_end(&headers(&[])).ok(), Some(false));
        assert_eq!(
            parse_stream_end(&headers(&[(STREAM_END, "true")])).ok(),
            Some(true)
        );
        assert!(parse_stream_end(&headers(&[(STREAM_END, "false")])).is_err());
        assert!(parse_stream_end(&headers(&[(STREAM_END, "TRUE")])).is_err());
    }

    #[test]
    fn req_0002_080_version_must_be_exactly_1_0() {
        assert!(check_version(Some("1.0")).is_ok());
        assert_eq!(check_version(None), Err(WireError::VersionNotSupported));
        assert_eq!(check_version(Some("")), Err(WireError::VersionNotSupported));
        assert_eq!(
            check_version(Some("0.3")),
            Err(WireError::VersionNotSupported)
        );
    }

    #[test]
    fn req_0002_082_service_param_names_follow_the_mapping() {
        assert_eq!(
            service_param_header("Version").ok().as_deref(),
            Some(A2A_VERSION)
        );
        assert_eq!(
            service_param_header("extensions").ok().as_deref(),
            Some(A2A_EXTENSIONS)
        );
        assert_eq!(
            service_param_header("Tenant-Hint").ok().as_deref(),
            Some("A2AB-SP-tenant-hint")
        );
        assert!(service_param_header("").is_err());
        assert!(service_param_header("bad_key").is_err());
        assert!(service_param_header("a.b").is_err());
        let too_long = "k".repeat(49);
        assert!(service_param_header(&too_long).is_err());
        assert!(service_param_header(&"k".repeat(48)).is_ok());
    }

    #[test]
    fn req_0002_082_service_params_are_read_back_lowercased() {
        let h = headers(&[
            ("A2AB-SP-tenant-hint", "eu"),
            (SENDER, "gateway"),
            (A2A_VERSION, "1.0"),
        ]);
        let params = service_params(&h).ok().unwrap_or_default();
        assert_eq!(params, vec![("tenant-hint".to_owned(), "eu".to_owned())]);
    }

    #[test]
    fn req_0002_082_service_param_value_over_1024_is_rejected() {
        let long = "v".repeat(MAX_VALUE_LEN + 1);
        let h = headers(&[("A2AB-SP-hint", long.as_str())]);
        assert!(service_params(&h).is_err());
    }
}
