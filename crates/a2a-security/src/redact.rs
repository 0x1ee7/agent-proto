//! Credential redaction before text reaches logs, audit events, egress or a model context
//! ([0007-130..133]). Patterns are data in this module; the RFC does not reproduce them.

use std::sync::LazyLock;

use regex::{Captures, Regex};

use crate::error::SecurityError;

/// Shannon entropy above which a long token counts as a credential ([0007-130](f)).
const ENTROPY_THRESHOLD_BITS: f64 = 4.5;
/// Minimum token length considered for the entropy rule ([0007-130](f)).
const ENTROPY_MIN_LEN: usize = 32;

/// Credential class named in the replacement `[REDACTED:<class>]` ([0007-131]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RedactionClass {
    ProviderKey,
    Bearer,
    PrivateKey,
    Jwt,
    CloudKeyId,
    HighEntropy,
}

impl RedactionClass {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProviderKey => "provider-key",
            Self::Bearer => "bearer",
            Self::PrivateKey => "private-key",
            Self::Jwt => "jwt",
            Self::CloudKeyId => "cloud-key-id",
            Self::HighEntropy => "high-entropy",
        }
    }
}

struct Redactor {
    patterns: Vec<(RedactionClass, Regex)>,
    token: Regex,
}

/// Compiled once. `None` records a compile failure so every call fails closed.
static REDACTOR: LazyLock<Option<Redactor>> = LazyLock::new(build);

/// Pattern table: order matters. Private-key blocks run first so their bodies are not
/// partially matched by later rules.
const PATTERNS: &[(RedactionClass, &str)] = &[
    (
        RedactionClass::PrivateKey,
        r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----[\s\S]*?(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|\z)",
    ),
    (
        RedactionClass::Bearer,
        r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]{8,}",
    ),
    (
        RedactionClass::Bearer,
        r"(?i)\bauthorization\s*[:=][^\r\n]*",
    ),
    (RedactionClass::ProviderKey, r"\bsk-[A-Za-z0-9_-]{16,}"),
    (RedactionClass::ProviderKey, r"\bAIza[0-9A-Za-z_-]{20,}"),
    (RedactionClass::ProviderKey, r"\bgh[pousr]_[A-Za-z0-9]{20,}"),
    (
        RedactionClass::ProviderKey,
        r"\bxox[abprs]-[A-Za-z0-9-]{10,}",
    ),
    (
        RedactionClass::Jwt,
        r"\beyJ[A-Za-z0-9_-]{5,}\.[A-Za-z0-9_-]{5,}\.[A-Za-z0-9_-]{5,}",
    ),
    (RedactionClass::CloudKeyId, r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"),
];

fn build() -> Option<Redactor> {
    let mut patterns = Vec::with_capacity(PATTERNS.len());
    for (class, source) in PATTERNS {
        patterns.push((*class, Regex::new(source).ok()?));
    }
    let token = Regex::new(&format!("[A-Za-z0-9+/_=-]{{{ENTROPY_MIN_LEN},}}")).ok()?;
    Some(Redactor { patterns, token })
}

fn redactor() -> Result<&'static Redactor, SecurityError> {
    REDACTOR.as_ref().ok_or(SecurityError::Invalid(
        "redaction patterns failed to compile",
    ))
}

/// Shannon entropy of `token` in bits per byte.
#[must_use]
fn shannon_bits_per_char(token: &str) -> f64 {
    let mut counts = [0usize; 256];
    for byte in token.bytes() {
        counts[usize::from(byte)] += 1;
    }
    let total = token.len() as f64;
    counts
        .iter()
        .filter(|&&count| count > 0)
        .map(|&count| {
            let p = count as f64 / total;
            -p * p.log2()
        })
        .sum()
}

