//! Identifier newtypes of the A2A-Bus vocabulary (RFC-0001 sec 4.4).
//!
//! * [`TenantId`] and [`AgentId`] are validated lowercase tokens (`[a-z0-9][a-z0-9-]*`, bounded).
//!   The reserved service identities `gateway`, `tui`, `cli` and `supervisor` are valid
//!   [`AgentId`] values; [`AgentId::is_reserved`] tells them apart.
//! * [`TaskId`], [`MessageId`] and [`ContextId`] are opaque, bounded, printable-ASCII strings
//!   ([`OPAQUE_ID_MAX_LEN`] bytes, no spaces). Ids received from peers are kept verbatim
//!   (RFC-0008 [0008-035], [0008-085]); a peer may use any such form, for example a 32-hex
//!   message id or a bridged `task-<hex>` task id.
//! * The bus generates its own ids with `generate()`, which returns a canonical lowercase UUIDv7
//!   string (RFC-0005 [0005-001], RFC-0002 [0002-031]). Generated ids are strictly increasing
//!   within a process. [`TaskId::is_uuidv7`] and its siblings tell whether a value has that form.

use std::fmt;
use std::str::FromStr;
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use uuid::{Uuid, Variant, Version};

/// Maximum length of a tenant id (RFC-0001 sec 4.4).
pub const TENANT_ID_MAX_LEN: usize = 32;
/// Maximum length of an agent id (RFC-0001 sec 4.4).
pub const AGENT_ID_MAX_LEN: usize = 48;
/// Maximum length in bytes of an opaque id (task, message or context id). The bound is a choice of
/// this crate (see the crate AGENTS.md).
pub const OPAQUE_ID_MAX_LEN: usize = 256;

/// Error returned when a textual identifier does not satisfy its grammar.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    /// The value is empty.
    #[error("identifier is empty")]
    Empty,
    /// The value is longer than the grammar allows.
    #[error("identifier is {len} bytes, maximum is {max}")]
    TooLong {
        /// Maximum allowed length in bytes.
        max: usize,
        /// Actual length in bytes.
        len: usize,
    },
    /// The value contains a character outside the grammar.
    #[error("identifier contains a forbidden character at byte {index}")]
    InvalidChar {
        /// Byte offset of the first forbidden character.
        index: usize,
    },
    /// The first character of a token is not `[a-z0-9]`.
    #[error("token must start with [a-z0-9]")]
    BadStart,
}

fn validate_token(value: &str, max: usize) -> Result<(), IdError> {
    if value.is_empty() {
        return Err(IdError::Empty);
    }
    if value.len() > max {
        return Err(IdError::TooLong {
            max,
            len: value.len(),
        });
    }
    for (index, byte) in value.bytes().enumerate() {
        let ok = byte.is_ascii_lowercase() || byte.is_ascii_digit() || (index > 0 && byte == b'-');
        if !ok {
            return if index == 0 {
                Err(IdError::BadStart)
            } else {
                Err(IdError::InvalidChar { index })
            };
        }
    }
    Ok(())
}

/// Validates the opaque grammar: 1 to [`OPAQUE_ID_MAX_LEN`] bytes of printable ASCII, no spaces.
fn validate_opaque(value: &str) -> Result<(), IdError> {
    if value.is_empty() {
        return Err(IdError::Empty);
    }
    if value.len() > OPAQUE_ID_MAX_LEN {
        return Err(IdError::TooLong {
            max: OPAQUE_ID_MAX_LEN,
            len: value.len(),
        });
    }
    if let Some(index) = value.bytes().position(|b| !(0x21..=0x7E).contains(&b)) {
        return Err(IdError::InvalidChar { index });
    }
    Ok(())
}

