//! Display sanitisation for operator-visible text.
//!
//! Workers redact before they publish (RFC-0013 R-A2); the console applies the
//! same rules again as defense in depth. Control characters are escaped so that
//! no agent-supplied text can move the cursor, change colours, or reach the
//! clipboard (RFC-0012 0012-049).

/// Longest argument summary shown on an approval card ([0013-022]).
pub const SUMMARY_MAX_CHARS: usize = 512;
/// Longest operator note accepted with a denial (RFC-0012 0012-046).
pub const NOTE_MAX_CHARS: usize = 280;
/// Replacement for credential-like values ([0013-023]).
pub const REDACTED: &str = "[redacted]";
/// Replacement for high-entropy tokens ([0013-024]).
pub const REDACTED_ENTROPY: &str = "[redacted:entropy]";

const SECRET_KEYS: [&str; 9] = [
    "token",
    "secret",
    "password",
    "passwd",
    "pwd",
    "api_key",
    "apikey",
    "authorization",
    "credential",
];

const SECRET_PREFIXES: [&str; 5] = ["sk-", "ghp_", "AKIA", "xox", "glpat-"];

/// Escapes every control character as `\u{..}` so the text is inert on a terminal.
#[must_use]
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_control() {
            out.push_str(&format!("\\u{{{:x}}}", u32::from(ch)));
        } else {
            out.push(ch);
        }
    }
    out
}

/// Builds the display form of an argument summary: whitespace normalised, control
/// characters escaped, credentials and high-entropy tokens redacted, and the
/// result capped at [`SUMMARY_MAX_CHARS`] characters.
#[must_use]
pub fn summary(raw: &str) -> String {
    let normalized = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let escaped = escape(&normalized);
    let redacted = redact_tokens(&escaped);
    truncate_chars(&redacted, SUMMARY_MAX_CHARS)
}

/// Display form of an operator note: escaped and capped at [`NOTE_MAX_CHARS`].
#[must_use]
pub fn note(raw: &str) -> String {
    truncate_chars(&escape(raw.trim()), NOTE_MAX_CHARS)
}

/// Truncates to `max` characters, ending with `...` when cut.
#[must_use]
pub fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let keep = max.saturating_sub(3);
    let mut out: String = text.chars().take(keep).collect();
    out.push_str("...");
    out
}

fn redact_tokens(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut redact_next: u8 = 0;
    let mut in_key_block = false;
    for token in text.split(' ') {
        if token.is_empty() {
            continue;
        }
        if in_key_block {
            if token.contains("-----END") {
                in_key_block = false;
            }
            out.push(REDACTED.to_owned());
            continue;
        }
        if token.starts_with("-----BEGIN") {
            in_key_block = !token.contains("-----END");
            out.push(REDACTED.to_owned());
            continue;
        }
        if redact_next > 0 {
            redact_next -= 1;
            if is_scheme_word(token) {
                // `Authorization: Bearer <value>`: the value follows the scheme word.
                redact_next += 1;
                out.push(token.to_owned());
            } else {
                out.push(REDACTED.to_owned());
            }
            continue;
        }
        if let Some(rewritten) = redact_key_value(token) {
            out.push(rewritten);
            continue;
        }
        if let Some(rewritten) = redact_url_userinfo(token) {
            out.push(rewritten);
            continue;
        }
        if let Some(keyword) = secret_key_colon(token) {
            out.push(keyword.to_owned());
            redact_next = 1;
            continue;
        }
        if has_secret_prefix(token) || is_high_entropy(token) {
            out.push(if is_high_entropy(token) && !has_secret_prefix(token) {
                REDACTED_ENTROPY.to_owned()
            } else {
                REDACTED.to_owned()
            });
            continue;
        }
        out.push(token.to_owned());
    }
    out.join(" ")
}

fn is_scheme_word(token: &str) -> bool {
    token.eq_ignore_ascii_case("bearer") || token.eq_ignore_ascii_case("basic")
}

/// `name=value` where `name` is secret-like: the value is replaced.
fn redact_key_value(token: &str) -> Option<String> {
    let (name, _value) = token.split_once('=')?;
    if is_secret_name(name) {
        Some(format!("{name}={REDACTED}"))
    } else {
        None
    }
}

/// `password:` written as a separate token: the next token is the secret.
fn secret_key_colon(token: &str) -> Option<&str> {
    let name = token.strip_suffix(':')?;
    if is_secret_name(name) {
        Some(token)
    } else {
        None
    }
}

