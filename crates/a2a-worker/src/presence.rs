//! Agent card and presence publication (RFC-0004; card rules in RFC-0003 section 4.7).
//!
//! Each worker publishes two records per agent:
//! - the card in the `cards` bucket (no TTL), rewritten at open and every `card_refresh`;
//! - the presence record in the `presence` bucket, written with `presence_ttl` and
//!   refreshed every `presence_refresh`.
//!
//! Presence is written only after the card is published (0004-010). The worker drives
//! [`Publisher::tick`] at open and on every pump, and asks [`Publisher::next_wakeup`]
//! when the next tick is due.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use a2a_security::card::sign_card;
use a2a_security::clock::{ManualClock, format_ts_ms};
use a2a_security::identity::Signer;
use a2a_security::trust::TrustStore;
use a2a_types::ids::{AgentId, TaskId, TenantId};
use a2a_types::proto::a2a::AgentCard;
use bus_api::bus::Bus;
use bus_api::error::BusError;
use bus_api::name::{Bucket, Key, Principal};
use bus_api::value::Revision;
use bytes::Bytes;
use serde_json::{Map, Value, json};

use crate::error::WorkerError;

const CARDS: &str = "cards";
const PRESENCE: &str = "presence";
/// Transport binding the card must advertise for the bus (RFC-0003 section 4.7).
const BINDING: &str = "urn:a2abus:binding:bus:1";
const MAX_CARD_BYTES: usize = 262_144;
const MAX_PRESENCE_BYTES: usize = 1024;
/// Delay before a failed non-strict publication is retried. This is an implementation
/// choice, not an RFC value: it keeps a failing bus from turning the pump into a busy loop.
const RETRY_AFTER_MS: u64 = 1_000;
/// Create/adopt attempts per claim before the claim reports a transient failure.
const CLAIM_ATTEMPTS: usize = 3;

/// Card signing policy (RFC-0003 section 4.7).
#[derive(Clone, Default)]
pub enum CardSigning {
    /// Cards are published without signatures.
    #[default]
    Disabled,
    /// Cards are signed with `signer`. `trust` must hold the signer's key so peers can verify.
    Enabled {
        signer: Arc<dyn Signer>,
        trust: Arc<TrustStore>,
    },
}

impl fmt::Debug for CardSigning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => f.write_str("Disabled"),
            Self::Enabled { signer, .. } => f
                .debug_struct("Enabled")
                .field("kid", &signer.kid().to_string())
                .finish_non_exhaustive(),
        }
    }
}

/// Timing policy for card and presence publication (RFC-0004 section 9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// Lifetime of a presence record without refresh (0004-011).
    pub presence_ttl: Duration,
    /// Interval between presence refreshes (0004-011). Must be shorter than `presence_ttl`.
    pub presence_refresh: Duration,
    /// Interval between card rewrites (0004-006).
    pub card_refresh: Duration,
    /// Longest drain before the presence record is deleted (0004-016).
    pub drain_timeout: Duration,
    /// Unhealthy period after which refreshes stop (0004-015).
    pub health_recovery: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            presence_ttl: Duration::from_secs(30),
            presence_refresh: Duration::from_secs(10),
            card_refresh: Duration::from_secs(300),
            drain_timeout: Duration::from_secs(30),
            health_recovery: Duration::from_secs(120),
        }
    }
}

/// What a worker publishes about itself. Set on `WorkerConfig::publication` to opt in.
#[derive(Clone)]
pub struct Publication {
    /// Host-provided card content. `version` is the first published version; `signatures`
    /// must be empty because the worker owns signing.
    pub card: AgentCard,
    /// Whether the card is signed.
    pub signing: CardSigning,
    /// Refresh and expiry timing.
    pub timing: Timing,
}

impl fmt::Debug for Publication {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Publication")
            .field("card_name", &self.card.name)
            .field("card_version", &self.card.version)
            .field("signing", &self.signing)
            .field("timing", &self.timing)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Starting,
    Ready,
    Draining,
}

impl State {
    fn wire(self) -> &'static str {
        match self {
            Self::Starting => "PRESENCE_STATE_STARTING",
            Self::Ready => "PRESENCE_STATE_READY",
            Self::Draining => "PRESENCE_STATE_DRAINING",
        }
    }
}

/// The presence record as this instance last wrote it.
#[derive(Debug, Clone, Copy)]
struct Live {
    state: State,
    revision: Revision,
    written_at: u64,
}

/// Publishes the card and presence for one worker. Driven by `Worker`.
pub(crate) struct Publisher {
    tenant: String,
    agent: String,
    cards: Bucket,
    presence: Bucket,
    key: Key,
    instance: String,
    started_at: String,
    timing: Timing,
    signing: CardSigning,
    declared_version: String,
    /// Card content without `version` and `signatures`.
    content: Map<String, Value>,
    published_version: Option<String>,
    card_due: u64,
    record: Option<Live>,
    draining: bool,
    drain_started: Option<u64>,
    unhealthy_since: Option<u64>,
    retry_at: Option<u64>,
}

