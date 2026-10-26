//! Capability declaration and profile checks (RFC-0011 [0011-030..037]).

use std::time::Duration;

use thiserror::Error;

/// Minimum dedupe window of profile `a2a-bus-m1`: 3600 s plus 30 s staleness
/// (RFC-0002 [0002-070, 0002-072], RFC-0011 [0011-031]).
pub const M1_MIN_DEDUPE_WINDOW: Duration = Duration::from_secs(3630);

/// Minimum message size of profile `a2a-bus-m1`: the inline limit of
/// RFC-0002 [0002-110].
pub const M1_MIN_MESSAGE_SIZE: usize = 262_144;

/// How the binding keeps stream and KV state (RFC-0011 [0011-063]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Persistence {
    /// State lives in process memory. A simulated restart keeps it; a real
    /// process exit does not.
    Memory,
    /// State survives process exit on disk.
    Disk,
}

/// Declared capability values of one binding instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// Messages carry headers.
    pub headers: bool,
    /// Request/reply is supported.
    pub request_reply: bool,
    /// A request with no responder fails with `NoResponder` rather than waiting.
    pub no_responder_signal: bool,
    /// Streams are durable: an acknowledged append is stored.
    pub durable_streams: bool,
    /// Duplicate suppression window for append message ids.
    pub dedupe_window: Duration,
    /// Consumers can start from an explicit sequence number.
    pub replay_from_seq: bool,
    /// Messages of one subject are delivered in stream order.
    pub ordered_per_subject: bool,
    /// Largest payload accepted on publish or append, in bytes.
    pub max_message_size: usize,
    /// Bound on in-flight deliveries per consumer and queued requests per server.
    pub max_pending: usize,
    /// KV buckets are supported.
    pub kv: bool,
    /// KV entries accept a per-write TTL.
    pub kv_ttl: bool,
    /// Watches deliver an end-of-snapshot marker.
    pub kv_snapshot_end: bool,
    /// Largest KV value in bytes.
    pub kv_max_value_size: usize,
    /// Content-addressed blobs are supported.
    pub blob_store: bool,
    /// Largest blob in bytes.
    pub blob_max_size: usize,
    /// The binding keeps its guarantees across partitions (recommended only).
    pub partition_tolerant: bool,
    /// Declared storage mode.
    pub persistence: Persistence,
}

/// Names of the boolean capabilities, as used in profile checks and errors.
pub const FLAG_NAMES: [&str; 11] = [
    "headers",
    "request_reply",
    "no_responder_signal",
    "durable_streams",
    "replay_from_seq",
    "ordered_per_subject",
    "kv",
    "kv_ttl",
    "kv_snapshot_end",
    "blob_store",
    "partition_tolerant",
];

impl Capabilities {
    /// Value of a boolean capability by name, or `None` for an unknown or
    /// non-boolean name.
    #[must_use]
    pub fn flag(&self, name: &str) -> Option<bool> {
        Some(match name {
            "headers" => self.headers,
            "request_reply" => self.request_reply,
            "no_responder_signal" => self.no_responder_signal,
            "durable_streams" => self.durable_streams,
            "replay_from_seq" => self.replay_from_seq,
            "ordered_per_subject" => self.ordered_per_subject,
            "kv" => self.kv,
            "kv_ttl" => self.kv_ttl,
            "kv_snapshot_end" => self.kv_snapshot_end,
            "blob_store" => self.blob_store,
            "partition_tolerant" => self.partition_tolerant,
            _ => return None,
        })
    }