/// True when `value` is a canonical lowercase hyphenated UUIDv7 (36 characters, RFC 9562 variant).
fn is_canonical_uuid_v7(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (index, byte) in bytes.iter().enumerate() {
        let expected_hyphen = matches!(index, 8 | 13 | 18 | 23);
        let ok = if expected_hyphen {
            *byte == b'-'
        } else {
            byte.is_ascii_digit() || (b'a'..=b'f').contains(byte)
        };
        if !ok {
            return false;
        }
    }
    Uuid::parse_str(value).is_ok_and(|uuid| {
        uuid.get_version() == Some(Version::SortRand) && uuid.get_variant() == Variant::RFC4122
    })
}

/// Process-wide state of the UUIDv7 generator. `last_ms` and `counter` order ids that are
/// generated in the same millisecond (RFC 9562 sec 6.2, method 1: a 74-bit counter that
/// starts at a random value and is incremented while the clock does not advance).
struct V7State {
    last_ms: u64,
    counter: u128,
}

static V7_STATE: Mutex<V7State> = Mutex::new(V7State {
    last_ms: 0,
    counter: 0,
});

const COUNTER_BITS: u32 = 74;
const COUNTER_MASK: u128 = (1u128 << COUNTER_BITS) - 1;

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        & 0xFFFF_FFFF_FFFF
}

fn random_counter() -> u128 {
    // Clear the top bit so that one increment step cannot overflow within a millisecond.
    Uuid::new_v4().as_u128() & (COUNTER_MASK >> 1)
}

/// Generates a UUIDv7 that is strictly greater than every UUIDv7 this process returned before.
fn next_uuid_v7() -> Uuid {
    let mut state = V7_STATE.lock().unwrap_or_else(PoisonError::into_inner);
    let now = unix_millis();
    if now > state.last_ms {
        state.last_ms = now;
        state.counter = random_counter();
    } else {
        // Same millisecond, or the clock moved backwards: keep the last timestamp and step the
        // counter. On overflow of the 74-bit counter, advance the timestamp by one millisecond.
        let next = state.counter.wrapping_add(1) & COUNTER_MASK;
        if next == 0 {
            state.last_ms = state.last_ms.saturating_add(1) & 0xFFFF_FFFF_FFFF;
            state.counter = random_counter();
        } else {
            state.counter = next;
        }
    }
    let rand_a = (state.counter >> 62) as u16; // top 12 bits of the counter
    let rand_b = (state.counter & ((1u128 << 62) - 1)) as u64; // low 62 bits
    let ms = state.last_ms;
    let mut bytes = [0u8; 16];
    bytes[0..6].copy_from_slice(&ms.to_be_bytes()[2..8]);
    bytes[6] = 0x70 | ((rand_a >> 8) as u8 & 0x0F);
    bytes[7] = rand_a as u8;
    bytes[8] = 0x80 | ((rand_b >> 56) as u8 & 0x3F);
    bytes[9..16].copy_from_slice(&rand_b.to_be_bytes()[1..8]);
    Uuid::from_bytes(bytes)
}

/// Tenant identifier, `[a-z0-9][a-z0-9-]{0,31}` (M1 uses `local`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TenantId(String);

impl TenantId {
    /// Parses and validates a tenant id.
    ///
    /// # Errors
    /// Returns [`IdError`] when the value violates the grammar.
    pub fn parse(value: &str) -> Result<Self, IdError> {
        validate_token(value, TENANT_ID_MAX_LEN)?;
        Ok(Self(value.to_owned()))
    }

    /// Returns the id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TenantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for TenantId {
    type Err = IdError;
    fn from_str(s: &str) -> Result<Self, IdError> {
        Self::parse(s)
    }
}

/// Agent identifier, `[a-z0-9][a-z0-9-]{0,47}`, unique per tenant.
///
/// The reserved service identities (`gateway`, `tui`, `cli`, `supervisor`) are valid agent-id
/// tokens for sender and audit identities. They MUST NOT name a Worker that runs tasks; that
/// check belongs to the Worker (RFC-0001 sec 4.4, RFC-0010 [0010-011]).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AgentId(String);

/// The reserved service identities of RFC-0001 sec 4.4.
pub const RESERVED_SERVICE_IDS: [&str; 4] = ["gateway", "tui", "cli", "supervisor"];