impl Publisher {
    /// Validates the publication and prepares the publisher. Nothing is written to the bus.
    pub(crate) fn new(
        tenant: &TenantId,
        agent: &AgentId,
        publication: &Publication,
        now: u64,
    ) -> Result<Self, WorkerError> {
        validate(&publication.card)?;
        validate_timing(&publication.timing)?;
        let started_at = format_ts_ms(clock_ms(now)?)
            .map_err(|_| WorkerError::Config("start time is out of range"))?;
        let mut content = match serde_json::to_value(&publication.card) {
            Ok(Value::Object(map)) => map,
            _ => return Err(WorkerError::Config("card does not serialize to an object")),
        };
        content.remove("version");
        content.remove("signatures");
        Ok(Self {
            tenant: tenant.as_str().to_owned(),
            agent: agent.as_str().to_owned(),
            cards: Bucket::new(tenant, CARDS)
                .map_err(|_| WorkerError::Config("invalid card bucket name"))?,
            presence: Bucket::new(tenant, PRESENCE)
                .map_err(|_| WorkerError::Config("invalid presence bucket name"))?,
            key: Key::parse(agent.as_str())
                .map_err(|_| WorkerError::Config("agent id is not a valid bus key"))?,
            instance: TaskId::generate().as_str().to_owned(),
            started_at,
            timing: publication.timing,
            signing: publication.signing.clone(),
            declared_version: publication.card.version.clone(),
            content,
            published_version: None,
            card_due: 0,
            record: None,
            draining: false,
            drain_started: None,
            unhealthy_since: None,
            retry_at: None,
        })
    }

    /// Records the drain flag. Departure happens on the next tick (0004-016).
    pub(crate) fn set_draining(&mut self, draining: bool) {
        self.draining = draining;
        if !draining {
            self.drain_started = None;
        }
    }

    /// Brings the card and presence up to date.
    ///
    /// Returns audit notes. In non-strict mode a bus failure becomes a note and a retry
    /// instead of an error. Strict mode (open) returns the bus error. Non-bus errors always
    /// propagate.
    pub(crate) async fn tick<B: Bus>(
        &mut self,
        bus: &B,
        who: &Principal,
        now: u64,
        healthy: bool,
        active: usize,
        strict: bool,
    ) -> Result<Vec<String>, WorkerError> {
        if self.retry_at.is_some_and(|at| now < at) {
            return Ok(Vec::new());
        }
        self.retry_at = None;
        let mut notes = Vec::new();
        match self.step(bus, who, now, healthy, active, &mut notes).await {
            Ok(()) => Ok(notes),
            Err(WorkerError::Bus(err)) if !strict => {
                notes.push(format!("PRESENCE_WRITE_FAILED: {err}"));
                self.retry_at = Some(now.saturating_add(RETRY_AFTER_MS));
                Ok(notes)
            }
            Err(err) => Err(err),
        }
    }

    /// Earliest time the next tick is needed, or `None` when nothing is scheduled.
    pub(crate) fn next_wakeup(&self, now: u64, healthy: bool) -> Option<u64> {
        if let Some(at) = self.retry_at {
            return Some(at);
        }
        if self.published_version.is_none() {
            return Some(now);
        }
        let mut wake = self.card_due;
        match self.record {
            Some(live) => {
                if live.state != self.desired(healthy) {
                    return Some(now);
                }
                wake = wake.min(
                    live.written_at
                        .saturating_add(millis(self.timing.presence_refresh)),
                );
                if let Some(since) = self.drain_started {
                    wake = wake.min(since.saturating_add(millis(self.timing.drain_timeout)));
                }
                if let Some(since) = self.unhealthy_since {
                    if !self.stopped(now) {
                        wake = wake.min(since.saturating_add(millis(self.timing.health_recovery)));
                    }
                }
            }
            None => {
                if !self.draining && !self.stopped(now) {
                    return Some(now);
                }
            }
        }
        Some(wake)
    }

