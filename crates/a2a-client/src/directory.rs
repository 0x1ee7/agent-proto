//! Discovery: the card and presence view of one tenant (RFC-0004 section 4.4).
//!
//! The view is built from the KV watch snapshots of the tenant's card and presence buckets
//! (RFC-0010 [0010-022], [0010-029]). A sync reads each watch up to its end-of-snapshot marker
//! (RFC-0004 [0004-019]) and replaces the whole view. The view is never presented as current
//! after [`VIEW_MAX_AGE_MS`] without a sync ([0004-020]); queries then fail closed.

use std::collections::BTreeMap;
use std::fmt;

use a2a_security::card::verify_card;
use a2a_security::clock::Clock as SecurityClock;
use a2a_types::ids::AgentId;
use bus_api::bus::{Bus, Watch};
use bus_api::message::WatchEvent;
use bus_api::name::{Bucket, KeyPrefix};
use bytes::Bytes;
use serde_json::Value;

use crate::client::Client;
use crate::error::{ClientError, from_bus};
use crate::presence::Presence;

/// Longest age of a view, measured by the local clock, before it must be synced again
/// ([0004-020], [0004-027]).
pub const VIEW_MAX_AGE_MS: u64 = 60_000;

/// One card record of the card store, with its signature state.
#[derive(Debug, Clone)]
pub struct CardRecord {
    agent: AgentId,
    version: String,
    card: Value,
    verified: bool,
}

impl CardRecord {
    /// Agent id: the record key under which the card was read ([0004-044]).
    #[must_use]
    pub const fn agent(&self) -> &AgentId {
        &self.agent
    }

    /// The card `version` ([0003-035]).
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// The card as proto-JSON.
    #[must_use]
    pub const fn card(&self) -> &Value {
        &self.card
    }

    /// True when a signature verified against the pinned key of this agent ([0004-039]).
    /// Only meaningful when trust is configured; without trust it is always false.
    #[must_use]
    pub const fn verified(&self) -> bool {
        self.verified
    }
}

/// A discovery query over card records joined with presence ([0004-022]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    /// Exact agent id.
    Agent(AgentId),
    /// Exact skill id.
    Skill(String),
    /// Any tag of any skill.
    Tag(String),
    /// URI in `capabilities.extensions`.
    Extension(String),
    /// Input mode of the card or of a skill override.
    InputMode(String),
    /// Output mode of the card or of a skill override.
    OutputMode(String),
}

/// One result of a query: a card and the presence record joined to it, if any.
#[derive(Debug, Clone, Copy)]
pub struct Listing<'a> {
    /// The matched card record.
    pub card: &'a CardRecord,
    /// The live presence record of the agent, if any.
    pub presence: Option<&'a Presence>,
}

impl Listing<'_> {
    /// True when the agent is routable: presence is `READY` ([0004-014]).
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.presence.is_some_and(Presence::is_ready)
    }
}

/// Local, fixed-time clock for card signature checks made during one sync.
struct SyncClock(i64);

impl SecurityClock for SyncClock {
    fn now_ms(&self) -> i64 {
        self.0
    }
}

/// The discovery view of one client's tenant.
pub struct Directory<'a, B: Bus> {
    client: &'a Client<B>,
    cards: BTreeMap<String, CardRecord>,
    presence: BTreeMap<String, Presence>,
    synced_ms: Option<u64>,
}

impl<B: Bus> fmt::Debug for Directory<'_, B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Directory")
            .field("tenant", &self.client.tenant)
            .field("cards", &self.cards.len())
            .field("presence", &self.presence.len())
            .field("synced_ms", &self.synced_ms)
            .finish_non_exhaustive()
    }
}

impl<'a, B: Bus> Directory<'a, B> {
    /// Creates an empty, unsynced view for the client's tenant.
    #[must_use]
    pub const fn new(client: &'a Client<B>) -> Self {
        Self {
            client,
            cards: BTreeMap::new(),
            presence: BTreeMap::new(),
            synced_ms: None,
        }
    }