impl AgentId {
    /// Parses and validates an agent id.
    ///
    /// # Errors
    /// Returns [`IdError`] when the value violates the grammar.
    pub fn parse(value: &str) -> Result<Self, IdError> {
        validate_token(value, AGENT_ID_MAX_LEN)?;
        Ok(Self(value.to_owned()))
    }

    /// The reserved `gateway` service identity.
    #[must_use]
    pub fn gateway() -> Self {
        Self("gateway".to_owned())
    }

    /// The reserved `tui` service identity.
    #[must_use]
    pub fn tui() -> Self {
        Self("tui".to_owned())
    }

    /// The reserved `cli` service identity.
    #[must_use]
    pub fn cli() -> Self {
        Self("cli".to_owned())
    }

    /// The reserved `supervisor` service identity.
    #[must_use]
    pub fn supervisor() -> Self {
        Self("supervisor".to_owned())
    }

    /// Returns true when the id is one of [`RESERVED_SERVICE_IDS`].
    #[must_use]
    pub fn is_reserved(&self) -> bool {
        RESERVED_SERVICE_IDS.contains(&self.0.as_str())
    }

    /// Returns the id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for AgentId {
    type Err = IdError;
    fn from_str(s: &str) -> Result<Self, IdError> {
        Self::parse(s)
    }
}