    async fn step<B: Bus>(
        &mut self,
        bus: &B,
        who: &Principal,
        now: u64,
        healthy: bool,
        active: usize,
        notes: &mut Vec<String>,
    ) -> Result<(), WorkerError> {
        self.track_health(now, healthy);
        if self.stopped(now) {
            if self.record.take().is_some() {
                notes.push(
                    "PRESENCE_STOPPED: unhealthy past the recovery window; record left to expire"
                        .to_owned(),
                );
            }
            return Ok(());
        }
        if self.draining {
            self.drain_started.get_or_insert(now);
        } else if self.record.is_none() {
            self.ensure_unclaimed(bus, who).await?;
        }
        if self.published_version.is_none() || now >= self.card_due {
            self.publish_card(bus, who, now).await?;
        }
        if self.draining {
            let Some(live) = self.record else {
                return Ok(());
            };
            let timed_out = self.drain_started.is_some_and(|since| {
                now.saturating_sub(since) >= millis(self.timing.drain_timeout)
            });
            if active == 0 || timed_out {
                return self.depart(bus, who, live, notes).await;
            }
            return self.sync(bus, who, now, State::Draining, active).await;
        }
        let desired = self.desired(healthy);
        self.sync(bus, who, now, desired, active).await
    }

    fn desired(&self, healthy: bool) -> State {
        if self.draining {
            State::Draining
        } else if healthy {
            State::Ready
        } else {
            State::Starting
        }
    }

    fn track_health(&mut self, now: u64, healthy: bool) {
        self.unhealthy_since = if healthy {
            None
        } else {
            Some(self.unhealthy_since.unwrap_or(now))
        };
    }

    fn stopped(&self, now: u64) -> bool {
        self.unhealthy_since
            .is_some_and(|since| now.saturating_sub(since) >= millis(self.timing.health_recovery))
    }

    /// Writes the card (0003-035, 0003-041). Keeps the card's version unless content or the
    /// declared version changes.
    async fn publish_card<B: Bus>(
        &mut self,
        bus: &B,
        who: &Principal,
        now: u64,
    ) -> Result<(), WorkerError> {
        let stored = match bus.kv_get(who, &self.cards, &self.key).await {
            Ok(entry) => Some(entry.value),
            Err(BusError::Absent) => None,
            Err(err) => return Err(err.into()),
        };
        let version = next_version(stored.as_deref(), &self.content, &self.declared_version)?;
        let mut card = self.content.clone();
        card.insert("version".to_owned(), Value::String(version.clone()));
        let mut value = Value::Object(card);
        if let CardSigning::Enabled { signer, trust } = &self.signing {
            sign_card(
                &mut value,
                signer.as_ref(),
                trust.as_ref(),
                &ManualClock::new(clock_ms(now)?),
            )
            .map_err(|_| WorkerError::Config("card signing failed"))?;
        }
        let bytes = serde_json::to_vec(&value)
            .map_err(|_| WorkerError::Invalid("card is not serializable"))?;
        if bytes.len() > MAX_CARD_BYTES {
            return Err(WorkerError::Invalid("card exceeds 262144 bytes"));
        }
        bus.kv_put(who, &self.cards, &self.key, Bytes::from(bytes), None)
            .await?;
        self.published_version = Some(version);
        self.card_due = now.saturating_add(millis(self.timing.card_refresh));
        Ok(())
    }

