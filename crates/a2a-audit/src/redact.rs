//! Redaction before hashing, spooling, publishing and writing (RFC-0013 0013-022, 0013-023,
//! 0013-025). Rules record only their identifier; matched content is never returned or kept.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::LazyLock;

use std::net::Ipv6Addr;

use regex::{Captures, Regex};
use serde_json::{Map, Value};

/// Token written in place of a credential-shaped value.
pub const REDACTED: &str = "[redacted]";
/// Token written in place of a long high-entropy string.
pub const REDACTED_ENTROPY: &str = "[redacted:entropy]";
/// Maximum characters kept in `args_summary` (0013-023 R-A2).
pub const ARGS_SUMMARY_MAX_CHARS: usize = 512;

const ENTROPY_MIN_LEN: usize = 24;
const ENTROPY_MIN_BITS: f64 = 4.0;

/// Redaction rule identifiers recorded in `redactions`. R-A1 is structural: the schema admits no
/// message bodies, so it has no runtime rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RuleId {
    /// Credentials, keys, long high-entropy strings.
    A2,
    /// Absolute filesystem paths.
    A3,
    /// Environment variable values.
    A4,
    /// Known secret material, removed entirely.
    A5,
    /// URLs, IP addresses, host:port.
    A6,
}

impl RuleId {
    /// The identifier written to `redactions` and to the `rules` detail.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::A2 => "R-A2",
            Self::A3 => "R-A3",
            Self::A4 => "R-A4",
            Self::A5 => "R-A5",
            Self::A6 => "R-A6",
        }
    }
}

/// Rule firing counts. Holds identifiers and counts only, never content.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuleTally {
    counts: BTreeMap<RuleId, u64>,
}

impl RuleTally {
    /// Records one firing of `rule`.
    pub fn hit(&mut self, rule: RuleId) {
        let slot = self.counts.entry(rule).or_insert(0);
        *slot = slot.saturating_add(1);
    }

    /// Adds the counts of `other`.
    pub fn merge(&mut self, other: &RuleTally) {
        for (rule, count) in &other.counts {
            let slot = self.counts.entry(*rule).or_insert(0);
            *slot = slot.saturating_add(*count);
        }
    }

    /// True when no rule fired.
    pub fn is_empty(&self) -> bool {
        self.counts.is_empty()
    }

    /// Firings recorded for `rule`.
    pub fn count(&self, rule: RuleId) -> u64 {
        self.counts.get(&rule).copied().unwrap_or(0)
    }

    /// Identifiers of the rules that fired, sorted.
    pub fn rule_ids(&self) -> Vec<String> {
        self.counts
            .keys()
            .map(|rule| rule.as_str().to_owned())
            .collect()
    }

    /// `{"R-A2": "3", ...}`: counts travel as strings (0013-008).
    pub fn to_value(&self) -> Value {
        Value::Object(
            self.counts
                .iter()
                .map(|(rule, count)| (rule.as_str().to_owned(), Value::String(count.to_string())))
                .collect::<Map<String, Value>>(),
        )
    }
}

struct Rules {
    nkey_seed: Regex,
    private_key: Regex,
    bearer: Regex,
    assignment: Regex,
    userinfo: Regex,
    url: Regex,
    host_port: Regex,
    ipv4: Regex,
    ipv6: Regex,
    path: Regex,
    token: Regex,
}