macro_rules! opaque_id {
    ($(#[$meta:meta])* $name:ident, $what:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(String);

        impl $name {
            #[doc = concat!("Generates a new ", $what, " as a canonical lowercase UUIDv7 string. Strictly increasing within the process.")]
            #[must_use]
            pub fn generate() -> Self {
                Self(next_uuid_v7().as_hyphenated().to_string())
            }

            #[doc = concat!("Parses a ", $what, " received from a peer or a stored record. The value must be an opaque token: 1 to [`OPAQUE_ID_MAX_LEN`] bytes of printable ASCII without spaces. It is kept verbatim and never rewritten.")]
            ///
            /// # Errors
            /// Returns [`IdError`] when the value is empty, too long, or contains a byte outside
            /// printable non-space ASCII.
            pub fn parse(value: &str) -> Result<Self, IdError> {
                validate_opaque(value)?;
                Ok(Self(value.to_owned()))
            }

            /// True when the value is a canonical lowercase UUIDv7 string, the form
            /// [`Self::generate`] produces. Opaque peer values may return false.
            #[must_use]
            pub fn is_uuidv7(&self) -> bool {
                is_canonical_uuid_v7(&self.0)
            }

            /// Returns the id as a string slice.
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
            type Err = IdError;
            fn from_str(s: &str) -> Result<Self, IdError> {
                Self::parse(s)
            }
        }
    };
}

opaque_id!(
    /// Task identifier (RFC-0005 [0005-001], RFC-0002 [0002-004]). The owner Worker assigns
    /// [`TaskId::generate`] values; a peer-created task id may be any opaque token (for example a
    /// bridged `task-<hex>` id) and is kept verbatim.
    TaskId,
    "task id"
);

opaque_id!(
    /// A2A `messageId` and `A2AB-Msg-Id` value (RFC-0002 [0002-031], RFC-0008 [0008-085]).
    /// [`MessageId::generate`] gives a UUIDv7 for messages the bus creates. Peer message ids in
    /// other forms, such as 32-hex strings, are kept verbatim.
    MessageId,
    "message id"
);

opaque_id!(
    /// Context identifier (RFC-0005 [0005-002], RFC-0008 [0008-035]). [`ContextId::generate`] gives
    /// a UUIDv7 for contexts the Worker creates. Supplied values are kept unchanged.
    ContextId,
    "context id"
);

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // [0010-019] tenant grammar: [a-z0-9][a-z0-9-]{0,31}
    #[test]
    fn req_0010_019_tenant_grammar_accepts_boundaries() {
        assert!(TenantId::parse("local").is_ok());
        assert!(TenantId::parse("a").is_ok());
        assert!(TenantId::parse("0-a").is_ok());
        let max = "a".repeat(TENANT_ID_MAX_LEN);
        assert_eq!(TenantId::parse(&max).unwrap().as_str(), max);
    }

    // [0010-019] tenant grammar rejects every out-of-grammar form
    #[test]
    fn req_0010_019_tenant_grammar_rejects_out_of_grammar() {
        assert_eq!(TenantId::parse(""), Err(IdError::Empty));
        assert_eq!(TenantId::parse("-a"), Err(IdError::BadStart));
        assert_eq!(
            TenantId::parse("A"),
            Err(IdError::BadStart),
            "uppercase is not allowed"
        );
        assert_eq!(
            TenantId::parse("a.b"),
            Err(IdError::InvalidChar { index: 1 })
        );
        assert_eq!(
            TenantId::parse("a*"),
            Err(IdError::InvalidChar { index: 1 })
        );
        assert_eq!(
            TenantId::parse("a b"),
            Err(IdError::InvalidChar { index: 1 })
        );
        let too_long = "a".repeat(TENANT_ID_MAX_LEN + 1);
        assert_eq!(
            TenantId::parse(&too_long),
            Err(IdError::TooLong {
                max: TENANT_ID_MAX_LEN,
                len: TENANT_ID_MAX_LEN + 1
            })
        );
    }

    // [0010-019] agent grammar: [a-z0-9][a-z0-9-]{0,47}
    #[test]
    fn req_0010_019_agent_grammar_boundaries() {
        for ok in ["omp", "claude", "hermes", "a-b-1"] {
            assert!(AgentId::parse(ok).is_ok(), "{ok} must be accepted");
        }
        let max = "a".repeat(AGENT_ID_MAX_LEN);
        assert!(AgentId::parse(&max).is_ok());
        let too_long = "a".repeat(AGENT_ID_MAX_LEN + 1);
        assert!(matches!(
            AgentId::parse(&too_long),
            Err(IdError::TooLong { .. })
        ));
        assert_eq!(AgentId::parse("_x"), Err(IdError::BadStart));
        assert_eq!(
            AgentId::parse("x_y"),
            Err(IdError::InvalidChar { index: 1 })
        );
    }

    // [0010-011] [0013-009] reserved service ids are valid sender identities and are flagged
    #[test]
    fn req_0010_011_reserved_service_ids_are_flagged() {
        let reserved = [
            AgentId::gateway(),
            AgentId::tui(),
            AgentId::cli(),
            AgentId::supervisor(),
        ];
        for id in &reserved {
            assert!(id.is_reserved(), "{id} must be reserved");
            assert_eq!(AgentId::parse(id.as_str()).unwrap(), *id);
        }
        assert!(!AgentId::parse("omp").unwrap().is_reserved());
        assert!(!AgentId::parse("supervisors").unwrap().is_reserved());
    }

    // [0002-004] Worker-generated task ids are canonical lowercase UUIDv7 strings
    #[test]
    fn req_0002_004_generated_task_id_is_canonical_uuid_v7() {
        let id = TaskId::generate();
        let text = id.to_string();
        assert_eq!(text.len(), 36);
        assert!(id.is_uuidv7());
        assert_eq!(TaskId::parse(&text).unwrap(), id);
        // RFC-0005 example task id is a canonical UUIDv7.
        assert!(
            TaskId::parse("0195f3a2-6c20-7e21-9b4d-2a7c8e9f0a12")
                .unwrap()
                .is_uuidv7()
        );
    }

    // [0002-004] peer task ids are kept verbatim; is_uuidv7 reports only the canonical v7 form
    #[test]
    fn req_0002_004_peer_task_ids_are_opaque_and_verbatim() {
        let v7 = "0195f3a2-6c20-7e21-9b4d-2a7c8e9f0a12";
        for peer in [
            v7.to_uppercase(),
            v7.replace('-', ""),
            format!("{{{v7}}}"),
            "task-9c1e0f4a2b7d3e58".to_owned(),
            "0195f3a2-6c20-4e21-9b4d-2a7c8e9f0a12".to_owned(),
        ] {
            let id = TaskId::parse(&peer).unwrap();
            assert_eq!(id.as_str(), peer, "opaque id must be preserved verbatim");
            assert!(!id.is_uuidv7(), "{peer} is not a canonical UUIDv7");
        }
    }

    // [0002-004] task id opaque grammar boundaries
    #[test]
    fn req_0002_004_task_id_opaque_grammar_boundaries() {
        assert_eq!(TaskId::parse(""), Err(IdError::Empty));
        assert_eq!(
            TaskId::parse("task 1"),
            Err(IdError::InvalidChar { index: 4 })
        );
        assert_eq!(
            TaskId::parse("a\u{7}b"),
            Err(IdError::InvalidChar { index: 1 })
        );
        let max = "t".repeat(OPAQUE_ID_MAX_LEN);
        assert!(TaskId::parse(&max).is_ok());
        let over = "t".repeat(OPAQUE_ID_MAX_LEN + 1);
        assert_eq!(
            TaskId::parse(&over),
            Err(IdError::TooLong {
                max: OPAQUE_ID_MAX_LEN,
                len: OPAQUE_ID_MAX_LEN + 1
            })
        );
    }

    // [0002-031] generated message ids are UUIDv7 and round-trip through parse
    #[test]
    fn req_0002_031_message_id_generation_is_uuid_v7() {
        for _ in 0..100 {
            let id = MessageId::generate();
            assert!(id.is_uuidv7());
            let parsed = MessageId::parse(&id.to_string()).unwrap();
            assert_eq!(parsed, id);
        }
    }

    // [0008-085] peer message ids such as Hermes 32-hex values are accepted verbatim
    #[test]
    fn req_0002_031_peer_message_id_forms_are_preserved() {
        let hermes = "9b1f0c2e3a4d5f60718293a4b5c6d7e8";
        let id = MessageId::parse(hermes).unwrap();
        assert_eq!(id.as_str(), hermes);
        assert!(!id.is_uuidv7());
        assert!(MessageId::parse("").is_err());
    }

    // [0002-004] [0002-031] generated ids are strictly increasing in one process
    #[test]
    fn req_0002_031_generated_ids_are_strictly_increasing() {
        let mut previous = TaskId::generate().to_string();
        for _ in 0..20_000 {
            let next = TaskId::generate().to_string();
            assert!(next > previous, "{next} must sort after {previous}");
            previous = next;
        }
    }

    // [0005-002] generated context ids are UUIDv7 strings; supplied ids are kept unchanged
    #[test]
    fn req_0005_002_context_id_generate_and_supplied_forms() {
        let generated = ContextId::generate();
        assert!(generated.is_uuidv7());
        // RFC-0008 example: external context strings are not UUIDs and must pass unchanged.
        let external = ContextId::parse("ctx-9c1e0f4a2b7d3e58").unwrap();
        assert_eq!(external.as_str(), "ctx-9c1e0f4a2b7d3e58");
        assert!(!external.is_uuidv7());
    }

    // [0005-002] context id boundaries
    #[test]
    fn req_0005_002_context_id_rejects_empty_space_and_overlong() {
        assert_eq!(ContextId::parse(""), Err(IdError::Empty));
        assert_eq!(
            ContextId::parse("a b"),
            Err(IdError::InvalidChar { index: 1 })
        );
        assert_eq!(
            ContextId::parse("a\u{7}b"),
            Err(IdError::InvalidChar { index: 1 })
        );
        let max = "x".repeat(OPAQUE_ID_MAX_LEN);
        assert!(ContextId::parse(&max).is_ok());
        let over = "x".repeat(OPAQUE_ID_MAX_LEN + 1);
        assert!(matches!(
            ContextId::parse(&over),
            Err(IdError::TooLong { .. })
        ));
    }
}
