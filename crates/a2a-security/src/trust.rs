//! Trust allowlist `trust.toml` ([0007-020..026]): load-time validation and hot reload.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};

use a2a_types::ids::{AgentId, TenantId};
use ed25519_dalek::VerifyingKey;
use serde::Deserialize;

use crate::clock::parse_rfc3339_utc_ms;
use crate::error::{BusError, Reason, SecurityError};
use crate::identity::{Kid, Verifier, decode_public_key, verify_ed25519};
use crate::storage;

/// Required first line value of `trust.toml` ([0007-020]).
pub const SCHEMA: &str = "a2abus.trust/1";
/// Default `max_skew_ms` ([0007-021]).
pub const DEFAULT_MAX_SKEW_MS: i64 = 300_000;
/// Largest permitted `max_skew_ms` ([0007-021]).
pub const MAX_SKEW_LIMIT_MS: i64 = 3_600_000;
const DEFAULT_TENANT: &str = "local";
pub(crate) const MAX_TRUST_BYTES: u64 = 1024 * 1024;
pub const OPERATOR_AGENT: &str = "operator";
/// Bus identities that relay operator decisions ([0010-063]). An envelope from one of them is
/// accepted only under an operator-role key, and the broker must publish it under the same name
/// ([0007-038]).
pub const OPERATOR_RELAY_IDS: [&str; 2] = ["tui", "cli"];

/// True when `sender` may carry a message signed by the key of agent `key_agent` ([0007-038]).
/// The operator key is carried by a relay identity; every other key by its own agent id.
#[must_use]
pub fn sender_matches_key(sender: &str, key_agent: &AgentId) -> bool {
    if key_agent.as_str() == OPERATOR_AGENT {
        OPERATOR_RELAY_IDS.contains(&sender)
    } else {
        sender == key_agent.as_str()
    }
}

/// Role of a trust entry ([0007-021]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Agent,
    Gateway,
    Operator,
}

impl Role {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "agent" => Some(Self::Agent),
            "gateway" => Some(Self::Gateway),
            "operator" => Some(Self::Operator),
            _ => None,
        }
    }
}

/// Lifecycle status of a key ([0007-021]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KeyStatus {
    #[default]
    Active,
    Retired,
    Revoked,
}

impl KeyStatus {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "active" => Some(Self::Active),
            "retired" => Some(Self::Retired),
            "revoked" => Some(Self::Revoked),
            _ => None,
        }
    }
}

/// One validated `[[key]]` entry.
#[derive(Clone, Debug)]
pub struct TrustEntry {
    kid: Kid,
    agent: AgentId,
    role: Role,
    public_key: VerifyingKey,
    status: KeyStatus,
    not_before_ms: Option<i64>,
    not_after_ms: Option<i64>,
    tenants: Vec<TenantId>,
    max_skew_ms: i64,
    require_signature: bool,
    mcp_direct: bool,
}

impl TrustEntry {
    #[must_use]
    pub const fn kid(&self) -> &Kid {
        &self.kid
    }

    #[must_use]
    pub const fn agent(&self) -> &AgentId {
        &self.agent
    }

    #[must_use]
    pub const fn role(&self) -> Role {
        self.role
    }

    #[must_use]
    pub const fn status(&self) -> KeyStatus {
        self.status
    }

    #[must_use]
    pub const fn max_skew_ms(&self) -> i64 {
        self.max_skew_ms
    }

    #[must_use]
    pub const fn require_signature(&self) -> bool {
        self.require_signature
    }

    #[must_use]
    pub const fn mcp_direct(&self) -> bool {
        self.mcp_direct
    }

    /// True if the entry's `tenants` include `tenant` ([0007-026]).
    #[must_use]
    pub fn serves(&self, tenant: &TenantId) -> bool {
        self.tenants.iter().any(|t| t == tenant)
    }

    /// The raw Ed25519 public key bytes of this entry ([0007-001]).
    #[must_use]
    pub(crate) fn public_key_bytes(&self) -> [u8; 32] {
        self.public_key.to_bytes()
    }

    /// Status and validity-window check at `now_ms` ([0007-039]).
    pub fn check_validity(&self, now_ms: i64) -> Result<(), BusError> {
        let kid = self.kid.to_string();
        if self.status == KeyStatus::Revoked {
            return Err(BusError::new(Reason::SignatureKeyRevoked).with_kid(kid));
        }
        if self.not_before_ms.is_some_and(|nb| now_ms < nb) {
            return Err(BusError::new(Reason::SignatureKeyInactive).with_kid(kid));
        }
        if self.not_after_ms.is_some_and(|na| now_ms >= na) {
            return Err(BusError::new(Reason::SignatureKeyInactive).with_kid(kid));
        }
        Ok(())
    }
}

