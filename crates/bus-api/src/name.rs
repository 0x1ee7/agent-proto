//! Validated names carried across the bus seam.
//!
//! Every constructor validates its input once; a value of these types is always
//! well formed. Grammars are the transport-neutral subset that RFC-0010 [0010-019,
//! 0010-020, 0010-028] requires of any binding.

use std::fmt;
use std::str::FromStr;

use a2a_types::ids::TenantId;
use thiserror::Error;

/// Upper bound on the byte length of subjects, patterns, keys and names.
pub const MAX_NAME_LEN: usize = 255;

/// A value rejected by one of the seam grammars.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid {kind}: {reason}")]
pub struct InvalidName {
    kind: &'static str,
    reason: &'static str,
}

impl InvalidName {
    pub(crate) const fn new(kind: &'static str, reason: &'static str) -> Self {
        Self { kind, reason }
    }

    /// The grammar that rejected the value, for example `subject`.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        self.kind
    }

    /// The rule that was violated.
    #[must_use]
    pub const fn reason(&self) -> &'static str {
        self.reason
    }
}

/// Printable ASCII without space.
const fn is_visible(byte: u8) -> bool {
    byte > b' ' && byte < 0x7f
}

/// Bytes allowed in a subject token: visible ASCII except `.`, `*` and `>`.
const fn is_token_byte(byte: u8) -> bool {
    is_visible(byte) && byte != b'.' && byte != b'*' && byte != b'>'
}

/// Bytes allowed in a KV key: visible ASCII except the wildcards `*` and `>`.
const fn is_key_byte(byte: u8) -> bool {
    is_visible(byte) && byte != b'*' && byte != b'>'
}

fn check_length(value: &str, kind: &'static str) -> Result<(), InvalidName> {
    if value.is_empty() {
        return Err(InvalidName::new(kind, "empty"));
    }
    if value.len() > MAX_NAME_LEN {
        return Err(InvalidName::new(kind, "longer than 255 bytes"));
    }
    Ok(())
}

fn check_word(value: &str, kind: &'static str) -> Result<(), InvalidName> {
    check_length(value, kind)?;
    if value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        Ok(())
    } else {
        Err(InvalidName::new(kind, "only [A-Za-z0-9_-] allowed"))
    }
}

fn check_key(value: &str, kind: &'static str) -> Result<(), InvalidName> {
    check_length(value, kind)?;
    if value.bytes().all(is_key_byte) {
        Ok(())
    } else {
        Err(InvalidName::new(
            kind,
            "must be visible ASCII without '*' or '>'",
        ))
    }
}

fn check_key_prefix(value: &str, kind: &'static str) -> Result<(), InvalidName> {
    if value.is_empty() {
        return Ok(());
    }
    check_key(value, kind)
}

fn check_printable(value: &str, kind: &'static str) -> Result<(), InvalidName> {
    check_length(value, kind)?;
    if value.bytes().all(is_visible) {
        Ok(())
    } else {
        Err(InvalidName::new(
            kind,
            "must be visible ASCII without space",
        ))
    }
}

macro_rules! text_newtype {
    ($(#[$meta:meta])* $name:ident, $kind:literal, $check:path) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(String);

        impl $name {
            /// Validates `value` against the grammar of this name.
            ///
            /// # Errors
            /// Returns [`InvalidName`] when `value` violates the grammar.
            pub fn parse(value: &str) -> Result<Self, InvalidName> {
                $check(value, $kind)?;
                Ok(Self(value.to_owned()))
            }

            /// The validated text.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = InvalidName;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::parse(s)
            }
        }
    };
}

text_newtype!(
    /// Name of a durable stream (`[A-Za-z0-9_-]+`, RFC-0010 [0010-028]).
    StreamName,
    "stream name",
    check_word
);

text_newtype!(
    /// Durable consumer name within a stream.
    ConsumerName,
    "consumer name",
    check_word
);

text_newtype!(
    /// Key of a KV entry. Dots are allowed, wildcards are not.
    Key,
    "key",
    check_key
);