impl Rules {
    fn compile() -> Option<Self> {
        Some(Self {
            nkey_seed: Regex::new(r"\bS[UAONCX][A-Z2-7]{56}\b").ok()?,
            private_key: Regex::new(
                r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----[\s\S]*?(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|$)",
            )
            .ok()?,
            bearer: Regex::new(r"(?i)bearer\s+[A-Za-z0-9._~+/=-]+").ok()?,
            assignment: Regex::new(
                r#"(?i)((?:[A-Za-z0-9_.-]*(?:password|passwd|secret|token|api[_-]?key|authorization|credential|private[_-]?key)[A-Za-z0-9_.-]*)\s*[=:]\s*)("[^"]*"|'[^']*'|[^\s,;&"'}\]]*)"#,
            )
            .ok()?,
            userinfo: Regex::new(r"([A-Za-z][A-Za-z0-9+.-]*://)[^\s/@]+@").ok()?,
            url: Regex::new(r#"\b[A-Za-z][A-Za-z0-9+.-]*://[^\s"'<>]+"#).ok()?,
            host_port: Regex::new(
                r"\b(?:localhost|(?:[A-Za-z0-9-]+\.)+[A-Za-z]{2,}|\d{1,3}(?:\.\d{1,3}){3}):\d{1,5}\b",
            )
            .ok()?,
            ipv4: Regex::new(r"\b\d{1,3}(?:\.\d{1,3}){3}\b").ok()?,
            ipv6: Regex::new(r"\[?[0-9A-Fa-f]{0,4}(?::[0-9A-Fa-f]{0,4}){2,7}\]?").ok()?,
            path: Regex::new(r#"(^|[\s"'=(:,])((?:[A-Za-z]:[\\/]|/)[^\s"'<>|,;]*)"#).ok()?,
            token: Regex::new(r"[A-Za-z0-9+/_=]{24,}").ok()?,
        })
    }
}

static RULES: LazyLock<Option<Rules>> = LazyLock::new(Rules::compile);

/// Applies R-A2..R-A6 to text and to the details map. Holds the workspace root and the known
/// secrets, which are configuration, not event content.
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    workspace: Option<Workspace>,
    secrets: Vec<String>,
}

#[derive(Debug, Clone)]
struct Workspace {
    root: String,
    id: String,
}

impl Redactor {
    /// A redactor with no workspace and no known secrets.
    pub fn new() -> Self {
        Self::default()
    }

    /// Paths under `root` become `<id>/<relative>` (R-A3). The root itself becomes `<workspace>`.
    pub fn with_workspace(mut self, root: &Path, id: &str) -> Self {
        let root = normalize_slashes(&root.to_string_lossy())
            .trim_end_matches('/')
            .to_owned();
        self.workspace = Some(Workspace {
            root,
            id: id.to_owned(),
        });
        self
    }

    /// Registers secret material that must never reach any stored record (R-A5). Empty values
    /// are ignored.
    pub fn with_secret(mut self, secret: &str) -> Self {
        if !secret.is_empty() {
            self.secrets.push(secret.to_owned());
        }
        self
    }

    /// Redacts every string in `details`. `env`/`environment` objects keep only their names
    /// (R-A4). `args_summary` is normalized and capped at 512 characters.
    pub fn redact_details(&self, details: &mut Map<String, Value>) -> RuleTally {
        let mut tally = RuleTally::default();
        for (key, value) in details.iter_mut() {
            match (key.as_str(), value) {
                ("args_summary", Value::String(text)) => {
                    *text = self.summary(text, &mut tally);
                }
                (_, value) => self.redact_value(key, value, &mut tally),
            }
        }
        tally
    }

    /// Redacts one text. Each rule that fires is recorded in `tally` by identifier only.
    pub fn redact_text(&self, text: &str, tally: &mut RuleTally) -> String {
        let Some(rules) = RULES.as_ref() else {
            // Fail closed: a broken rule set must not let the original text through.
            tally.hit(RuleId::A2);
            return REDACTED.to_owned();
        };
        for secret in &self.secrets {
            if text.contains(secret.as_str()) {
                tally.hit(RuleId::A5);
                return REDACTED.to_owned();
            }
        }
        let out = replace(&rules.nkey_seed, text.to_owned(), |_| {
            tally.hit(RuleId::A5);
            REDACTED.to_owned()
        });
        let out = replace(&rules.private_key, out, |_| {
            tally.hit(RuleId::A2);
            REDACTED.to_owned()
        });
        let out = replace(&rules.bearer, out, |_| {
            tally.hit(RuleId::A2);
            REDACTED.to_owned()
        });
        let out = replace(&rules.assignment, out, |c| {
            tally.hit(RuleId::A2);
            format!("{}{}", &c[1], REDACTED)
        });
        let out = replace(&rules.userinfo, out, |c| {
            tally.hit(RuleId::A2);
            format!("{}{}@", &c[1], REDACTED)
        });
        let out = replace(&rules.url, out, |_| {
            tally.hit(RuleId::A6);
            "<address>".to_owned()
        });
        let out = replace(&rules.host_port, out, |_| {
            tally.hit(RuleId::A6);
            "<address>".to_owned()
        });
        let out = replace(&rules.ipv4, out, |_| {
            tally.hit(RuleId::A6);
            "<address>".to_owned()
        });
        let out = replace_ipv6(&rules.ipv6, &out, tally);
        let out = replace(&rules.path, out, |c| {
            tally.hit(RuleId::A3);
            format!("{}{}", &c[1], self.path_label(&c[2]))
        });
        replace(&rules.token, out, |c| {
            if is_high_entropy(&c[0]) {
                tally.hit(RuleId::A2);
                REDACTED_ENTROPY.to_owned()
            } else {
                c[0].to_owned()
            }
        })
    }