impl Verifier for TrustEntry {
    fn verify_bytes(&self, message: &[u8], signature: &[u8]) -> bool {
        verify_ed25519(&self.public_key, message, signature)
    }
}

/// Validated allowlist, keyed by kid text.
#[derive(Debug, Default)]
pub struct TrustStore {
    entries: HashMap<String, TrustEntry>,
}

impl TrustStore {
    /// Loads and validates `path`. Refuses files that fail the permission check ([0007-012]).
    pub fn load(path: &Path) -> Result<Self, SecurityError> {
        storage::check_trust_file(path)?;
        let text = storage::read_limited(path, MAX_TRUST_BYTES)?;
        Self::parse(&text)
    }

    /// Parses and validates trust file text ([0007-022]).
    pub fn parse(text: &str) -> Result<Self, SecurityError> {
        let raw: RawFile =
            toml::from_str(text).map_err(|_| SecurityError::Trust("malformed TOML".to_owned()))?;
        if raw.schema != SCHEMA {
            return Err(SecurityError::Trust("unsupported schema".to_owned()));
        }
        let mut entries: HashMap<String, TrustEntry> = HashMap::new();
        for (index, raw_key) in raw.key.into_iter().enumerate() {
            let entry = raw_key
                .validate()
                .map_err(|reason| SecurityError::Trust(format!("key[{index}]: {reason}")))?;
            let kid = entry.kid.to_string();
            if entries.contains_key(&kid) {
                return Err(SecurityError::Trust(format!("key[{index}]: duplicate kid")));
            }
            entries.insert(kid, entry);
        }
        for (index, entry) in entries.values().enumerate() {
            let active = entries
                .values()
                .filter(|other| other.agent == entry.agent && other.status == KeyStatus::Active)
                .count();
            if active > 1 {
                return Err(SecurityError::Trust(format!(
                    "agent has more than one active key (entry {index})"
                )));
            }
        }
        Ok(Self { entries })
    }

    /// Looks up an entry by the wire text of its kid.
    #[must_use]
    pub fn entry(&self, kid: &str) -> Option<&TrustEntry> {
        self.entries.get(kid)
    }

    /// The single active key of `agent`, used for signing.
    #[must_use]
    pub fn active_for_agent(&self, agent: &AgentId) -> Option<&TrustEntry> {
        self.entries
            .values()
            .find(|e| &e.agent == agent && e.status == KeyStatus::Active)
    }

    /// True if any key of `agent` sets `require_signature` ([0007-021]).
    #[must_use]
    pub fn agent_requires_signature(&self, agent: &AgentId) -> bool {
        self.entries
            .values()
            .any(|e| &e.agent == agent && e.require_signature)
    }

    pub fn entries(&self) -> impl Iterator<Item = &TrustEntry> {
        self.entries.values()
    }
}

/// Hot-reload failure: the previous state stays in force ([0007-023]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustReloadRejected {
    pub file: PathBuf,
    pub reason: String,
}

impl fmt::Display for TrustReloadRejected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "trust reload rejected: {}", self.reason)
    }
}

impl std::error::Error for TrustReloadRejected {}

/// Holds the current [`TrustStore`] and swaps it atomically on reload.
#[derive(Debug)]
pub struct TrustHolder {
    current: RwLock<Arc<TrustStore>>,
}