text_newtype!(
    /// Prefix of KV keys for a watch. The empty prefix matches every key.
    KeyPrefix,
    "key prefix",
    check_key_prefix
);

text_newtype!(
    /// Message identifier used for append deduplication (RFC-0005 [0005-023]).
    MsgId,
    "message id",
    check_printable
);

/// A subject: dot-separated, non-empty tokens with no wildcards.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Subject(String);

impl Subject {
    /// Validates a concrete subject.
    ///
    /// # Errors
    /// Returns [`InvalidName`] for empty tokens, wildcards, whitespace,
    /// control characters or a total length above 255 bytes.
    pub fn parse(value: &str) -> Result<Self, InvalidName> {
        check_dotted(value, "subject", false)?;
        Ok(Self(value.to_owned()))
    }

    /// The validated text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Iterates the dot-separated tokens.
    pub fn tokens(&self) -> impl Iterator<Item = &str> {
        self.0.split('.')
    }
}

impl fmt::Display for Subject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for Subject {
    type Err = InvalidName;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// A subscription pattern: `*` matches one token, a final `>` matches one or
/// more trailing tokens.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SubjectPattern(String);

impl SubjectPattern {
    /// Validates a subscription pattern.
    ///
    /// # Errors
    /// Returns [`InvalidName`] for empty tokens, a `>` that is not the last
    /// token, or any other malformed token.
    pub fn parse(value: &str) -> Result<Self, InvalidName> {
        check_dotted(value, "subject pattern", true)?;
        Ok(Self(value.to_owned()))
    }

    /// The validated text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The pattern that matches exactly `subject`.
    #[must_use]
    pub fn exact(subject: &Subject) -> Self {
        Self(subject.0.clone())
    }

    /// Whether `subject` matches this pattern.
    #[must_use]
    pub fn matches(&self, subject: &Subject) -> bool {
        let mut pattern = self.0.split('.');
        let mut tokens = subject.0.split('.');
        loop {
            match (pattern.next(), tokens.next()) {
                (Some(">"), Some(_)) => return true,
                (Some(">"), None) | (None, Some(_)) | (Some(_), None) => return false,
                (None, None) => return true,
                (Some("*"), Some(_)) => {}
                (Some(p), Some(t)) if p == t => {}
                (Some(_), Some(_)) => return false,
            }
        }
    }
}

impl fmt::Display for SubjectPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for SubjectPattern {
    type Err = InvalidName;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

fn check_dotted(value: &str, kind: &'static str, allow_wildcards: bool) -> Result<(), InvalidName> {
    check_length(value, kind)?;
    let last = value.split('.').count() - 1;
    for (index, token) in value.split('.').enumerate() {
        if token.is_empty() {
            return Err(InvalidName::new(kind, "empty token"));
        }
        if allow_wildcards && (token == "*" || (token == ">" && index == last)) {
            continue;
        }
        if token == ">" || token == "*" {
            return Err(InvalidName::new(kind, "wildcard not allowed here"));
        }
        if !token.bytes().all(is_token_byte) {
            return Err(InvalidName::new(
                kind,
                "token contains a forbidden character",
            ));
        }
    }
    Ok(())
}

/// Name of a KV bucket. Buckets belong to exactly one tenant: a bucket is
/// addressed by `(tenant, name)` and tenant isolation is absolute (RFC-0011
/// [0011-042]).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Bucket {
    tenant: TenantId,
    name: String,
}

impl Bucket {
    /// Builds a bucket name inside `tenant`.
    ///
    /// # Errors
    /// Returns [`InvalidName`] when `name` violates the word grammar.
    pub fn new(tenant: &TenantId, name: &str) -> Result<Self, InvalidName> {
        check_word(name, "bucket name")?;
        Ok(Self {
            tenant: tenant.clone(),
            name: name.to_owned(),
        })
    }

    /// The owning tenant.
    #[must_use]
    pub const fn tenant(&self) -> &TenantId {
        &self.tenant
    }