    /// Reads both KV snapshots and replaces the view ([0004-019]).
    ///
    /// Card records whose agent id is invalid, whose JSON is malformed, or that lack a
    /// `version` are dropped. When trust is configured, a card without a signature that verifies
    /// for its agent id is kept but reported with `verified() == false` ([0004-039], [0004-044]).
    ///
    /// # Errors
    /// Returns a bus error when a watch cannot be opened or ends early, or the clock is unreadable.
    pub async fn sync(&mut self) -> Result<(), ClientError> {
        let now = self.client.clock.now_ms().ok_or(ClientError::Clock)?;
        let now_signed = i64::try_from(now).map_err(|_| ClientError::Clock)?;
        let card_bucket = self.bucket("cards")?;
        let presence_bucket = self.bucket("presence")?;
        let raw_cards = self.snapshot(&card_bucket).await?;
        let raw_presence = self.snapshot(&presence_bucket).await?;

        let clock = SyncClock(now_signed);
        let mut cards = BTreeMap::new();
        for (key, bytes) in raw_cards {
            let Ok(agent) = AgentId::parse(&key) else {
                continue;
            };
            let Ok(card) = serde_json::from_slice::<Value>(&bytes) else {
                continue;
            };
            let Some(version) = card.get("version").and_then(Value::as_str) else {
                continue;
            };
            let verified = self.signed_by(&agent, &card, &clock);
            cards.insert(
                key,
                CardRecord {
                    agent,
                    version: version.to_owned(),
                    card,
                    verified,
                },
            );
        }

        let mut presence = BTreeMap::new();
        for (key, bytes) in raw_presence {
            if let Ok(record) = Presence::parse(&key, self.client.tenant.as_str(), &bytes) {
                presence.insert(key, record);
            }
        }

        self.cards = cards;
        self.presence = presence;
        self.synced_ms = Some(now);
        Ok(())
    }

    /// The card record of `agent`, when present and the view is fresh.
    ///
    /// # Errors
    /// Returns [`ClientError::Discovery`] when the view was never synced or is older than
    /// [`VIEW_MAX_AGE_MS`].
    pub fn card(&self, agent: &AgentId) -> Result<Option<&CardRecord>, ClientError> {
        self.fresh()?;
        Ok(self.cards.get(agent.as_str()))
    }

    /// The live presence record of `agent`, when present and the view is fresh.
    ///
    /// # Errors
    /// Same as [`Directory::card`].
    pub fn presence(&self, agent: &AgentId) -> Result<Option<&Presence>, ClientError> {
        self.fresh()?;
        Ok(self.presence.get(agent.as_str()))
    }