    /// Refuses before the card is rewritten when another live instance holds the agent id.
    async fn ensure_unclaimed<B: Bus>(&self, bus: &B, who: &Principal) -> Result<(), WorkerError> {
        match bus.kv_get(who, &self.presence, &self.key).await {
            Ok(entry) if !owns(&entry.value, &self.instance) => Err(WorkerError::Config(
                "agent id is served by another live instance",
            )),
            Ok(_) | Err(BusError::Absent) => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    /// Makes the record state `desired`, refreshing it when due. Re-claims the record once
    /// if it changed under us.
    async fn sync<B: Bus>(
        &mut self,
        bus: &B,
        who: &Principal,
        now: u64,
        desired: State,
        active: usize,
    ) -> Result<(), WorkerError> {
        let live = match self.record {
            Some(live) => live,
            None => self.claim(bus, who, now, active).await?,
        };
        let refresh_due =
            now.saturating_sub(live.written_at) >= millis(self.timing.presence_refresh);
        if live.state == desired && !refresh_due {
            return Ok(());
        }
        if self.write(bus, who, live, now, desired, active).await? {
            return Ok(());
        }
        let fresh = self.claim(bus, who, now, active).await?;
        if fresh.state != desired {
            self.write(bus, who, fresh, now, desired, active).await?;
        }
        Ok(())
    }

    /// Creates the record as STARTING, or takes over our own leftover record (0004-042).
    /// Refuses another live instance (0004-012, 0004-025).
    async fn claim<B: Bus>(
        &mut self,
        bus: &B,
        who: &Principal,
        now: u64,
        active: usize,
    ) -> Result<Live, WorkerError> {
        let ttl = Some(self.timing.presence_ttl);
        for _ in 0..CLAIM_ATTEMPTS {
            let body = self.presence_body(State::Starting, active)?;
            let claimed = match bus
                .kv_create(who, &self.presence, &self.key, body, ttl)
                .await
            {
                Ok(revision) => Some(revision),
                Err(BusError::Conflict { .. }) => self.adopt(bus, who, active).await?,
                Err(err) => return Err(err.into()),
            };
            if let Some(revision) = claimed {
                let live = Live {
                    state: State::Starting,
                    revision,
                    written_at: now,
                };
                self.record = Some(live);
                return Ok(live);
            }
        }
        Err(BusError::Unavailable {
            reason: "presence record kept changing during claim",
        }
        .into())
    }

    /// Takes over the existing record when it is ours. `None` means it changed or vanished.
    async fn adopt<B: Bus>(
        &self,
        bus: &B,
        who: &Principal,
        active: usize,
    ) -> Result<Option<Revision>, WorkerError> {
        let entry = match bus.kv_get(who, &self.presence, &self.key).await {
            Ok(entry) => entry,
            Err(BusError::Absent) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        if !owns(&entry.value, &self.instance) {
            return Err(WorkerError::Config(
                "agent id is served by another live instance",
            ));
        }
        let body = self.presence_body(State::Starting, active)?;
        match bus
            .kv_update(
                who,
                &self.presence,
                &self.key,
                body,
                entry.revision,
                Some(self.timing.presence_ttl),
            )
            .await
        {
            Ok(revision) => Ok(Some(revision)),
            Err(BusError::Conflict { .. } | BusError::Absent) => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    /// Compare-and-set write of `state`. Returns `false` when the record changed or expired
    /// under us, in which case the local record is dropped.
    async fn write<B: Bus>(
        &mut self,
        bus: &B,
        who: &Principal,
        live: Live,
        now: u64,
        state: State,
        active: usize,
    ) -> Result<bool, WorkerError> {
        let body = self.presence_body(state, active)?;
        match bus
            .kv_update(
                who,
                &self.presence,
                &self.key,
                body,
                live.revision,
                Some(self.timing.presence_ttl),
            )
            .await
        {
            Ok(revision) => {
                self.record = Some(Live {
                    state,
                    revision,
                    written_at: now,
                });
                Ok(true)
            }
            Err(BusError::Conflict { .. } | BusError::Absent) => {
                self.record = None;
                Ok(false)
            }
            Err(err) => Err(err.into()),
        }
    }

    /// Deletes the record on departure (0004-005, 0004-016). The card stays.
    async fn depart<B: Bus>(
        &mut self,
        bus: &B,
        who: &Principal,
        live: Live,
        notes: &mut Vec<String>,
    ) -> Result<(), WorkerError> {
        match bus
            .kv_delete(who, &self.presence, &self.key, Some(live.revision))
            .await
        {
            Ok(_) => notes.push("PRESENCE_DEPARTED".to_owned()),
            Err(BusError::Conflict { .. } | BusError::Absent) => {}
            Err(err) => return Err(err.into()),
        }
        self.record = None;
        Ok(())
    }

    fn presence_body(&self, state: State, active: usize) -> Result<Bytes, WorkerError> {
        let version = self
            .published_version
            .as_deref()
            .ok_or(WorkerError::Invalid(
                "card must be published before presence",
            ))?;
        let mut record = json!({
            "agentId": self.agent,
            "tenant": self.tenant,
            "instanceId": self.instance,
            "cardVersion": version,
            "state": state.wire(),
            "startedAt": self.started_at,
        });
        if active > 0 {
            record["activeTasks"] = json!(active);
        }
        let bytes = serde_json::to_vec(&record)
            .map_err(|_| WorkerError::Invalid("presence record is not serializable"))?;
        if bytes.len() > MAX_PRESENCE_BYTES {
            return Err(WorkerError::Invalid("presence record exceeds 1024 bytes"));
        }
        Ok(Bytes::from(bytes))
    }
}

fn require(ok: bool, reason: &'static str) -> Result<(), WorkerError> {
    if ok {
        Ok(())
    } else {
        Err(WorkerError::Config(reason))
    }
}

/// Checks the host card against RFC-0003 section 4.7 before anything is published.
fn validate(card: &AgentCard) -> Result<(), WorkerError> {
    require(!card.name.is_empty(), "card name must not be empty")?;
    require(
        !card.description.is_empty(),
        "card description must not be empty",
    )?;
    require(
        parse_semver(&card.version).is_some(),
        "card version must be MAJOR.MINOR.PATCH",
    )?;
    require(
        card.signatures.is_empty(),
        "host card must not carry signatures",
    )?;
    let interface = card
        .supported_interfaces
        .first()
        .ok_or(WorkerError::Config("card needs a bus interface"))?;
    require(
        interface.protocol_binding == BINDING,
        "first interface must use the a2abus bus binding",
    )?;
    require(
        !interface.url.is_empty() && !interface.url.contains('@'),
        "interface url must be non-empty and carry no credentials",
    )?;
    let capabilities = card
        .capabilities
        .as_ref()
        .ok_or(WorkerError::Config("card must declare capabilities"))?;
    require(
        capabilities.streaming == Some(true),
        "card must advertise streaming",
    )?;
    require(
        capabilities.push_notifications != Some(true),
        "push notifications are not supported on the bus",
    )?;
    require(
        capabilities.extended_agent_card != Some(true),
        "extended agent cards are not supported on the bus",
    )?;
    require(
        card.security_schemes.is_empty() && card.security_requirements.is_empty(),
        "card must not declare security schemes",
    )?;
    require(
        !card.default_input_modes.is_empty() && !card.default_output_modes.is_empty(),
        "card must declare input and output modes",
    )?;
    require(
        !card.skills.is_empty(),
        "card must declare at least one skill",
    )
}

fn validate_timing(timing: &Timing) -> Result<(), WorkerError> {
    let all = [
        timing.presence_ttl,
        timing.presence_refresh,
        timing.card_refresh,
        timing.drain_timeout,
        timing.health_recovery,
    ];
    require(
        all.iter().all(|d| !d.is_zero()),
        "timing values must be nonzero",
    )?;
    require(
        timing.presence_refresh < timing.presence_ttl,
        "presence refresh must be shorter than the presence TTL",
    )
}

/// Version a card must carry for `content`, given the stored card bytes (0003-035).
///
/// A higher declared version always wins. Otherwise unchanged content keeps the stored
/// version and changed content bumps the stored patch. An unreadable stored card is
/// replaced at the declared version.
fn next_version(
    stored: Option<&[u8]>,
    content: &Map<String, Value>,
    declared: &str,
) -> Result<String, WorkerError> {
    let Some(stored) = stored else {
        return Ok(declared.to_owned());
    };
    let Ok(Value::Object(mut previous)) = serde_json::from_slice::<Value>(stored) else {
        return Ok(declared.to_owned());
    };
    let Some(previous_version) = previous
        .remove("version")
        .and_then(|v| v.as_str().map(str::to_owned))
    else {
        return Ok(declared.to_owned());
    };
    previous.remove("signatures");
    let Some(old) = parse_semver(&previous_version) else {
        return Ok(declared.to_owned());
    };
    let new = parse_semver(declared).ok_or(WorkerError::Config(
        "card version must be MAJOR.MINOR.PATCH",
    ))?;
    if new > old {
        return Ok(declared.to_owned());
    }
    if previous == *content {
        return Ok(previous_version);
    }
    let (major, minor, patch) = old;
    let patch = patch
        .checked_add(1)
        .ok_or(WorkerError::Config("card version overflow"))?;
    Ok(format!("{major}.{minor}.{patch}"))
}

fn parse_semver(text: &str) -> Option<(u64, u64, u64)> {
    let mut parts = text.split('.');
    let major = parse_number(parts.next()?)?;
    let minor = parse_number(parts.next()?)?;
    let patch = parse_number(parts.next()?)?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Decimal digits without leading zeros.
fn parse_number(text: &str) -> Option<u64> {
    let canonical = !text.is_empty()
        && text.bytes().all(|b| b.is_ascii_digit())
        && (text == "0" || !text.starts_with('0'));
    if canonical { text.parse().ok() } else { None }
}

/// True when a presence record belongs to `instance`.
fn owns(value: &[u8], instance: &str) -> bool {
    serde_json::from_slice::<Value>(value)
        .ok()
        .is_some_and(|record| record["instanceId"].as_str() == Some(instance))
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn clock_ms(now: u64) -> Result<i64, WorkerError> {
    i64::try_from(now).map_err(|_| WorkerError::Config("clock is out of range"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::future::Future;
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    use a2a_security::card::verify_card;
    use a2a_security::identity::{Ed25519Signer, Signer, encode_public_key};
    use a2a_types::proto::a2a::{AgentCapabilities, AgentInterface, AgentSkill};
    use bus_api::control::BusControl;
    use bus_api::message::Entry;
    use bus_mem::{Grant, MemBus, MemConfig, m1_capabilities};

    use super::*;

    struct Fixture {
        bus: MemBus,
        who: Principal,
        tenant: TenantId,
        agent: AgentId,
    }

    fn fixture() -> Fixture {
        let tenant = TenantId::parse("local").unwrap();
        let agent = AgentId::parse("planner").unwrap();
        let who = Principal::named("planner").unwrap();
        let config = MemConfig {
            seed: 7,
            capabilities: m1_capabilities(),
            streams: Vec::new(),
            kv_buckets: vec![
                Bucket::new(&tenant, CARDS).unwrap(),
                Bucket::new(&tenant, PRESENCE).unwrap(),
            ],
            grants: vec![Grant {
                principal: who.clone(),
                tenant: tenant.clone(),
                publish: Vec::new(),
                serve: Vec::new(),
                streams: Vec::new(),
            }],
            ack_wait: Duration::from_secs(30),
            overload_retry_after: Duration::from_secs(1),
            blob_tenant_quota: 1 << 20,
        };
        Fixture {
            bus: MemBus::new(config).unwrap(),
            who,
            tenant,
            agent,
        }
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = pin!(future);
        let mut cx = Context::from_waker(Waker::noop());
        for _ in 0..10_000 {
            if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
                return out;
            }
        }
        panic!("bus future did not complete; in-memory bus futures must resolve immediately")
    }

    fn now(f: &Fixture) -> u64 {
        u64::try_from(f.bus.control().now().unwrap().as_millis()).unwrap()
    }

    fn advance_ms(f: &Fixture, ms: u64) {
        f.bus.control().advance(Duration::from_millis(ms)).unwrap();
    }

    fn card() -> AgentCard {
        AgentCard {
            name: "planner".into(),
            description: "plans work".into(),
            version: "1.0.0".into(),
            supported_interfaces: vec![AgentInterface {
                url: "bus://local/planner".into(),
                protocol_binding: BINDING.into(),
                tenant: "local".into(),
                protocol_version: "1.0".into(),
            }],
            capabilities: Some(AgentCapabilities {
                streaming: Some(true),
                push_notifications: Some(false),
                extended_agent_card: Some(false),
                ..Default::default()
            }),
            default_input_modes: vec!["text/plain".into()],
            default_output_modes: vec!["text/plain".into()],
            skills: vec![AgentSkill {
                id: "plan".into(),
                name: "plan".into(),
                description: "plans".into(),
                tags: vec!["plan".into()],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn publication() -> Publication {
        Publication {
            card: card(),
            signing: CardSigning::Disabled,
            timing: Timing::default(),
        }
    }

    fn publisher(f: &Fixture, publication: &Publication) -> Publisher {
        Publisher::new(&f.tenant, &f.agent, publication, now(f)).unwrap()
    }

    fn tick(f: &Fixture, p: &mut Publisher, healthy: bool, active: usize) -> Vec<String> {
        block_on(p.tick(&f.bus, &f.who, now(f), healthy, active, true)).unwrap()
    }

    fn entry(f: &Fixture, bucket: &str) -> Option<Entry> {
        let bucket = Bucket::new(&f.tenant, bucket).unwrap();
        let key = Key::parse(f.agent.as_str()).unwrap();
        match block_on(f.bus.kv_get(&f.who, &bucket, &key)) {
            Ok(entry) => Some(entry),
            Err(BusError::Absent) => None,
            Err(err) => panic!("kv_get failed: {err:?}"),
        }
    }

    fn stored(f: &Fixture, bucket: &str) -> Option<Value> {
        entry(f, bucket).map(|e| serde_json::from_slice(&e.value).unwrap())
    }

    fn trust_for(signer: &Ed25519Signer, agent: &str) -> TrustStore {
        let body = format!(
            "schema = \"a2abus.trust/1\"\n[[key]]\nkid = \"{}\"\nagent = \"{agent}\"\nrole = \"agent\"\npublic_key = \"{}\"\n",
            signer.kid(),
            encode_public_key(&signer.public_key()),
        );
        TrustStore::parse(&body).unwrap()
    }

    /// req_0004_013: presence is claimed as STARTING and then reaches READY with the card
    /// version and identity fields.
    #[test]
    fn req_0004_013_claims_starting_then_publishes_ready() {
        let f = fixture();
        let mut p = publisher(&f, &publication());
        assert!(tick(&f, &mut p, true, 0).is_empty());
        let record = stored(&f, "presence").unwrap();
        assert_eq!(record["state"], "PRESENCE_STATE_READY");
        assert_eq!(record["cardVersion"], "1.0.0");
        assert_eq!(record["agentId"], "planner");
        assert_eq!(record["tenant"], "local");
        assert!(record.get("activeTasks").is_none());
        assert_eq!(stored(&f, "cards").unwrap()["version"], "1.0.0");
    }

    /// req_0004_011: refresh is due at the interval, not before, and the next wakeup targets
    /// it.
    #[test]
    fn req_0004_011_refresh_waits_for_interval() {
        let f = fixture();
        let mut p = publisher(&f, &publication());
        tick(&f, &mut p, true, 0);
        let start = now(&f);
        let first = entry(&f, "presence").unwrap().revision;
        assert_eq!(p.next_wakeup(start, true), Some(start + 10_000));
        advance_ms(&f, 9_999);
        tick(&f, &mut p, true, 0);
        assert!(entry(&f, "presence").unwrap().revision == first);
        advance_ms(&f, 1);
        tick(&f, &mut p, true, 0);
        assert!(entry(&f, "presence").unwrap().revision != first);
    }

    /// req_0004_011: a record expires exactly at its TTL unless it was refreshed.
    #[test]
    fn req_0004_011_record_expires_at_ttl_boundary() {
        let f = fixture();
        let mut p = publisher(&f, &publication());
        tick(&f, &mut p, true, 0);
        advance_ms(&f, 10_000);
        tick(&f, &mut p, true, 0);
        advance_ms(&f, 29_999);
        assert!(entry(&f, "presence").is_some());
        advance_ms(&f, 1);
        assert!(entry(&f, "presence").is_none());
    }

    /// req_0004_016: draining publishes DRAINING while tasks run, deletes the presence
    /// record once idle, and keeps the card.
    #[test]
    fn req_0004_005_016_drain_publishes_draining_then_departs() {
        let f = fixture();
        let mut p = publisher(&f, &publication());
        tick(&f, &mut p, true, 2);
        p.set_draining(true);
        assert!(tick(&f, &mut p, true, 2).is_empty());
        assert_eq!(
            stored(&f, "presence").unwrap()["state"],
            "PRESENCE_STATE_DRAINING"
        );
        assert_eq!(stored(&f, "presence").unwrap()["activeTasks"], 2);
        let notes = tick(&f, &mut p, true, 0);
        assert_eq!(notes, vec!["PRESENCE_DEPARTED".to_owned()]);
        assert!(stored(&f, "presence").is_none());
        assert!(stored(&f, "cards").is_some());
    }

    /// req_0004_016: drain departs at the drain timeout even with tasks still active.
    #[test]
    fn req_0004_016_drain_timeout_forces_departure() {
        let f = fixture();
        let mut p = publisher(&f, &publication());
        tick(&f, &mut p, true, 2);
        p.set_draining(true);
        tick(&f, &mut p, true, 2);
        advance_ms(&f, 29_999);
        tick(&f, &mut p, true, 2);
        assert!(stored(&f, "presence").is_some());
        advance_ms(&f, 1);
        tick(&f, &mut p, true, 2);
        assert!(stored(&f, "presence").is_none());
    }

    /// req_0004_012: a second live instance under the same agent id is refused, and the
    /// card is left untouched.
    #[test]
    fn req_0004_012_025_duplicate_instance_refused() {
        let f = fixture();
        let mut first = publisher(&f, &publication());
        tick(&f, &mut first, true, 0);
        let mut second = publisher(&f, &publication());
        let result = block_on(second.tick(&f.bus, &f.who, now(&f), true, 0, true));
        assert!(matches!(result, Err(WorkerError::Config(_))));
        let record = stored(&f, "presence").unwrap();
        assert_eq!(record["instanceId"], first.instance.as_str());
    }

    /// req_0004_015: an unhealthy worker keeps refreshing until the recovery window
    /// passes, then stops, then re-claims after recovery.
    #[test]
    fn req_0004_015_unhealthy_stops_refresh_then_reclaims() {
        let f = fixture();
        let mut p = publisher(&f, &publication());
        tick(&f, &mut p, false, 0);
        assert_eq!(
            stored(&f, "presence").unwrap()["state"],
            "PRESENCE_STATE_STARTING"
        );
        for _ in 0..11 {
            advance_ms(&f, 10_000);
            tick(&f, &mut p, false, 0);
        }
        advance_ms(&f, 10_000);
        let notes = tick(&f, &mut p, false, 0);
        assert_eq!(notes.len(), 1);
        assert!(notes[0].starts_with("PRESENCE_STOPPED"));
        assert!(stored(&f, "presence").is_some());
        advance_ms(&f, 30_000);
        assert!(stored(&f, "presence").is_none());
        assert!(tick(&f, &mut p, true, 0).is_empty());
        assert_eq!(
            stored(&f, "presence").unwrap()["state"],
            "PRESENCE_STATE_READY"
        );
    }

    /// req_0004_041: a record deleted out from under us is recreated on the next refresh.
    #[test]
    fn req_0004_041_lost_record_is_reclaimed() {
        let f = fixture();
        let mut p = publisher(&f, &publication());
        tick(&f, &mut p, true, 0);
        let bucket = Bucket::new(&f.tenant, PRESENCE).unwrap();
        let key = Key::parse(f.agent.as_str()).unwrap();
        block_on(f.bus.kv_delete(&f.who, &bucket, &key, None)).unwrap();
        advance_ms(&f, 5_000);
        tick(&f, &mut p, true, 0);
        assert!(stored(&f, "presence").is_none());
        advance_ms(&f, 5_000);
        tick(&f, &mut p, true, 0);
        assert_eq!(
            stored(&f, "presence").unwrap()["state"],
            "PRESENCE_STATE_READY"
        );
    }

    /// req_0004_004: an invalid card is rejected before anything is published.
    #[test]
    fn req_0004_004_invalid_card_rejected() {
        let f = fixture();
        let mut bad = publication();
        bad.card.skills.clear();
        assert!(matches!(
            Publisher::new(&f.tenant, &f.agent, &bad, now(&f)),
            Err(WorkerError::Config(_))
        ));
        let mut signed = publication();
        signed.card.supported_interfaces[0].protocol_binding = "urn:other".into();
        assert!(matches!(
            Publisher::new(&f.tenant, &f.agent, &signed, now(&f)),
            Err(WorkerError::Config(_))
        ));
    }

    /// req_0003_035: the version is kept for unchanged content, bumped for changed content,
    /// and raised to a higher declared version.
    #[test]
    fn req_0003_035_version_bumps_only_when_content_changes() {
        let f = fixture();
        let at = now(&f);
        let mut base = publisher(&f, &publication());
        block_on(base.publish_card(&f.bus, &f.who, at)).unwrap();
        assert_eq!(stored(&f, "cards").unwrap()["version"], "1.0.0");

        let mut same = publisher(&f, &publication());
        block_on(same.publish_card(&f.bus, &f.who, at)).unwrap();
        assert_eq!(stored(&f, "cards").unwrap()["version"], "1.0.0");

        let mut changed = publication();
        changed.card.description = "plans harder".into();
        let mut edited = publisher(&f, &changed);
        block_on(edited.publish_card(&f.bus, &f.who, at)).unwrap();
        assert_eq!(stored(&f, "cards").unwrap()["version"], "1.0.1");

        let mut again = publisher(&f, &changed);
        block_on(again.publish_card(&f.bus, &f.who, at)).unwrap();
        assert_eq!(stored(&f, "cards").unwrap()["version"], "1.0.1");

        let mut declared = changed.clone();
        declared.card.version = "2.0.0".into();
        let mut major = publisher(&f, &declared);
        block_on(major.publish_card(&f.bus, &f.who, at)).unwrap();
        assert_eq!(stored(&f, "cards").unwrap()["version"], "2.0.0");
    }

    /// req_0004_010: a bus failure is audited and retried in non-strict mode, the presence
    /// record is not written without the card, and strict mode returns the error.
    #[test]
    fn req_0004_010_bus_failure_is_audited_and_retried() {
        let f = fixture();
        let stranger = Principal::named("stranger").unwrap();
        let at = now(&f);
        let mut p = publisher(&f, &publication());
        let notes = block_on(p.tick(&f.bus, &stranger, at, true, 0, false)).unwrap();
        assert_eq!(notes.len(), 1);
        assert!(notes[0].starts_with("PRESENCE_WRITE_FAILED"));
        assert!(stored(&f, "presence").is_none());
        assert_eq!(p.next_wakeup(at, true), Some(at + 1_000));
        let within = block_on(p.tick(&f.bus, &stranger, at + 500, true, 0, false)).unwrap();
        assert!(within.is_empty());

        let mut strict = publisher(&f, &publication());
        assert!(matches!(
            block_on(strict.tick(&f.bus, &stranger, at, true, 0, true)),
            Err(WorkerError::Bus(_))
        ));
    }

    /// req_0003_041: a signed card verifies against the trust store.
    #[test]
    fn req_0003_041_signed_card_verifies() {
        let f = fixture();
        let signer = std::sync::Arc::new(Ed25519Signer::from_seed(&f.agent, &[9; 32]));
        let trust = trust_for(&signer, "planner");
        let mut signed = publication();
        signed.signing = CardSigning::Enabled {
            signer: signer.clone(),
            trust: std::sync::Arc::new(trust),
        };
        let mut p = publisher(&f, &signed);
        tick(&f, &mut p, true, 0);
        let card = stored(&f, "cards").unwrap();
        assert!(card["signatures"].as_array().is_some_and(|s| !s.is_empty()));
        let verify_trust = trust_for(&signer, "planner");
        let clock = ManualClock::new(clock_ms(now(&f)).unwrap());
        let outcome = verify_card(&card, &f.agent, &verify_trust, &clock).unwrap();
        outcome.require_verified().unwrap();
    }

    /// req_0003_041: signing is opt-in; a disabled publication carries no signatures.
    #[test]
    fn req_0003_041_disabled_signing_publishes_unsigned_card() {
        let f = fixture();
        let mut p = publisher(&f, &publication());
        tick(&f, &mut p, true, 0);
        let card = stored(&f, "cards").unwrap();
        assert!(card.get("signatures").is_none());
    }
}