    /// The bucket name within its tenant.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// Identity of the caller of a bus operation (RFC-0011 [0011-040]).
///
/// `Principal::anonymous()` is representable so that bindings can reject it;
/// every operation takes a principal and anonymous callers are denied.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Principal(Option<String>);

impl Principal {
    /// Validates a named principal.
    ///
    /// # Errors
    /// Returns [`InvalidName`] when the name is empty, too long or contains
    /// whitespace or control characters.
    pub fn named(name: &str) -> Result<Self, InvalidName> {
        check_printable(name, "principal")?;
        Ok(Self(Some(name.to_owned())))
    }

    /// The caller that carries no identity. Bindings must reject it.
    #[must_use]
    pub const fn anonymous() -> Self {
        Self(None)
    }

    /// Whether this value carries no identity.
    #[must_use]
    pub const fn is_anonymous(&self) -> bool {
        self.0.is_none()
    }

    /// The principal name, or `None` when anonymous.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.0.as_deref()
    }
}

#[cfg(test)]
#[allow(clippy::panic, reason = "test setup failures abort the test")]
mod tests {
    use super::*;

    fn subject(text: &str) -> Subject {
        match Subject::parse(text) {
            Ok(subject) => subject,
            Err(error) => panic!("test subject {text} rejected: {error}"),
        }
    }

    fn pattern(text: &str) -> SubjectPattern {
        match SubjectPattern::parse(text) {
            Ok(pattern) => pattern,
            Err(error) => panic!("test pattern {text} rejected: {error}"),
        }
    }

    // [0010-019] tokens reject '.', '*', '>', whitespace and control characters.
    #[test]
    fn req_0010_019_subject_token_rejects_forbidden_bytes() {
        for bad in ["a..b", "a.*", "a.>", "a b", "a\tb", "a\u{7f}", ""] {
            assert!(Subject::parse(bad).is_err(), "accepted {bad:?}");
        }
        assert!(Subject::parse("a.b-1.c").is_ok());
    }

    // [0010-020] subjects are at most 255 bytes.
    #[test]
    fn req_0010_020_subject_length_boundary() {
        let at_limit = "a".repeat(255);
        assert!(Subject::parse(&at_limit).is_ok());
        let over = "a".repeat(256);
        assert!(Subject::parse(&over).is_err());
    }

    // [0010-028] stream names are [A-Za-z0-9_-]+.
    #[test]
    fn req_0010_028_stream_name_grammar() {
        assert!(StreamName::parse("A2AB_EV_local").is_ok());
        assert!(StreamName::parse("").is_err());
        assert!(StreamName::parse("a.b").is_err());
        assert!(StreamName::parse("a b").is_err());
    }

    #[test]
    fn pattern_wildcards_match_tokens() {
        let s = subject("a2a.v1.local.task");
        assert!(pattern("a2a.v1.local.task").matches(&s));
        assert!(pattern("a2a.v1.*.task").matches(&s));
        assert!(pattern("a2a.v1.>").matches(&s));
        assert!(!pattern("a2a.v1").matches(&s));
        assert!(!pattern("a2a.v1.local.task.x").matches(&s));
        assert!(!pattern("a2a.v1.other.>").matches(&s));
    }

    #[test]
    fn pattern_greater_requires_at_least_one_token() {
        assert!(!pattern("a.>").matches(&subject("a")));
        assert!(pattern("a.>").matches(&subject("a.b")));
    }

    #[test]
    fn pattern_rejects_misplaced_greater() {
        assert!(SubjectPattern::parse("a.>.b").is_err());
        assert!(SubjectPattern::parse("a.b>").is_err());
        assert!(Subject::parse("a.*").is_err());
    }

    #[test]
    fn key_rejects_wildcards_and_accepts_dots() {
        assert!(Key::parse("agent.msg-1").is_ok());
        assert!(Key::parse("agent.*").is_err());
        assert!(KeyPrefix::parse("").is_ok());
        assert!(Key::parse("").is_err());
    }

    #[test]
    fn anonymous_principal_is_representable_but_not_named() {
        assert!(Principal::anonymous().is_anonymous());
        assert!(Principal::anonymous().name().is_none());
        assert!(Principal::named("").is_err());
        assert!(Principal::named("supervisor-1").is_ok());
    }
}
