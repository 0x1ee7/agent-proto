//! Best-effort redaction of credential-shaped text in events and errors (RFC-0009 0009-070).

/// Maximum characters kept in a detail string.
const DETAIL_CHARS: usize = 300;

/// Replaces credential-shaped words with `[redacted]`, keeping all other text and whitespace.
pub(crate) fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut redact_next = false;
    let mut rest = text;
    while !rest.is_empty() {
        let whitespace = rest.starts_with(char::is_whitespace);
        let end = rest
            .find(|c: char| c.is_whitespace() != whitespace)
            .unwrap_or(rest.len());
        let (token, tail) = rest.split_at(end);
        rest = tail;
        if whitespace {
            out.push_str(token);
            continue;
        }
        if redact_next || is_credential(token) {
            out.push_str("[redacted]");
        } else {
            out.push_str(token);
        }
        redact_next = token.eq_ignore_ascii_case("bearer");
    }
    out
}

/// Redacts and bounds `text` for use as an event detail.
pub(crate) fn detail(text: &str) -> String {
    let redacted = redact(text);
    match redacted.char_indices().nth(DETAIL_CHARS) {
        Some((cut, _)) => {
            let mut bounded = redacted[..cut].to_owned();
            bounded.push_str("...");
            bounded
        }
        None => redacted,
    }
}

fn is_credential(token: &str) -> bool {
    let trimmed = token.trim_matches(|c: char| c == '"' || c == '\'' || c == ',' || c == ';');
    if trimmed.starts_with("sk-")
        || trimmed.starts_with("ghp_")
        || trimmed.starts_with("github_pat_")
        || trimmed.starts_with("xox")
        || trimmed.starts_with("AKIA")
    {
        return true;
    }
    if let Some((key, _)) = trimmed.split_once('=') {
        let upper = key.to_ascii_uppercase();
        return ["KEY", "TOKEN", "SECRET", "PASSWORD"]
            .iter()
            .any(|marker| upper.contains(marker));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_known_token_shapes_and_keeps_text_0009_070() {
        let out = redact("use sk-abcdef123456 and ghp_xyz now");
        assert_eq!(out, "use [redacted] and [redacted] now");
    }

    #[test]
    fn redacts_bearer_value_and_key_assignments() {
        assert_eq!(
            redact("Authorization: Bearer abc.def"),
            "Authorization: Bearer [redacted]"
        );
        assert_eq!(redact("API_TOKEN=hunter2 ok"), "[redacted] ok");
    }

    #[test]
    fn keeps_plain_text_and_whitespace() {
        assert_eq!(redact("line one\n  line two"), "line one\n  line two");
    }

    #[test]
    fn detail_is_bounded() {
        let long = "a".repeat(400);
        let bounded = detail(&long);
        assert!(bounded.ends_with("..."));
        assert_eq!(bounded.chars().count(), 303);
    }
}