impl TrustHolder {
    #[must_use]
    pub fn new(store: TrustStore) -> Self {
        Self {
            current: RwLock::new(Arc::new(store)),
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> Arc<TrustStore> {
        let guard = self.current.read().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(&guard)
    }

    /// Replaces the state with `path` if it validates; otherwise keeps the last good state.
    pub fn reload(&self, path: &Path) -> Result<(), TrustReloadRejected> {
        match TrustStore::load(path) {
            Ok(store) => {
                let mut guard = self.current.write().unwrap_or_else(PoisonError::into_inner);
                *guard = Arc::new(store);
                Ok(())
            }
            Err(error) => Err(TrustReloadRejected {
                file: path.to_path_buf(),
                reason: error.to_string(),
            }),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    schema: String,
    #[serde(default)]
    key: Vec<RawKey>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawKey {
    kid: String,
    agent: String,
    role: String,
    #[serde(default = "default_tenants")]
    tenants: Vec<String>,
    public_key: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    not_before: Option<String>,
    #[serde(default)]
    not_after: Option<String>,
    #[serde(default = "default_skew")]
    max_skew_ms: i64,
    #[serde(default)]
    require_signature: bool,
    #[serde(default)]
    mcp_direct: bool,
}

fn default_tenants() -> Vec<String> {
    vec![DEFAULT_TENANT.to_owned()]
}

const fn default_skew() -> i64 {
    DEFAULT_MAX_SKEW_MS
}

impl RawKey {
    fn validate(self) -> Result<TrustEntry, &'static str> {
        let agent = AgentId::parse(&self.agent).map_err(|_| "invalid agent id")?;
        let role = Role::parse(&self.role).ok_or("invalid role")?;
        let status = match self.status.as_deref() {
            None => KeyStatus::default(),
            Some(text) => KeyStatus::parse(text).ok_or("invalid status")?,
        };
        let public_key_bytes = decode_public_key(&self.public_key).ok_or("invalid public key")?;
        let public_key =
            VerifyingKey::from_bytes(&public_key_bytes).map_err(|_| "invalid public key")?;
        let kid = Kid::parse(&self.kid).ok_or("invalid kid")?;
        if kid != Kid::derive(&agent, &public_key_bytes) {
            return Err("kid does not match agent and public key");
        }
        let not_before_ms = parse_optional_instant(self.not_before.as_deref())?;
        let not_after_ms = parse_optional_instant(self.not_after.as_deref())?;
        if status == KeyStatus::Retired && not_after_ms.is_none() {
            return Err("retired key requires not_after");
        }
        if let (Some(before), Some(after)) = (not_before_ms, not_after_ms) {
            if before >= after {
                return Err("not_before must precede not_after");
            }
        }
        if !(1..=MAX_SKEW_LIMIT_MS).contains(&self.max_skew_ms) {
            return Err("max_skew_ms out of range");
        }
        if self.tenants.is_empty() {
            return Err("tenants must not be empty");
        }
        let tenants = self
            .tenants
            .iter()
            .map(|t| TenantId::parse(t).map_err(|_| "invalid tenant"))
            .collect::<Result<Vec<_>, _>>()?;
        match role {
            Role::Operator if agent.as_str() != OPERATOR_AGENT => {
                return Err("operator role requires agent operator");
            }
            Role::Gateway if agent != AgentId::gateway() => {
                return Err("gateway role requires agent gateway");
            }
            Role::Agent if agent.is_reserved() || agent.as_str() == OPERATOR_AGENT => {
                return Err("agent role may not use a reserved name");
            }
            _ => {}
        }
        if self.mcp_direct && role != Role::Agent {
            return Err("mcp_direct requires role agent");
        }
        Ok(TrustEntry {
            kid,
            agent,
            role,
            public_key,
            status,
            not_before_ms,
            not_after_ms,
            tenants,
            max_skew_ms: self.max_skew_ms,
            require_signature: self.require_signature,
            mcp_direct: self.mcp_direct,
        })
    }
}

fn parse_optional_instant(text: Option<&str>) -> Result<Option<i64>, &'static str> {
    text.map(|t| parse_rfc3339_utc_ms(t).ok_or("timestamp must be RFC 3339 UTC"))
        .transpose()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::identity::{Ed25519Signer, Signer, encode_public_key};
    use crate::testkit::{agent, file, key_block, key_block_with_role};

    #[test]
    fn req_0007_020_schema_line_is_required() {
        let body = key_block("alpha", 1, "");
        assert!(TrustStore::parse(&body).is_err(), "missing schema");
        let wrong = format!("schema = \"a2abus.trust/2\"\n{body}");
        assert!(TrustStore::parse(&wrong).is_err(), "wrong schema");
        assert!(TrustStore::parse(&file(&body)).is_ok());
    }

    #[test]
    fn req_0007_021_defaults_apply_when_fields_are_omitted() {
        let store = TrustStore::parse(&file(&key_block("alpha", 1, ""))).expect("loads");
        let kid = Ed25519Signer::from_seed(&agent("alpha"), &[1; 32])
            .kid()
            .to_string();
        let entry = store.entry(&kid).expect("present");
        assert_eq!(entry.status(), KeyStatus::Active);
        assert_eq!(entry.max_skew_ms(), DEFAULT_MAX_SKEW_MS);
        assert!(!entry.require_signature());
        assert!(!entry.mcp_direct());
        assert!(entry.serves(&TenantId::parse("local").expect("tenant")));
        assert!(!entry.serves(&TenantId::parse("other").expect("tenant")));
    }

    #[test]
    fn req_0007_021_unknown_fields_are_a_load_error() {
        let body = key_block("alpha", 1, "surprise = true");
        assert!(TrustStore::parse(&file(&body)).is_err());
        let top = format!("{}\nextra = 1\n", file(&key_block("alpha", 1, "")));
        assert!(TrustStore::parse(&top).is_err());
    }

    #[test]
    fn req_0007_021_max_skew_bounds_are_enforced() {
        for ok in ["1", "3600000"] {
            let body = key_block("alpha", 1, &format!("max_skew_ms = {ok}"));
            assert!(TrustStore::parse(&file(&body)).is_ok(), "rejected {ok}");
        }
        for bad in ["0", "-5", "3600001"] {
            let body = key_block("alpha", 1, &format!("max_skew_ms = {bad}"));
            assert!(TrustStore::parse(&file(&body)).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn req_0007_022_kid_must_derive_from_agent_and_key() {
        let signer = Ed25519Signer::from_seed(&agent("alpha"), &[1; 32]);
        let other_kid = Ed25519Signer::from_seed(&agent("alpha"), &[2; 32])
            .kid()
            .to_string();
        let pk = encode_public_key(&signer.public_key());
        let body = format!(
            "[[key]]\nkid = \"{other_kid}\"\nagent = \"alpha\"\nrole = \"agent\"\npublic_key = \"{pk}\"\n"
        );
        assert!(TrustStore::parse(&file(&body)).is_err());
    }

    #[test]
    fn req_0007_022_kids_must_be_unique() {
        let body = format!("{}{}", key_block("alpha", 1, ""), key_block("alpha", 1, ""));
        assert!(TrustStore::parse(&file(&body)).is_err());
    }

    #[test]
    fn req_0007_022_at_most_one_active_key_per_agent() {
        let body = format!("{}{}", key_block("alpha", 1, ""), key_block("alpha", 2, ""));
        assert!(TrustStore::parse(&file(&body)).is_err());
        let retired = key_block(
            "alpha",
            2,
            "status = \"retired\"\nnot_after = \"2030-01-01T00:00:00Z\"",
        );
        let two = format!("{}{retired}", key_block("alpha", 1, ""));
        assert!(TrustStore::parse(&file(&two)).is_ok());
    }

    #[test]
    fn req_0007_022_role_and_reserved_names_must_agree() {
        let op_as_agent = key_block("operator", 1, "");
        assert!(
            TrustStore::parse(&file(&op_as_agent)).is_err(),
            "agent role uses reserved name"
        );
        let gateway_role = format!(
            "[[key]]\nkid = \"{}\"\nagent = \"alpha\"\nrole = \"gateway\"\npublic_key = \"{}\"\n",
            Ed25519Signer::from_seed(&agent("alpha"), &[1; 32]).kid(),
            encode_public_key(&Ed25519Signer::from_seed(&agent("alpha"), &[1; 32]).public_key()),
        );
        assert!(
            TrustStore::parse(&file(&gateway_role)).is_err(),
            "gateway role on non-gateway"
        );
    }

    #[test]
    fn req_0007_006_operator_role_requires_the_operator_agent_and_name() {
        let ok = key_block_with_role("operator", 7, "operator", "");
        assert!(TrustStore::parse(&file(&ok)).is_ok());
        for (name, role) in [
            ("tui", "operator"),
            ("cli", "operator"),
            ("alice", "operator"),
            ("operator", "agent"),
            ("operator", "gateway"),
            ("tui", "agent"),
        ] {
            let body = key_block_with_role(name, 7, role, "");
            assert!(TrustStore::parse(&file(&body)).is_err(), "{name} as {role}");
        }
    }

    #[test]
    fn req_0007_038_relay_identities_match_only_the_operator_key() {
        let operator = agent("operator");
        for relay in OPERATOR_RELAY_IDS {
            assert!(sender_matches_key(relay, &operator), "{relay} relays");
            assert!(
                !sender_matches_key(relay, &agent("alice")),
                "{relay} is not alice"
            );
        }
        assert!(!sender_matches_key("operator", &operator));
        assert!(!sender_matches_key("alice", &operator));
        assert!(sender_matches_key("alice", &agent("alice")));
        assert!(!sender_matches_key("tui", &agent("alice")));
    }

    #[test]
    fn req_0007_022_mcp_direct_only_for_agent_role() {
        let body = key_block("alpha", 1, "mcp_direct = true");
        assert!(TrustStore::parse(&file(&body)).is_ok());
        let as_gateway = format!(
            "[[key]]\nkid = \"{kid}\"\nagent = \"gateway\"\nrole = \"gateway\"\npublic_key = \"{pk}\"\nmcp_direct = true\n",
            kid = Ed25519Signer::from_seed(&agent("gateway"), &[5; 32]).kid(),
            pk = encode_public_key(
                &Ed25519Signer::from_seed(&agent("gateway"), &[5; 32]).public_key()
            ),
        );
        assert!(TrustStore::parse(&file(&as_gateway)).is_err());
    }

    #[test]
    fn req_0007_022_tenants_must_be_non_empty_and_valid() {
        assert!(TrustStore::parse(&file(&key_block("alpha", 1, "tenants = []"))).is_err());
        assert!(
            TrustStore::parse(&file(&key_block("alpha", 1, "tenants = [\"Bad Tenant\"]"))).is_err()
        );
    }

    #[test]
    fn req_0007_022_retired_requires_not_after_and_window_must_be_ordered() {
        let retired = key_block("alpha", 1, "status = \"retired\"");
        assert!(TrustStore::parse(&file(&retired)).is_err());
        let reversed = key_block(
            "alpha",
            1,
            "not_before = \"2031-01-01T00:00:00Z\"\nnot_after = \"2030-01-01T00:00:00Z\"",
        );
        assert!(TrustStore::parse(&file(&reversed)).is_err());
    }

    #[test]
    fn req_0007_039_validity_window_and_revocation_are_checked_at_now() {
        let body = key_block(
            "alpha",
            1,
            "not_before = \"2026-01-01T00:00:00Z\"\nnot_after = \"2027-01-01T00:00:00Z\"",
        );
        let store = TrustStore::parse(&file(&body)).expect("loads");
        let kid = Ed25519Signer::from_seed(&agent("alpha"), &[1; 32])
            .kid()
            .to_string();
        let entry = store.entry(&kid).expect("present");
        let before = crate::clock::parse_ts_ms("2025-12-31T23:59:59.999Z").expect("ts");
        let inside = crate::clock::parse_ts_ms("2026-06-01T00:00:00.000Z").expect("ts");
        let at_end = crate::clock::parse_ts_ms("2027-01-01T00:00:00.000Z").expect("ts");
        assert_eq!(
            entry.check_validity(before).err().map(|e| e.reason()),
            Some(Reason::SignatureKeyInactive)
        );
        assert!(entry.check_validity(inside).is_ok());
        assert_eq!(
            entry.check_validity(at_end).err().map(|e| e.reason()),
            Some(Reason::SignatureKeyInactive),
            "not_after is exclusive"
        );
    }

    #[test]
    fn req_0007_039_revoked_key_is_reported_as_revoked_with_kid() {
        let body = key_block("alpha", 1, "status = \"revoked\"");
        let store = TrustStore::parse(&file(&body)).expect("loads");
        let kid = Ed25519Signer::from_seed(&agent("alpha"), &[1; 32])
            .kid()
            .to_string();
        let err = store.entry(&kid).expect("present").check_validity(0).err();
        assert_eq!(
            err.as_ref().map(BusError::reason),
            Some(Reason::SignatureKeyRevoked)
        );
        assert_eq!(
            err.map(|e| e.to_status().message),
            Some("SIGNATURE_KEY_REVOKED".to_owned())
        );
    }

    #[test]
    fn req_0007_031_require_signature_is_per_agent() {
        let body = format!(
            "{}{}",
            key_block("alpha", 1, "require_signature = true"),
            key_block("beta", 2, "")
        );
        let store = TrustStore::parse(&file(&body)).expect("loads");
        assert!(store.agent_requires_signature(&agent("alpha")));
        assert!(!store.agent_requires_signature(&agent("beta")));
        assert!(store.active_for_agent(&agent("alpha")).is_some());
    }

    #[test]
    fn req_0007_022_malformed_toml_is_refused_without_echoing_input() {
        let err = TrustStore::parse("schema = \"a2abus.trust/1\"\n[[key\nsecret = 1").err();
        assert!(matches!(&err, Some(SecurityError::Trust(m)) if m == "malformed TOML"));
    }

    #[test]
    fn req_0007_023_reload_keeps_last_good_state_when_file_is_invalid() {
        let dir = std::env::temp_dir().join(format!("a2a-trust-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("trust.toml");
        let good = file(&key_block("alpha", 1, ""));
        std::fs::write(&path, &good).expect("write");
        let holder = TrustHolder::new(TrustStore::parse(&good).expect("loads"));
        std::fs::write(&path, file("[[key]]\nkid = \"x\"\n")).expect("write");
        let rejected = holder.reload(&path).err();
        assert_eq!(
            rejected.as_ref().map(|r| r.file.clone()),
            Some(path.clone())
        );
        assert_eq!(
            holder.snapshot().entries().count(),
            1,
            "previous state kept"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
