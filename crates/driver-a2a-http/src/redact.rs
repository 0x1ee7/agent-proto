//! Redaction of peer text before it leaves the driver (RFC-0009 0009-071, 0009-072).

/// Maximum length of a redacted detail string, in bytes.
pub const MAX_DETAIL_BYTES: usize = 300;

/// Replaces the bearer secret and any `Bearer <token>` text. Used for outbound text (0009-044),
/// which is sent in full.
pub fn scrub(text: &str, secret: Option<&str>) -> String {
    let out = match secret.filter(|s| s.len() >= 4) {
        Some(secret) => text.replace(secret, "<redacted>"),
        None => text.to_owned(),
    };
    redact_bearer(&out)
}

/// Scrubs secret text and bounds the result for detail strings (0009-070, 0009-072).
pub fn redact(text: &str, secret: Option<&str>) -> String {
    truncate(scrub(text, secret))
}

fn redact_bearer(text: &str) -> String {
    const PREFIX: &str = "bearer ";
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    while let Some(pos) = lower[cursor..].find(PREFIX) {
        let token_start = cursor + pos + PREFIX.len();
        let token_end = text[token_start..]
            .find(|c: char| c.is_whitespace() || c == '"' || c == ',' || c == '}')
            .map_or(text.len(), |offset| token_start + offset);
        out.push_str(&text[cursor..token_start]);
        out.push_str("<redacted>");
        cursor = token_end;
    }
    out.push_str(&text[cursor..]);
    out
}

fn truncate(mut text: String) -> String {
    if text.len() > MAX_DETAIL_BYTES {
        let mut end = MAX_DETAIL_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("...");
    }
    text
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// 0009-070: the bearer token never appears in a detail string.
    #[test]
    fn req_0009_070_strips_secret_and_bearer_text() {
        let text = r#"auth failed for "tok-secret-123", header Bearer tok-secret-123, other"#;
        let out = redact(text, Some("tok-secret-123"));
        assert!(!out.contains("tok-secret-123"), "{out}");
        assert!(out.contains("<redacted>"), "{out}");
    }

    /// 0009-044: outbound text is scrubbed of the secret but never truncated.
    #[test]
    fn req_0009_044_scrub_keeps_long_text_intact() {
        let long = "x".repeat(400) + " tok-secret-123";
        let out = scrub(&long, Some("tok-secret-123"));
        assert_eq!(out, format!("{} <redacted>", "x".repeat(400)));
    }

    #[test]
    fn bearer_pattern_without_known_secret_is_redacted() {
        let out = redact("sent Authorization: Bearer abc.def ok", None);
        assert_eq!(out, "sent Authorization: Bearer <redacted> ok");
    }

    /// 0009-072: details are bounded and never carry raw bodies in full.
    #[test]
    fn long_detail_is_truncated_on_char_boundary() {
        let long = "é".repeat(400);
        let out = redact(&long, None);
        assert!(out.ends_with("..."));
        assert!(out.len() <= MAX_DETAIL_BYTES + 3);
    }
}
