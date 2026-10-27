//! Conversions between `bus-api` headers and NATS headers, and the per-message
//! size budget (0010-015).

use async_nats::HeaderMap;
use bus_api::error::BusError;
use bus_api::headers::Headers;
use bytes::Bytes;

use crate::layout::{MAX_BODY_BYTES, MAX_WIRE_BYTES};
use crate::principal::PRINCIPAL_HEADER;

/// The NATS header status line, `NATS/1.0\r\n`.
const STATUS_LINE_LEN: usize = 10;
/// The blank line that ends a header block.
const HEADER_END_LEN: usize = 2;

/// Stamps `identity` as the reserved principal header, replacing any value the caller
/// set, so the stream message carries its publisher (see `principal`). Applied before
/// the budget check, so the stamped header counts toward the limit.
pub(crate) fn stamp_principal(headers: &Headers, identity: &str) -> Result<Headers, BusError> {
    let mut out = headers.clone();
    out.insert(PRINCIPAL_HEADER, identity)
        .map_err(|_| BusError::Internal)?;
    Ok(out)
}

/// `headers` without the reserved principal header, which the caller never set.
pub(crate) fn without_principal(headers: &Headers) -> Headers {
    let mut out = Headers::new();
    for (name, value) in headers.iter() {
        if name != PRINCIPAL_HEADER {
            // Names and values were validated when `headers` was built.
            let _ = out.insert(name, value);
        }
    }
    out
}

/// Checks the body limit and the headers-plus-body limit before sending.
pub(crate) fn check_budget(headers: &Headers, payload: &Bytes) -> Result<(), BusError> {
    if payload.len() > MAX_BODY_BYTES {
        return Err(BusError::PayloadTooLarge {
            limit: MAX_BODY_BYTES,
        });
    }
    if wire_size(headers, payload.len()) > MAX_WIRE_BYTES {
        return Err(BusError::PayloadTooLarge {
            limit: MAX_WIRE_BYTES,
        });
    }
    Ok(())
}

/// Bytes a message occupies on the wire: header block plus body. A message without
/// headers carries no header block.
pub(crate) fn wire_size(headers: &Headers, body_len: usize) -> usize {
    let block = if headers.is_empty() {
        0
    } else {
        // Each line is `name: value\r\n`.
        let lines: usize = headers.iter().map(|(k, v)| k.len() + v.len() + 4).sum();
        STATUS_LINE_LEN + lines + HEADER_END_LEN
    };
    block + body_len
}

/// Copies bus headers into a NATS header map. Names and values were validated by
/// `bus-api::Headers`, so this never drops an entry.
pub(crate) fn to_header_map(headers: &Headers) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in headers.iter() {
        map.insert(name, value);
    }
    map
}

/// Converts inbound NATS headers. Returns `None` when a name or value cannot be
/// represented in `bus-api::Headers` (a header with several values, or one that
/// is not valid); the caller then drops the message.
pub(crate) fn from_header_map(map: &HeaderMap) -> Option<Headers> {
    let mut out = Headers::new();
    for (name, values) in map.iter() {
        let [value] = values.as_slice() else {
            return None;
        };
        out.insert(name.as_ref(), value.as_str()).ok()?;
    }
    Some(out)
}

/// Inbound headers of an optional NATS header map.
pub(crate) fn inbound_headers(map: Option<&HeaderMap>) -> Option<Headers> {
    map.map_or_else(|| Some(Headers::new()), from_header_map)
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> Headers {
        let mut h = Headers::new();
        for (k, v) in pairs {
            h.insert(k, v).expect("valid header");
        }
        h
    }

    // 0010-015: body limit of 524288 bytes.
    #[test]
    fn req_0010_015_body_over_limit_is_rejected() {
        let body = Bytes::from(vec![0u8; MAX_BODY_BYTES + 1]);
        assert_eq!(
            check_budget(&Headers::new(), &body),
            Err(BusError::PayloadTooLarge {
                limit: MAX_BODY_BYTES
            })
        );
    }

    // 0010-015: body at the limit with no headers is accepted.
    #[test]
    fn req_0010_015_body_at_limit_is_accepted() {
        let body = Bytes::from(vec![0u8; MAX_BODY_BYTES]);
        assert_eq!(check_budget(&Headers::new(), &body), Ok(()));
    }

    // 0010-015: headers plus body must fit in 1048576 bytes.
    #[test]
    fn req_0010_015_headers_plus_body_over_max_payload_is_rejected() {
        let h = headers(&[("x-pad", "a")]);
        let body = Bytes::from(vec![0u8; MAX_BODY_BYTES]);
        assert_eq!(check_budget(&h, &body), Ok(()));
        let big = "v".repeat(600_000);
        let h = headers(&[("x-a", big.as_str()), ("x-b", big.as_str())]);
        let small = Bytes::from_static(b"x");
        assert_eq!(
            check_budget(&h, &small),
            Err(BusError::PayloadTooLarge {
                limit: MAX_WIRE_BYTES
            })
        );
    }

    #[test]
    fn wire_size_counts_status_line_and_terminator() {
        // "NATS/1.0\r\n" (10) + "a: b\r\n" (6) + "\r\n" (2) + 3 body bytes.
        let h = headers(&[("a", "b")]);
        assert_eq!(wire_size(&h, 3), 10 + 6 + 2 + 3);
        assert_eq!(wire_size(&Headers::new(), 3), 3);
    }

    #[test]
    fn header_round_trip_keeps_every_entry() {
        let h = headers(&[("a2a-trace", "t-1"), ("x-kind", "ev")]);
        let back = from_header_map(&to_header_map(&h)).expect("round trip");
        assert_eq!(back, h);
    }

    #[test]
    fn inbound_header_with_two_values_is_refused() {
        let mut map = HeaderMap::new();
        map.insert("x-a", "1");
        map.append("x-a", "2");
        assert!(from_header_map(&map).is_none());
    }
}