/// Replaces credential-shaped substrings with `[REDACTED:<class>]` ([0007-130], [0007-131]).
/// The input is not modified; the result is a new string.
pub fn redact(text: &str) -> Result<String, SecurityError> {
    let redactor = redactor()?;
    let mut out = text.to_owned();
    for (class, pattern) in &redactor.patterns {
        let replacement = format!("[REDACTED:{}]", class.as_str());
        out = pattern.replace_all(&out, replacement.as_str()).into_owned();
    }
    let replacement = format!("[REDACTED:{}]", RedactionClass::HighEntropy.as_str());
    let out = redactor.token.replace_all(&out, |captures: &Captures<'_>| {
        let token = &captures[0];
        if shannon_bits_per_char(token) > ENTROPY_THRESHOLD_BITS {
            replacement.clone()
        } else {
            token.to_owned()
        }
    });
    Ok(out.into_owned())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{RedactionClass, redact, shannon_bits_per_char};

    #[test]
    fn req_0007_130_provider_keys_are_replaced() {
        let text = "a sk-abcdefghijklmnop1234 b AIzaSyD1234567890abcdefghijklmn c ghp_abcdefghijklmnopqrst1234 d xoxb-1234567890-abcdef e";
        let out = redact(text).expect("redact");
        assert_eq!(
            out,
            "a [REDACTED:provider-key] b [REDACTED:provider-key] c [REDACTED:provider-key] d [REDACTED:provider-key] e"
        );
    }

    #[test]
    fn req_0007_130_bearer_and_authorization_values_are_replaced() {
        let out = redact("Authorization: Bearer abcdefgh12345678").expect("redact");
        assert_eq!(out, "[REDACTED:bearer]");
        let bare = redact("send bearer abcdefgh12345678 now").expect("redact");
        assert_eq!(bare, "send [REDACTED:bearer] now");
    }

    #[test]
    fn req_0007_130_pem_private_key_block_is_replaced_whole() {
        let text = "a\n-----BEGIN PRIVATE KEY-----\nMIIBVQIBADANBgkq\n-----END PRIVATE KEY-----\nb";
        assert_eq!(
            redact(text).expect("redact"),
            "a\n[REDACTED:private-key]\nb"
        );
    }

    #[test]
    fn req_0007_130_truncated_pem_block_is_replaced_to_end_of_text() {
        let text = "-----BEGIN RSA PRIVATE KEY-----\nMIIBVQIB";
        assert_eq!(redact(text).expect("redact"), "[REDACTED:private-key]");
    }

    #[test]
    fn req_0007_130_jwt_shaped_string_is_replaced() {
        let out = redact("token eyJhbGciOi.eyJzdWIiOi.c2lnbmF0dXJl end").expect("redact");
        assert_eq!(out, "token [REDACTED:jwt] end");
    }

    #[test]
    fn req_0007_130_cloud_access_key_ids_are_replaced() {
        assert_eq!(
            redact("id AKIAABCDEFGHIJKLMNOP").expect("redact"),
            "id [REDACTED:cloud-key-id]"
        );
        assert_eq!(
            redact("id ASIAABCDEFGHIJKLMNOP").expect("redact"),
            "id [REDACTED:cloud-key-id]"
        );
        assert_eq!(redact("AKIAshort").expect("redact"), "AKIAshort");
    }

    #[test]
    fn req_0007_131_high_entropy_token_of_32_chars_is_replaced() {
        let token = "aZ9xQ2mW7pL4kR8sT1vN6bY3cH5dJ0fG";
        assert_eq!(token.len(), 32);
        assert_eq!(
            redact(&format!("x {token} y")).expect("redact"),
            "x [REDACTED:high-entropy] y"
        );
    }

    #[test]
    fn req_0007_130_low_entropy_and_short_tokens_are_kept() {
        let uniform = "a".repeat(40);
        assert_eq!(redact(&uniform).expect("redact"), uniform);
        let short = "aZ9xQ2mW7pL4kR8sT1vN6bY3cH5dJ0f";
        assert_eq!(short.len(), 31);
        assert_eq!(redact(short).expect("redact"), short);
        let prose = "the quick brown fox jumps over the lazy dog";
        assert_eq!(redact(prose).expect("redact"), prose);
    }

    #[test]
    fn req_0007_131_shannon_entropy_matches_definition() {
        assert!(shannon_bits_per_char("aaaa").abs() < 1e-12);
        assert!((shannon_bits_per_char("abcd") - 2.0).abs() < 1e-12);
        assert!((shannon_bits_per_char("abab") - 1.0).abs() < 1e-12);
    }

    #[test]
    fn req_0007_131_class_labels_are_the_rfc_set() {
        let labels: Vec<&str> = [
            RedactionClass::ProviderKey,
            RedactionClass::Bearer,
            RedactionClass::PrivateKey,
            RedactionClass::Jwt,
            RedactionClass::CloudKeyId,
            RedactionClass::HighEntropy,
        ]
        .iter()
        .map(|class| class.as_str())
        .collect();
        assert_eq!(
            labels,
            [
                "provider-key",
                "bearer",
                "private-key",
                "jwt",
                "cloud-key-id",
                "high-entropy"
            ]
        );
    }

    #[test]
    fn req_0007_133_redaction_is_idempotent_and_leaves_input_untouched() {
        let input = String::from("k sk-abcdefghijklmnop1234");
        let once = redact(&input).expect("redact");
        assert_eq!(input, "k sk-abcdefghijklmnop1234");
        assert_eq!(redact(&once).expect("redact"), once);
    }
}