    /// Evaluates `query` over cards joined with presence.
    ///
    /// Only `READY` agents are returned unless `include_offline` is set ([0004-023]). When trust
    /// is configured, only cards whose signature verified are returned ([0004-039]). Results are
    /// ordered `READY` first, then by agent id in ascending byte order ([0004-023]).
    ///
    /// # Errors
    /// Same as [`Directory::card`].
    pub fn query(
        &self,
        query: &Query,
        include_offline: bool,
    ) -> Result<Vec<Listing<'_>>, ClientError> {
        self.listings(|record| matches(query, record), include_offline)
    }

    /// Lists every card joined with presence, without a skill or tag match. Same filters and
    /// order as [`Directory::query`] ([0004-021], [0004-023]).
    ///
    /// # Errors
    /// Same as [`Directory::card`].
    pub fn all(&self, include_offline: bool) -> Result<Vec<Listing<'_>>, ClientError> {
        self.listings(|_| true, include_offline)
    }

    fn listings(
        &self,
        keep: impl Fn(&CardRecord) -> bool,
        include_offline: bool,
    ) -> Result<Vec<Listing<'_>>, ClientError> {
        self.fresh()?;
        let require_signature = self.client.config.trust.is_some();
        let mut listings: Vec<Listing<'_>> = self
            .cards
            .values()
            .filter(|record| !require_signature || record.verified)
            .filter(|record| keep(record))
            .map(|record| Listing {
                card: record,
                presence: self.presence.get(record.agent.as_str()),
            })
            .filter(|listing| include_offline || listing.is_ready())
            .collect();
        listings.sort_by(|a, b| {
            b.is_ready()
                .cmp(&a.is_ready())
                .then_with(|| a.card.agent.as_str().cmp(b.card.agent.as_str()))
        });
        Ok(listings)
    }

    fn fresh(&self) -> Result<(), ClientError> {
        let now = self.client.clock.now_ms().ok_or(ClientError::Clock)?;
        let synced = self
            .synced_ms
            .ok_or(ClientError::Discovery("directory not synced"))?;
        if now.saturating_sub(synced) > VIEW_MAX_AGE_MS {
            return Err(ClientError::Discovery(
                "directory view older than 60 s; sync again",
            ));
        }
        Ok(())
    }

    /// Builds a logical bucket name such as `cards`. The bus binding adds the tenant's physical
    /// prefix (RFC-0010), so the client must not add one here.
    fn bucket(&self, name: &str) -> Result<Bucket, ClientError> {
        Bucket::new(&self.client.tenant, name).map_err(ClientError::Name)
    }

    /// Reads one KV watch up to its end-of-snapshot marker ([0004-019]).
    async fn snapshot(&self, bucket: &Bucket) -> Result<BTreeMap<String, Bytes>, ClientError> {
        let prefix = KeyPrefix::parse("").map_err(ClientError::Name)?;
        let mut watch = self
            .client
            .bus
            .kv_watch(&self.client.principal, bucket, &prefix)
            .await
            .map_err(from_bus)?;
        let mut entries = BTreeMap::new();
        loop {
            match watch.next().await.map_err(from_bus)? {
                WatchEvent::Put(entry) => {
                    entries.insert(entry.key.as_str().to_owned(), entry.value);
                }
                WatchEvent::Delete { key, .. } => {
                    entries.remove(key.as_str());
                }
                WatchEvent::SnapshotEnd => return Ok(entries),
            }
        }
    }

    fn signed_by(&self, agent: &AgentId, card: &Value, clock: &SyncClock) -> bool {
        let Some(trust) = &self.client.config.trust else {
            return false;
        };
        match verify_card(card, agent, trust, clock) {
            Ok(outcome) => outcome.require_verified().is_ok(),
            Err(_) => false,
        }
    }
}

fn matches(query: &Query, record: &CardRecord) -> bool {
    let card = &record.card;
    match query {
        Query::Agent(id) => record.agent.as_str() == id.as_str(),
        Query::Skill(id) => skills(card).any(|s| s.get("id").and_then(Value::as_str) == Some(id)),
        Query::Tag(tag) => skills(card).any(|s| strings(s, "tags").any(|t| t == tag)),
        Query::Extension(uri) => card
            .pointer("/capabilities/extensions")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items
                    .iter()
                    .any(|e| e.get("uri").and_then(Value::as_str) == Some(uri))
            }),
        Query::InputMode(mode) => {
            strings(card, "defaultInputModes").any(|m| m == mode)
                || skills(card).any(|s| strings(s, "inputModes").any(|m| m == mode))
        }
        Query::OutputMode(mode) => {
            strings(card, "defaultOutputModes").any(|m| m == mode)
                || skills(card).any(|s| strings(s, "outputModes").any(|m| m == mode))
        }
    }
}

fn skills(card: &Value) -> impl Iterator<Item = &Value> {
    card.get("skills")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn strings<'a>(value: &'a Value, key: &str) -> impl Iterator<Item = &'a str> {
    value
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
}
