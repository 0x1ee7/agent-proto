//! RFC 8785 (JCS) canonical JSON for audit lines and hash inputs (RFC-0013 0013-018, 0013-026).
//!
//! Audit payloads carry no JSON numbers (details admit strings, booleans, lists and objects, and
//! proto3 JSON carries uint64 as strings), so only integers are formatted. Any other number is
//! refused instead of being formatted incorrectly.

use std::fmt::Write as _;

use serde_json::Value;
use sha2::{Digest, Sha256};

/// Canonicalization failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CanonError {
    #[error("JSON number {0} is not an integer; the audit canonical form admits integers only")]
    NonInteger(String),
}

/// Serializes `value` in RFC 8785 form: sorted keys (UTF-16 order), no whitespace.
pub fn canonical_string(value: &Value) -> Result<String, CanonError> {
    let mut out = String::new();
    write_value(&mut out, value)?;
    Ok(out)
}

/// [`canonical_string`] as UTF-8 bytes.
pub fn canonical_bytes(value: &Value) -> Result<Vec<u8>, CanonError> {
    canonical_string(value).map(String::into_bytes)
}

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn write_value(out: &mut String, value: &Value) -> Result<(), CanonError> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => {
            if let Some(unsigned) = number.as_u64() {
                out.push_str(&unsigned.to_string());
            } else if let Some(signed) = number.as_i64() {
                out.push_str(&signed.to_string());
            } else {
                return Err(CanonError::NonInteger(number.to_string()));
            }
        }
        Value::String(text) => write_string(out, text),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_value(out, item)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
            out.push('{');
            for (index, (key, item)) in entries.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_string(out, key);
                out.push(':');
                write_value(out, item)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

fn write_string(out: &mut String, text: &str) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn sorts_keys_by_utf16_and_strips_whitespace() {
        let value = json!({"b": 1, "a": [true, null, "x"], "é": "y"});
        assert_eq!(
            canonical_string(&value).unwrap_or_default(),
            r#"{"a":[true,null,"x"],"b":1,"é":"y"}"#
        );
    }

    #[test]
    fn escapes_control_characters_lowercase() {
        let value = json!({"k": "a\u{01}\n\"\\"});
        assert_eq!(
            canonical_string(&value).unwrap_or_default(),
            r#"{"k":"a\u0001\n\"\\"}"#
        );
    }

    #[test]
    fn refuses_non_integer_numbers() {
        let value = json!({"n": 1.5});
        assert!(matches!(
            canonical_string(&value),
            Err(CanonError::NonInteger(_))
        ));
    }
}