    fn summary(&self, text: &str, tally: &mut RuleTally) -> String {
        let normalized = normalize_whitespace(text);
        let redacted = self.redact_text(&normalized, tally);
        redacted.chars().take(ARGS_SUMMARY_MAX_CHARS).collect()
    }

    fn redact_value(&self, key: &str, value: &mut Value, tally: &mut RuleTally) {
        if is_environment_key(key) {
            let names: Option<Vec<Value>> = match value {
                Value::Object(map) => {
                    let mut keys: Vec<String> = map.keys().cloned().collect();
                    keys.sort();
                    Some(keys.into_iter().map(Value::String).collect())
                }
                _ => None,
            };
            if let Some(names) = names {
                tally.hit(RuleId::A4);
                *value = Value::Array(names);
                return;
            }
        }
        match value {
            Value::String(text) => *text = self.redact_text(text, tally),
            Value::Array(items) => {
                for item in items.iter_mut() {
                    self.redact_value(key, item, tally);
                }
            }
            Value::Object(map) => {
                for (child_key, child) in map.iter_mut() {
                    self.redact_value(child_key, child, tally);
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }

    fn path_label(&self, path: &str) -> String {
        let normalized = normalize_slashes(path);
        if let Some(ws) = &self.workspace {
            if let Some(rest) = strip_prefix_ci(&normalized, &ws.root) {
                if rest.is_empty() {
                    return "<workspace>".to_owned();
                }
                if let Some(relative) = rest.strip_prefix('/') {
                    return format!("{}/{}", ws.id, relative);
                }
            }
        }
        "<path>".to_owned()
    }
}

fn replace(re: &Regex, text: String, f: impl FnMut(&Captures) -> String) -> String {
    re.replace_all(&text, f).into_owned()
}

/// Replaces each IPv6 address-shaped run that is delimited by non-word characters and parses as
/// an IPv6 address. Plain identifiers such as `vec::Vec` are left alone.
fn replace_ipv6(re: &Regex, text: &str, tally: &mut RuleTally) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for found in re.find_iter(text) {
        out.push_str(&text[last..found.start()]);
        last = found.end();
        let bounded = text[..found.start()]
            .chars()
            .next_back()
            .is_none_or(|c| !is_word_char(c))
            && text[found.end()..]
                .chars()
                .next()
                .is_none_or(|c| !is_word_char(c));
        let candidate = found.as_str().trim_matches(['[', ']']);
        if bounded && candidate.parse::<Ipv6Addr>().is_ok() {
            tally.hit(RuleId::A6);
            out.push_str("<address>");
        } else {
            out.push_str(found.as_str());
        }
    }
    out.push_str(&text[last..]);
    out
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn is_environment_key(key: &str) -> bool {
    key.eq_ignore_ascii_case("env") || key.eq_ignore_ascii_case("environment")
}

fn normalize_slashes(path: &str) -> String {
    path.replace('\\', "/")
}

fn strip_prefix_ci<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    if head.eq_ignore_ascii_case(prefix) {
        text.get(prefix.len()..)
    } else {
        None
    }
}

fn normalize_whitespace(text: &str) -> String {
    text.split(|c: char| c.is_control() || c.is_whitespace())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// A long contiguous token that looks random. Hex-only tokens (digests, trace ids) and tokens
/// without both letters and digits are not secrets by this rule.
fn is_high_entropy(token: &str) -> bool {
    if token.len() < ENTROPY_MIN_LEN {
        return false;
    }
    if token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return false;
    }
    let has_letter = token.bytes().any(|b| b.is_ascii_alphabetic());
    let has_digit = token.bytes().any(|b| b.is_ascii_digit());
    has_letter && has_digit && shannon_bits(token) >= ENTROPY_MIN_BITS
}

fn shannon_bits(text: &str) -> f64 {
    let mut counts = [0usize; 256];
    for byte in text.bytes() {
        if let Some(slot) = counts.get_mut(usize::from(byte)) {
            *slot = slot.saturating_add(1);
        }
    }
    let total = text.len() as f64;
    counts
        .iter()
        .filter(|count| **count > 0)
        .map(|count| {
            let p = *count as f64 / total;
            -p * p.log2()
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::json;

    use super::*;

    const SECRET_TOKEN: &str = "Zx9qLm4pRt7WvY2nBc8dFg3H";

    fn run(text: &str) -> (String, RuleTally) {
        let mut tally = RuleTally::default();
        let out = Redactor::new().redact_text(text, &mut tally);
        (out, tally)
    }

    #[test]
    fn req_0013_023_rule_set_compiles() {
        assert!(
            RULES.is_some(),
            "every static redaction pattern must compile"
        );
    }

    #[test]
    fn req_0013_023_bearer_and_assignments_are_redacted_and_recorded_by_id_only() {
        let (out, tally) = run(&format!(
            "auth Bearer {SECRET_TOKEN} and password={SECRET_TOKEN}"
        ));
        assert!(!out.contains(SECRET_TOKEN));
        assert_eq!(out, "auth [redacted] and password=[redacted]");
        assert_eq!(tally.count(RuleId::A2), 2);
        assert_eq!(tally.rule_ids(), vec!["R-A2".to_owned()]);
    }

    #[test]
    fn req_0013_023_private_key_block_is_removed() {
        let text = "x -----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBgkq\n-----END PRIVATE KEY----- y";
        let (out, tally) = run(text);
        assert!(!out.contains("MIIEvQ"));
        assert!(out.contains("[redacted]"));
        assert_eq!(tally.count(RuleId::A2), 1);
    }

    #[test]
    fn req_0013_023_url_userinfo_and_addresses_are_redacted() {
        let (out, tally) = run("nats://user:pw@10.0.0.5:4222/x");
        assert!(!out.contains("pw@"));
        assert!(!out.contains("10.0.0.5"));
        assert_eq!(out, "<address>");
        assert!(tally.count(RuleId::A6) >= 1);
        assert!(tally.count(RuleId::A2) >= 1);
    }

    #[test]
    fn req_0013_023_host_port_and_ipv6_addresses_are_redacted() {
        let (out, _) = run("connect api.example.com:443 or fe80::1ff:fe23");
        assert_eq!(out, "connect <address> or <address>");
    }

    #[test]
    fn req_0013_023_times_and_plain_words_are_not_addresses() {
        let (out, tally) = run("09:14:03.218Z std::vec::Vec file.rs");
        assert_eq!(out, "09:14:03.218Z std::vec::Vec file.rs");
        assert!(tally.is_empty());
    }

    #[test]
    fn req_0013_023_absolute_paths_are_relative_to_the_workspace() {
        let mut tally = RuleTally::default();
        let redactor = Redactor::new().with_workspace(Path::new("/home/u/ws"), "ws1");
        let out = redactor.redact_text(
            "open /home/u/ws/src/a.rs and /home/u/ws then /etc/passwd",
            &mut tally,
        );
        assert_eq!(out, "open ws1/src/a.rs and <workspace> then <path>");
        assert_eq!(tally.count(RuleId::A3), 3);
    }

    #[test]
    fn req_0013_023_workspace_prefix_must_match_a_whole_component() {
        let mut tally = RuleTally::default();
        let redactor = Redactor::new().with_workspace(Path::new("/home/u/ws"), "ws1");
        let out = redactor.redact_text("/home/u/wsx/a", &mut tally);
        assert_eq!(out, "<path>");
    }

    #[test]
    fn req_0013_023_high_entropy_strings_are_replaced() {
        let (out, tally) = run("key ABCdef123ghiJKL456mnoPQR789stu end");
        assert!(out.contains(REDACTED_ENTROPY), "got {out}");
        assert_eq!(tally.count(RuleId::A2), 1);
    }

    #[test]
    fn req_0013_023_digests_trace_ids_and_uuids_are_not_redacted() {
        let digest = "a".repeat(40);
        let sha = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
        let uuid = "0195f3a2-6c20-7e21-9b4d-2a7c8e9f0a12";
        let trace = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        for text in [digest.as_str(), sha, uuid, trace] {
            let (out, tally) = run(text);
            assert_eq!(out, text, "must not redact {text}");
            assert!(tally.is_empty());
        }
    }

    #[test]
    fn req_0013_023_nkey_seed_is_removed_entirely() {
        let seed = format!("SUA{}", "A".repeat(55));
        let mut tally = RuleTally::default();
        let out = Redactor::new().redact_text(&format!("seed {seed}"), &mut tally);
        assert!(!out.contains(&seed));
        assert_eq!(tally.count(RuleId::A5), 1);
    }

    #[test]
    fn req_0013_023_known_secret_removes_the_whole_value() {
        let mut tally = RuleTally::default();
        let redactor = Redactor::new().with_secret("hunter2-zz");
        let out = redactor.redact_text("login hunter2-zz ok", &mut tally);
        assert_eq!(out, REDACTED);
        assert_eq!(tally.count(RuleId::A5), 1);
    }

    #[test]
    fn req_0013_023_env_values_become_sorted_names() {
        let mut details = json!({"env": {"TOKEN": "abc", "PATH": "/usr/bin"}})
            .as_object()
            .cloned()
            .unwrap_or_default();
        let tally = Redactor::new().redact_details(&mut details);
        assert_eq!(details["env"], json!(["PATH", "TOKEN"]));
        assert_eq!(tally.count(RuleId::A4), 1);
    }

    #[test]
    fn req_0013_023_args_summary_is_normalized_and_capped() {
        let raw = format!("run\n\tcmd\u{7}  {}", "x ".repeat(400));
        let mut details = json!({"args_summary": raw})
            .as_object()
            .cloned()
            .unwrap_or_default();
        Redactor::new().redact_details(&mut details);
        let summary = details["args_summary"].as_str().unwrap_or_default();
        assert!(summary.chars().count() <= ARGS_SUMMARY_MAX_CHARS);
        assert!(summary.starts_with("run cmd x x"));
        assert!(!summary.chars().any(char::is_control));
    }

    #[test]
    fn req_0013_022_details_strings_are_redacted_before_use() {
        let mut details = json!({"detail": format!("token={SECRET_TOKEN}")})
            .as_object()
            .cloned()
            .unwrap_or_default();
        let tally = Redactor::new().redact_details(&mut details);
        assert_eq!(details["detail"], json!("token=[redacted]"));
        assert_eq!(tally.count(RuleId::A2), 1);
    }

    #[test]
    fn req_0013_023_tally_counts_serialize_as_strings() {
        let mut tally = RuleTally::default();
        tally.hit(RuleId::A2);
        tally.hit(RuleId::A2);
        tally.hit(RuleId::A6);
        assert_eq!(tally.to_value(), json!({"R-A2": "2", "R-A6": "1"}));
    }

    #[test]
    fn req_0013_023_shannon_bits_of_uniform_text() {
        assert!((shannon_bits("abcd") - 2.0).abs() < 1e-9);
    }
}
