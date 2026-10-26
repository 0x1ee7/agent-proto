//! JCS (RFC 8785), SHA-256, hex and base64url helpers shared across the crate.

use std::fmt::Write as _;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::ser::{Serialize, SerializeMap, Serializer};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::error::SecurityError;

#[must_use]
pub fn b64_encode(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Decodes base64url without padding and rejects any non-canonical spelling.
#[must_use]
pub fn b64_decode_canonical(text: &str) -> Option<Vec<u8>> {
    let bytes = URL_SAFE_NO_PAD.decode(text).ok()?;
    (URL_SAFE_NO_PAD.encode(&bytes) == text).then_some(bytes)
}

#[must_use]
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    let digest = Sha256::digest(bytes);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

#[must_use]
pub fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// RFC 8785 canonical bytes of any serializable value.
pub fn jcs(value: &impl Serialize) -> Result<Vec<u8>, SecurityError> {
    serde_json_canonicalizer::to_vec(value)
        .map_err(|_| SecurityError::Canonical("serialization failed"))
}

struct WithoutMember<'a> {
    object: &'a Map<String, Value>,
    skip: &'a str,
}

impl Serialize for WithoutMember<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        for (key, value) in self.object {
            if key != self.skip {
                map.serialize_entry(key, value)?;
            }
        }
        map.end()
    }
}

/// JCS of `object` with one top-level member removed (signature bodies, [0007-035]).
pub fn jcs_without(object: &Map<String, Value>, skip: &str) -> Result<Vec<u8>, SecurityError> {
    jcs(&WithoutMember { object, skip })
}

/// True if any JSON number in `value` is a float literal. Signed statements must not contain them.
#[must_use]
pub fn contains_float(value: &Value) -> bool {
    match value {
        Value::Number(number) => number.is_f64(),
        Value::Array(items) => items.iter().any(contains_float),
        Value::Object(members) => members.values().any(contains_float),
        Value::Null | Value::Bool(_) | Value::String(_) => false,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn req_0007_035_b64_rejects_padded_and_standard_alphabet_spellings() {
        assert_eq!(b64_decode_canonical("AQI"), Some(vec![1, 2]));
        assert_eq!(b64_decode_canonical("AQI="), None);
        assert_eq!(b64_decode_canonical("+/8"), None);
        assert_eq!(b64_decode_canonical("AR"), None, "non-zero trailing bits");
    }

    #[test]
    fn req_0007_035_jcs_sorts_members_and_skips_named_member() {
        let value = json!({"b": 1, "a": {"y": true, "x": null}, "signature": "zz"});
        let object = value.as_object().expect("object");
        let bytes = jcs_without(object, "signature").expect("canonical");
        assert_eq!(
            String::from_utf8(bytes).expect("utf8"),
            r#"{"a":{"x":null,"y":true},"b":1}"#
        );
    }

    #[test]
    fn req_0007_092_floats_are_detected_at_any_depth() {
        assert!(contains_float(&json!({"n": 5.0})));
        assert!(contains_float(&json!({"list": [1, 2.0]})));
        assert!(contains_float(&json!({"nested": {"big": 1e300}})));
        assert!(!contains_float(&json!({"n": 5, "list": [1, 2]})));
        assert!(!contains_float(&json!({"text": "5.0"})));
    }

    #[test]
    fn req_0007_035_sha256_and_hex_match_known_vector() {
        assert_eq!(
            to_hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