    /// Returns a copy with the boolean capability `name` set to `value`, or
    /// `None` when `name` is not a boolean capability.
    #[must_use]
    pub fn with_flag(mut self, name: &str, value: bool) -> Option<Self> {
        let slot = match name {
            "headers" => &mut self.headers,
            "request_reply" => &mut self.request_reply,
            "no_responder_signal" => &mut self.no_responder_signal,
            "durable_streams" => &mut self.durable_streams,
            "replay_from_seq" => &mut self.replay_from_seq,
            "ordered_per_subject" => &mut self.ordered_per_subject,
            "kv" => &mut self.kv,
            "kv_ttl" => &mut self.kv_ttl,
            "kv_snapshot_end" => &mut self.kv_snapshot_end,
            "blob_store" => &mut self.blob_store,
            "partition_tolerant" => &mut self.partition_tolerant,
            _ => return None,
        };
        *slot = value;
        Some(self)
    }
}

/// One condition a profile places on a binding's capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requirement {
    /// A boolean capability must be declared `true`.
    Flag(&'static str),
    /// The dedupe window must be at least this long.
    MinDedupeWindow(Duration),
    /// The maximum message size must be at least this many bytes.
    MinMessageSize(usize),
}

impl Requirement {
    /// The capability name this requirement is reported under.
    #[must_use]
    pub const fn capability(&self) -> &'static str {
        match self {
            Self::Flag(name) => name,
            Self::MinDedupeWindow(_) => "dedupe_window",
            Self::MinMessageSize(_) => "max_message_size",
        }
    }

    /// Whether `caps` satisfies this requirement.
    #[must_use]
    pub fn satisfied_by(&self, caps: &Capabilities) -> bool {
        match self {
            Self::Flag(name) => caps.flag(name).unwrap_or(false),
            Self::MinDedupeWindow(min) => caps.dedupe_window >= *min,
            Self::MinMessageSize(min) => caps.max_message_size >= *min,
        }
    }
}

/// A named set of requirements a binding must satisfy to claim the profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    name: &'static str,
    requirements: &'static [Requirement],
}

/// Profile `a2a-bus-m1` (RFC-0011 [0011-034]).
pub const A2A_BUS_M1: Profile = Profile {
    name: "a2a-bus-m1",
    requirements: &[
        Requirement::Flag("headers"),
        Requirement::Flag("request_reply"),
        Requirement::Flag("no_responder_signal"),
        Requirement::Flag("durable_streams"),
        Requirement::MinDedupeWindow(M1_MIN_DEDUPE_WINDOW),
        Requirement::Flag("replay_from_seq"),
        Requirement::Flag("ordered_per_subject"),
        Requirement::MinMessageSize(M1_MIN_MESSAGE_SIZE),
        Requirement::Flag("kv"),
        Requirement::Flag("kv_ttl"),
        Requirement::Flag("kv_snapshot_end"),
        Requirement::Flag("blob_store"),
    ],
};

/// Registry of profile names (RFC-0011 [0011-037]). Unknown names fail.
pub const PROFILE_REGISTRY: &[Profile] = &[A2A_BUS_M1];

/// A profile lookup or check failure.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProfileError {
    /// The name is not in the profile registry.
    #[error("unknown profile `{name}`")]
    UnknownProfile { name: String },
    /// The binding does not satisfy the profile; `missing` lists every
    /// unsatisfied capability.
    #[error("profile `{profile}` not satisfied; missing: {}", missing.join(", "))]
    Unsatisfied {
        profile: &'static str,
        missing: Vec<&'static str>,
    },
}

impl Profile {
    /// Looks up a profile by its registry name.
    ///
    /// # Errors
    /// Returns [`ProfileError::UnknownProfile`] for names outside the registry.
    pub fn by_name(name: &str) -> Result<&'static Self, ProfileError> {
        PROFILE_REGISTRY
            .iter()
            .find(|profile| profile.name == name)
            .ok_or_else(|| ProfileError::UnknownProfile {
                name: name.to_owned(),
            })
    }

    /// The registry name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// The requirements this profile places on a binding.
    #[must_use]
    pub const fn requirements(&self) -> &'static [Requirement] {
        self.requirements
    }

    /// Checks `caps` against every requirement and reports all failures at once.
    ///
    /// # Errors
    /// Returns [`ProfileError::Unsatisfied`] naming every missing capability.
    pub fn check(&self, caps: &Capabilities) -> Result<(), ProfileError> {
        let missing: Vec<&'static str> = self
            .requirements
            .iter()
            .filter(|requirement| !requirement.satisfied_by(caps))
            .map(Requirement::capability)
            .collect();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(ProfileError::Unsatisfied {
                profile: self.name,
                missing,
            })
        }
    }
}

