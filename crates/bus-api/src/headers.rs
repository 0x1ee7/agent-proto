//! Message headers: ASCII names and printable ASCII values, byte-exact on round trip.

use std::collections::BTreeMap;

use crate::name::InvalidName;

/// Ordered, case-sensitive header map. Values are ASCII text without control
/// characters, so they survive a round trip through any binding unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Headers(BTreeMap<String, String>);

impl Headers {
    /// An empty header set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets `name` to `value`, replacing any previous value.
    ///
    /// # Errors
    /// Returns [`InvalidName`] when the name is empty, longer than 255 bytes,
    /// or contains anything but `[A-Za-z0-9-]`; or when the value contains a
    /// byte outside printable ASCII, space or tab.
    pub fn insert(&mut self, name: &str, value: &str) -> Result<(), InvalidName> {
        check_header_name(name)?;
        check_header_value(value)?;
        self.0.insert(name.to_owned(), value.to_owned());
        Ok(())
    }

    /// The value of `name`, matched case-sensitively.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    /// Number of headers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether no headers are set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Iterates `(name, value)` in ascending name order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
}

fn check_header_name(name: &str) -> Result<(), InvalidName> {
    if name.is_empty() {
        return Err(InvalidName::new("header name", "empty"));
    }
    if name.len() > 255 {
        return Err(InvalidName::new("header name", "longer than 255 bytes"));
    }
    if name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        Ok(())
    } else {
        Err(InvalidName::new("header name", "only [A-Za-z0-9-] allowed"))
    }
}

fn check_header_value(value: &str) -> Result<(), InvalidName> {
    if value
        .bytes()
        .all(|b| b == b'\t' || (b' '..0x7f).contains(&b))
    {
        Ok(())
    } else {
        Err(InvalidName::new(
            "header value",
            "must be printable ASCII, space or tab",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // [0011-003] header values survive a round trip byte-exact.
    #[test]
    fn req_0011_003_header_round_trip_is_exact() {
        let mut headers = Headers::new();
        let value = "0190f3a2-7c1d-7e00-8000-000000000001";
        assert!(headers.insert("A2AB-Msg-Id", value).is_ok());
        assert_eq!(headers.get("A2AB-Msg-Id"), Some(value));
        assert_eq!(headers.get("a2ab-msg-id"), None, "names are case-sensitive");
    }

    #[test]
    fn rejects_control_characters_and_crlf_in_values() {
        let mut headers = Headers::new();
        assert!(headers.insert("X-Test", "line\r\nInjected: 1").is_err());
        assert!(headers.insert("X-Test", "nul\0byte").is_err());
        assert!(headers.insert("X-Test", "tab\tok").is_ok());
        assert!(!headers.is_empty());
    }

    #[test]
    fn rejects_bad_header_names() {
        let mut headers = Headers::new();
        assert!(headers.insert("", "v").is_err());
        assert!(headers.insert("bad name", "v").is_err());
        assert!(headers.insert("under_score", "v").is_err());
    }

    #[test]
    fn insert_replaces_previous_value() {
        let mut headers = Headers::new();
        assert!(headers.insert("K", "1").is_ok());
        assert!(headers.insert("K", "2").is_ok());
        assert_eq!(headers.get("K"), Some("2"));
        assert_eq!(headers.len(), 1);
    }
}