fn is_secret_name(name: &str) -> bool {
    let lowered = name
        .trim_matches(|c: char| matches!(c, '"' | '\'' | '{' | '(' | '-' | '_' | ','))
        .to_ascii_lowercase();
    SECRET_KEYS.iter().any(|key| lowered.contains(key))
}

/// `scheme://user:password@host` becomes `scheme://[redacted]@host`.
fn redact_url_userinfo(token: &str) -> Option<String> {
    let (scheme, rest) = token.split_once("://")?;
    let (userinfo, host) = rest.split_once('@')?;
    if userinfo.contains(':') {
        Some(format!("{scheme}://{REDACTED}@{host}"))
    } else {
        None
    }
}

fn has_secret_prefix(token: &str) -> bool {
    let trimmed = trim_punct(token);
    trimmed.len() >= 12 && SECRET_PREFIXES.iter().any(|p| trimmed.starts_with(p))
}

/// Heuristic for opaque credentials: long, mixed letters and digits, drawn from the
/// base64/hex/url-safe alphabets, with enough distinct characters.
fn is_high_entropy(token: &str) -> bool {
    let trimmed = trim_punct(token);
    if trimmed.len() < 24 {
        return false;
    }
    let charset_ok = trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '_' | '-'));
    if !charset_ok {
        return false;
    }
    let has_letter = trimmed.chars().any(|c| c.is_ascii_alphabetic());
    let has_digit = trimmed.chars().any(|c| c.is_ascii_digit());
    let mut distinct: Vec<char> = trimmed.chars().collect();
    distinct.sort_unstable();
    distinct.dedup();
    has_letter && has_digit && distinct.len() >= 12
}

fn trim_punct(token: &str) -> &str {
    token.trim_matches(|c: char| matches!(c, '"' | '\'' | ',' | ';' | '(' | ')' | '[' | ']'))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn req_0013_022_args_summary_is_limited_to_512_chars() {
        let raw = "x".repeat(2_000);
        let out = summary(&raw);
        assert_eq!(out.chars().count(), SUMMARY_MAX_CHARS);
        assert!(out.ends_with("..."));
    }

    #[test]
    fn req_0013_022_summary_normalises_whitespace() {
        assert_eq!(summary("  write\t\tfile   a.txt\n"), "write file a.txt");
    }

    #[test]
    fn req_0013_023_credential_values_are_redacted() {
        assert_eq!(summary("curl --token=abc123"), "curl --token=[redacted]");
        assert_eq!(
            summary("deploy password: hunter2 now"),
            "deploy password: [redacted] now"
        );
        assert_eq!(
            summary("header Authorization: Bearer xyz987"),
            "header Authorization: Bearer [redacted]"
        );
        assert_eq!(
            summary("git clone https://user:s3cret@example.org/repo"),
            "git clone https://[redacted]@example.org/repo"
        );
        assert_eq!(summary("use sk-abcdefghijklmnop"), "use [redacted]");
    }

    #[test]
    fn req_0013_023_private_key_blocks_are_redacted() {
        let out = summary("-----BEGIN PRIVATE KEY----- MIIE abc -----END PRIVATE KEY-----");
        assert!(!out.contains("MIIE"));
        assert!(out.contains(REDACTED));
    }

    #[test]
    fn req_0013_024_high_entropy_tokens_are_redacted() {
        let token = "Qz9fK2mP8xR4vN7sL1wT6yB3hD0jG5cA";
        let out = summary(&format!("fetch {token}"));
        assert_eq!(out, format!("fetch {REDACTED_ENTROPY}"));
        assert_eq!(summary("read main.rs"), "read main.rs");
    }

    #[test]
    fn req_0012_049_escapes_control_sequences() {
        let raw = "ok\u{1b}[31mred\u{7}\rline";
        let out = escape(raw);
        assert!(out.chars().all(|c| !c.is_control()));
        assert!(out.contains("\\u{1b}"));
        assert!(out.contains("\\u{7}"));
        assert!(out.contains("\\u{d}"));
    }

    #[test]
    fn req_0012_046_operator_note_is_capped_at_280_chars() {
        let out = note(&"n".repeat(400));
        assert_eq!(out.chars().count(), NOTE_MAX_CHARS);
    }
}