#[cfg(test)]
#[allow(clippy::panic, reason = "test setup failures abort the test")]
mod tests {
    use super::*;

    fn full() -> Capabilities {
        Capabilities {
            headers: true,
            request_reply: true,
            no_responder_signal: true,
            durable_streams: true,
            dedupe_window: M1_MIN_DEDUPE_WINDOW,
            replay_from_seq: true,
            ordered_per_subject: true,
            max_message_size: M1_MIN_MESSAGE_SIZE,
            max_pending: 1024,
            kv: true,
            kv_ttl: true,
            kv_snapshot_end: true,
            kv_max_value_size: 262_144,
            blob_store: true,
            blob_max_size: 16 * 1024 * 1024,
            partition_tolerant: true,
            persistence: Persistence::Memory,
        }
    }

    // [0011-034] the M1 profile is satisfied by a fully declared binding.
    #[test]
    fn req_0011_034_m1_accepts_full_declaration() {
        assert!(A2A_BUS_M1.check(&full()).is_ok());
    }

    // [0011-035] a missing required capability fails startup and is named.
    #[test]
    fn req_0011_035_missing_capability_is_named() {
        let caps = full().with_flag("kv_ttl", false);
        let Some(caps) = caps else {
            panic!("kv_ttl must be a flag");
        };
        match A2A_BUS_M1.check(&caps) {
            Err(ProfileError::Unsatisfied { missing, .. }) => {
                assert_eq!(missing, vec!["kv_ttl"]);
            }
            other => panic!("expected Unsatisfied, got {other:?}"),
        }
    }

    // [0011-031] the dedupe window must reach 3630 s for M1.
    #[test]
    fn req_0011_031_dedupe_window_boundary() {
        let mut caps = full();
        caps.dedupe_window = M1_MIN_DEDUPE_WINDOW - Duration::from_secs(1);
        match A2A_BUS_M1.check(&caps) {
            Err(ProfileError::Unsatisfied { missing, .. }) => {
                assert_eq!(missing, vec!["dedupe_window"]);
            }
            other => panic!("expected Unsatisfied, got {other:?}"),
        }
    }

    // [0011-032] max_message_size of profile a2a-bus-m1 is at least 262144 bytes.
    #[test]
    fn req_0011_032_message_size_boundary_for_profile() {
        let mut caps = full();
        caps.max_message_size = M1_MIN_MESSAGE_SIZE - 1;
        assert!(A2A_BUS_M1.check(&caps).is_err());
        caps.max_message_size = M1_MIN_MESSAGE_SIZE;
        assert!(A2A_BUS_M1.check(&caps).is_ok());
    }

    // [0011-037] profile names are validated against the registry.
    #[test]
    fn req_0011_037_profile_names_are_validated() {
        assert!(Profile::by_name("a2a-bus-m1").is_ok());
        assert_eq!(
            Profile::by_name("a2a-bus-m9"),
            Err(ProfileError::UnknownProfile {
                name: "a2a-bus-m9".to_owned()
            })
        );
    }

    #[test]
    fn flag_names_resolve_and_numeric_names_do_not() {
        let caps = full();
        for name in FLAG_NAMES {
            assert!(caps.flag(name).is_some(), "{name} not a flag");
        }
        assert_eq!(caps.flag("dedupe_window"), None);
        assert!(caps.clone().with_flag("dedupe_window", false).is_none());
    }
}
