//! Result text hygiene: a redaction hook and UTF-8-safe truncation.
//!
//! The crate does not depend on `a2a-security` (RFC-0015 [0015-002]); callers
//! inject redaction through [`Redactor`]. The default [`NoRedaction`] passes
//! text unchanged.

/// Redacts secret-shaped material from text before it reaches the MCP client.
pub trait Redactor: Send + Sync {
    /// Returns the redacted form of `text`.
    fn redact(&self, text: &str) -> String;
}

/// Redactor that passes text through unchanged.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoRedaction;

impl Redactor for NoRedaction {
    fn redact(&self, text: &str) -> String {
        text.to_owned()
    }
}

/// Keeps at most `max` bytes of `text`, cutting on a UTF-8 boundary.
///
/// Returns the kept prefix and the number of omitted bytes.
pub fn truncate_utf8(text: &str, max: usize) -> (&str, u64) {
    if text.len() <= max {
        return (text, 0);
    }
    let mut cut = max;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    (&text[..cut], (text.len() - cut) as u64)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    // RFC-0015 [0015-032]: text cap cut on UTF-8 boundary.
    #[test]
    fn truncation_respects_char_boundary_0015_032() {
        let text = "ab\u{e9}cd"; // 'é' is two bytes at offset 2..4
        let (kept, omitted) = truncate_utf8(text, 3);
        assert_eq!(kept, "ab");
        assert_eq!(omitted, 4);
        let (kept, omitted) = truncate_utf8(text, 10);
        assert_eq!(kept, text);
        assert_eq!(omitted, 0);
    }
}
