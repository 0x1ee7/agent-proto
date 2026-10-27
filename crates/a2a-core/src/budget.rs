//! Budget allowance, metering, enforcement, lineage and loop control (RFC-0006 sections 4.6-4.8).
//!
//! Pure logic and deterministic: a [`Ledger`] tracks one hop (its grant, its usage and the reserve
//! held for children). Every check is a local comparison (0006-035), so no hop needs another hop's
//! clock or counters.

use std::collections::BTreeMap;

use a2a_types::failure::FailureCode;
use a2a_types::proto::a2abus::{Budget, BudgetUsage, Hop};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use unicode_normalization::UnicodeNormalization;

/// Unlimited sentinel. Valid for `max_tokens` and `max_cost_units` at the root and on delegated
/// grants (0006-032, 0015-055); never for wall time or the count dimensions.
pub const UNLIMITED: u64 = u64::MAX;
/// Margin subtracted from the parent's available wall time for each child (0006-033).
pub const WALL_MARGIN_MS: u64 = 1_000;
/// Default root wall allowance, matching RFC-0005 [0005-040] (0006-031).
pub const DEFAULT_MAX_WALL_MS: u64 = 3_600_000;
/// Default root hop allowance (0006-031).
pub const DEFAULT_MAX_HOPS: u32 = 8;
/// Default root depth allowance (0006-031).
pub const DEFAULT_MAX_DEPTH: u32 = 3;
/// Default root children allowance (0006-031).
pub const DEFAULT_MAX_CHILDREN: u32 = 8;
/// Failures from one origin that open the breaker (0006-048).
pub const BREAKER_THRESHOLD: usize = 3;
/// Window in which failures are counted (0006-048), milliseconds.
pub const BREAKER_WINDOW_MS: u64 = 600_000;
/// How long an open breaker stays open (0006-048), milliseconds.
pub const BREAKER_OPEN_MS: u64 = 600_000;

/// A budget dimension, named as in the wire detail (0006-040).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dimension {
    /// `max_tokens`.
    Tokens,
    /// `max_cost_units`.
    CostUnits,
    /// `max_wall_ms`.
    WallMs,
    /// `max_hops`.
    Hops,
    /// `max_depth`.
    Depth,
    /// `max_children`.
    Children,
}

impl Dimension {
    /// Wire name used in the `dimension` detail field.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Dimension::Tokens => "max_tokens",
            Dimension::CostUnits => "max_cost_units",
            Dimension::WallMs => "max_wall_ms",
            Dimension::Hops => "max_hops",
            Dimension::Depth => "max_depth",
            Dimension::Children => "max_children",
        }
    }
}

/// Errors in a budget value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum BudgetError {
    /// The unlimited sentinel appears where it is not allowed (0006-032).
    #[error("unlimited sentinel not allowed for {}", .0.name())]
    Sentinel(Dimension),
}

/// The allowance a hop receives. Values are remaining amounts (0006-030, 0006-032).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Allowance {
    /// Tokens this hop and its descendants may spend.
    pub max_tokens: u64,
    /// Integer cost units this hop and its descendants may spend.
    pub max_cost_units: u64,
    /// Relative wall time in milliseconds (0006-035).
    pub max_wall_ms: u64,
    /// Backend dispatches remaining for this hop.
    pub max_hops: u32,
    /// Delegation depth remaining below this hop.
    pub max_depth: u32,
    /// Children this hop may create.
    pub max_children: u32,
}

impl Default for Allowance {
    /// Configuration defaults from 0006-031 with no token or cost cap.
    fn default() -> Self {
        Self {
            max_tokens: UNLIMITED,
            max_cost_units: UNLIMITED,
            max_wall_ms: DEFAULT_MAX_WALL_MS,
            max_hops: DEFAULT_MAX_HOPS,
            max_depth: DEFAULT_MAX_DEPTH,
            max_children: DEFAULT_MAX_CHILDREN,
        }
    }
}

impl Allowance {
    /// Root allowance (0006-031): the component-wise minimum of the request budget and the
    /// configured values. Without a request budget the configured values are used.
    #[must_use]
    pub fn root(config: &Allowance, request: Option<&Allowance>) -> Allowance {
        match request {
            None => *config,
            Some(req) => Allowance {
                max_tokens: config.max_tokens.min(req.max_tokens),
                max_cost_units: config.max_cost_units.min(req.max_cost_units),
                max_wall_ms: config.max_wall_ms.min(req.max_wall_ms),
                max_hops: config.max_hops.min(req.max_hops),
                max_depth: config.max_depth.min(req.max_depth),
                max_children: config.max_children.min(req.max_children),
            },
        }
    }

    /// Checks sentinel placement (0006-032, 0015-055). The sentinel is valid for tokens and cost
    /// on the root hop and on delegated grants alike. Wall time is always finite, and the count
    /// dimensions never take it.
    pub fn validate(&self) -> Result<(), BudgetError> {
        if self.max_wall_ms == UNLIMITED {
            return Err(BudgetError::Sentinel(Dimension::WallMs));
        }
        for (dim, value) in [
            (Dimension::Hops, self.max_hops),
            (Dimension::Depth, self.max_depth),
            (Dimension::Children, self.max_children),
        ] {
            if value == u32::MAX {
                return Err(BudgetError::Sentinel(dim));
            }
        }
        Ok(())
    }

    /// Reads the allowance fields of a `Budget` message. The lineage in `chain` is separate.
    #[must_use]
    pub fn from_proto(budget: &Budget) -> Allowance {
        Allowance {
            max_tokens: budget.max_tokens,
            max_cost_units: budget.max_cost_units,
            max_wall_ms: budget.max_wall_ms,
            max_hops: budget.max_hops,
            max_depth: budget.max_depth,
            max_children: budget.max_children,
        }
    }

    /// Builds a `Budget` message from this allowance and a lineage chain (0006-043).
    #[must_use]
    pub fn to_proto(&self, chain: Vec<Hop>) -> Budget {
        Budget {
            max_tokens: self.max_tokens,
            max_cost_units: self.max_cost_units,
            max_wall_ms: self.max_wall_ms,
            max_hops: self.max_hops,
            max_depth: self.max_depth,
            max_children: self.max_children,
            chain,
        }
    }
}

/// Consumption of one hop (0006-041). Wall time and counts are the hop's own; the parent adds a
/// child's final usage when the child settles (0006-034).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Tokens spent.
    pub tokens: u64,
    /// Integer cost units spent.
    pub cost_units: u64,
    /// Wall time spent, milliseconds.
    pub wall_ms: u64,
    /// Backend dispatches made by this hop.
    pub hops: u32,
    /// Children created by this hop.
    pub children: u32,
}

impl Usage {
    /// Converts to the wire `BudgetUsage` with the hop's depth.
    #[must_use]
    pub fn to_proto(&self, depth: u32) -> BudgetUsage {
        BudgetUsage {
            tokens_used: self.tokens,
            cost_units_used: self.cost_units,
            wall_ms_used: self.wall_ms,
            hops_used: self.hops,
            children_used: self.children,
            depth,
        }
    }

    /// Reads the consumption fields of a `BudgetUsage`. The depth is not part of `Usage`.
    #[must_use]
    pub fn from_proto(usage: &BudgetUsage) -> Usage {
        Usage {
            tokens: usage.tokens_used,
            cost_units: usage.cost_units_used,
            wall_ms: usage.wall_ms_used,
            hops: usage.hops_used,
            children: usage.children_used,
        }
    }
}

/// Reserve held for live children (0006-034). Only finite dimensions are reserved.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reserve {
    /// Tokens reserved for children.
    pub tokens: u64,
    /// Cost units reserved for children.
    pub cost_units: u64,
    /// Wall milliseconds reserved for children.
    pub wall_ms: u64,
}

/// Which usage sources the backend driver can report (0006-037).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metering {
    /// Per-turn token counts are available (reported or self-reported).
    pub tokens: bool,
    /// Cumulative cost samples are available.
    pub cost: bool,
}

/// A budget trip (0006-040): the dimension, its limit and the amount consumed or reserved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("budget exhausted: {}", .dimension.name())]
pub struct BudgetTrip {
    /// Exhausted dimension.
    pub dimension: Dimension,
    /// Granted limit for the dimension.
    pub limit: u64,
    /// Amount consumed plus reserved.
    pub used: u64,
}

impl BudgetTrip {
    /// Failure detail with numbers and dimension names only (0006-040).
    #[must_use]
    pub fn detail(&self) -> serde_json::Value {
        serde_json::json!({
            "dimension": self.dimension.name(),
            "limit": self.limit.to_string(),
            "used": self.used.to_string(),
        })
    }

    /// The failure code this trip commits (0006-040).
    #[must_use]
    pub const fn code(&self) -> FailureCode {
        FailureCode::BudgetExhausted
    }
}

/// Why a delegation was refused (0006-033). The hop continues without the child.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum DelegationRefused {
    /// `max_depth` is 0.
    #[error("depth allowance exhausted")]
    Depth,
    /// No hop remains for the delegation.
    #[error("hop allowance exhausted")]
    Hops,
    /// `children_used` reached `max_children`.
    #[error("children allowance exhausted")]
    Children,
}

/// Result of a cumulative cost sample (0006-037).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostSample {
    /// Cost is not metered for this hop; nothing is charged.
    Unmetered,
    /// The sample exceeded the high-water mark; the difference was charged.
    Charged(u64),
    /// The sample equals the high-water mark; nothing to charge.
    NoChange,
    /// The sample is below the high-water mark: it is treated as a fault and charges nothing.
    Decreased,
}

/// Accounting state of one hop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ledger {
    grant: Allowance,
    depth: u32,
    used: Usage,
    reserved: Reserve,
    metering: Metering,
    cost_high_water: u64,
    cost_faults: u32,
}

impl Ledger {
    /// Ledger of the root hop (depth 0).
    pub fn root(grant: Allowance, metering: Metering) -> Result<Ledger, BudgetError> {
        grant.validate()?;
        Ok(Self::new(grant, 0, metering))
    }

    /// Ledger of a child hop at `depth`.
    pub fn child(grant: Allowance, depth: u32, metering: Metering) -> Result<Ledger, BudgetError> {
        // Delegated grants keep the unlimited sentinel on tokens and cost (0015-055).
        grant.validate()?;
        Ok(Self::new(grant, depth, metering))
    }

    fn new(grant: Allowance, depth: u32, metering: Metering) -> Ledger {
        Ledger {
            grant,
            depth,
            used: Usage::default(),
            reserved: Reserve::default(),
            metering,
            cost_high_water: 0,
            cost_faults: 0,
        }
    }

    /// The allowance this hop received.
    #[must_use]
    pub const fn grant(&self) -> &Allowance {
        &self.grant
    }

    /// Consumption so far.
    #[must_use]
    pub const fn usage(&self) -> &Usage {
        &self.used
    }

    /// Depth from the root (root = 0, 0006-036).
    #[must_use]
    pub const fn depth(&self) -> u32 {
        self.depth
    }

    /// Reserve held for live children.
    #[must_use]
    pub const fn reserved(&self) -> &Reserve {
        &self.reserved
    }

    /// Number of cost samples that decreased (faults, 0006-037).
    #[must_use]
    pub const fn cost_faults(&self) -> u32 {
        self.cost_faults
    }

    /// `BudgetUsage` for the terminal event metadata (0006-041).
    #[must_use]
    pub fn usage_proto(&self) -> BudgetUsage {
        self.used.to_proto(self.depth)
    }

    /// Remaining amount for a dimension. For tokens and cost on an unlimited grant the result is
    /// the distance to the sentinel and is never reserved against.
    #[must_use]
    pub fn remaining(&self, dim: Dimension) -> u64 {
        match dim {
            Dimension::Tokens => finite_remaining(
                self.grant.max_tokens,
                self.used.tokens,
                self.reserved.tokens,
            ),
            Dimension::CostUnits => finite_remaining(
                self.grant.max_cost_units,
                self.used.cost_units,
                self.reserved.cost_units,
            ),
            Dimension::WallMs => wall_remaining(
                self.grant.max_wall_ms,
                self.used.wall_ms,
                self.reserved.wall_ms,
            ),
            Dimension::Hops => u64::from(self.grant.max_hops.saturating_sub(self.used.hops)),
            Dimension::Depth => u64::from(self.grant.max_depth),
            Dimension::Children => {
                u64::from(self.grant.max_children.saturating_sub(self.used.children))
            }
        }
    }

    /// Checks the limits before a backend turn and counts the dispatch (0006-036, 0006-039).
    /// Tokens and cost are enforced only when metered (0006-037); wall time and hops always.
    pub fn start_turn(&mut self) -> Result<(), BudgetTrip> {
        if self.used.hops >= self.grant.max_hops {
            return Err(self.trip(Dimension::Hops));
        }
        if self.metering.tokens {
            self.require_remaining(Dimension::Tokens)?;
        }
        if self.metering.cost {
            self.require_remaining(Dimension::CostUnits)?;
        }
        self.require_remaining(Dimension::WallMs)?;
        self.used.hops = self.used.hops.saturating_add(1);
        Ok(())
    }

    /// Charges per-turn tokens (0006-037). Returns false and charges nothing when tokens are
    /// unmetered, so the audit record can mark the dimension `unmetered`.
    pub fn charge_tokens(&mut self, tokens: u64) -> bool {
        if !self.metering.tokens {
            return false;
        }
        self.used.tokens = self.used.tokens.saturating_add(tokens);
        true
    }

    /// Charges the increase over the highest cost sample already charged (0006-037). The sample
    /// is cumulative; a decrease is recorded as a fault and charges nothing.
    pub fn observe_cost(&mut self, cumulative: u64) -> CostSample {
        if !self.metering.cost {
            return CostSample::Unmetered;
        }
        match cumulative.cmp(&self.cost_high_water) {
            std::cmp::Ordering::Greater => {
                let charged = cumulative - self.cost_high_water;
                self.cost_high_water = cumulative;
                self.used.cost_units = self.used.cost_units.saturating_add(charged);
                CostSample::Charged(charged)
            }
            std::cmp::Ordering::Equal => CostSample::NoChange,
            std::cmp::Ordering::Less => {
                self.cost_faults = self.cost_faults.saturating_add(1);
                CostSample::Decreased
            }
        }
    }

    /// Hard stop after a usage report (0006-039): reports the first exhausted enforced dimension.
    #[must_use]
    pub fn check_after_usage(&self) -> Option<BudgetTrip> {
        if self.metering.tokens && self.remaining(Dimension::Tokens) == 0 {
            return Some(self.trip(Dimension::Tokens));
        }
        if self.metering.cost && self.remaining(Dimension::CostUnits) == 0 {
            return Some(self.trip(Dimension::CostUnits));
        }
        if self.remaining(Dimension::WallMs) == 0 {
            return Some(self.trip(Dimension::WallMs));
        }
        None
    }

    /// Creates a child allowance and reserves it (0006-033, 0006-034). Returns the child's
    /// allowance; the caller builds the child ledger from it with [`Ledger::child`].
    pub fn delegate(&mut self) -> Result<Allowance, DelegationRefused> {
        if self.grant.max_depth < 1 {
            return Err(DelegationRefused::Depth);
        }
        if self.remaining(Dimension::Hops) < 1 {
            return Err(DelegationRefused::Hops);
        }
        if self.used.children >= self.grant.max_children {
            return Err(DelegationRefused::Children);
        }
        let child = Allowance {
            max_tokens: self.child_share(Dimension::Tokens),
            max_cost_units: self.child_share(Dimension::CostUnits),
            max_wall_ms: self
                .remaining(Dimension::WallMs)
                .saturating_sub(WALL_MARGIN_MS),
            max_hops: self.grant.max_hops - self.used.hops - 1,
            max_depth: self.grant.max_depth - 1,
            max_children: self.grant.max_children - 1,
        };
        if self.grant.max_tokens != UNLIMITED {
            self.reserved.tokens = self.reserved.tokens.saturating_add(child.max_tokens);
        }
        if self.grant.max_cost_units != UNLIMITED {
            self.reserved.cost_units = self
                .reserved
                .cost_units
                .saturating_add(child.max_cost_units);
        }
        self.reserved.wall_ms = self.reserved.wall_ms.saturating_add(child.max_wall_ms);
        self.used.children = self.used.children.saturating_add(1);
        Ok(child)
    }

    /// Settles a terminal child (0006-034): releases its reserve and charges its final usage.
    pub fn settle_child(&mut self, child_grant: &Allowance, child_usage: &Usage) {
        if self.grant.max_tokens != UNLIMITED {
            self.reserved.tokens = self.reserved.tokens.saturating_sub(child_grant.max_tokens);
        }
        if self.grant.max_cost_units != UNLIMITED {
            self.reserved.cost_units = self
                .reserved
                .cost_units
                .saturating_sub(child_grant.max_cost_units);
        }
        self.reserved.wall_ms = self
            .reserved
            .wall_ms
            .saturating_sub(child_grant.max_wall_ms);
        self.used.tokens = self.used.tokens.saturating_add(child_usage.tokens);
        self.used.cost_units = self.used.cost_units.saturating_add(child_usage.cost_units);
        self.used.wall_ms = self.used.wall_ms.saturating_add(child_usage.wall_ms);
    }

    /// Child share of a token or cost dimension (0015-055): the available amount, or the
    /// sentinel itself when the parent is unlimited, so the child keeps the sentinel.
    fn child_share(&self, dim: Dimension) -> u64 {
        let unlimited = match dim {
            Dimension::Tokens => self.grant.max_tokens == UNLIMITED,
            _ => self.grant.max_cost_units == UNLIMITED,
        };
        if unlimited {
            UNLIMITED
        } else {
            self.remaining(dim)
        }
    }

    fn require_remaining(&self, dim: Dimension) -> Result<(), BudgetTrip> {
        if self.remaining(dim) == 0 {
            return Err(self.trip(dim));
        }
        Ok(())
    }

    fn trip(&self, dim: Dimension) -> BudgetTrip {
        let (limit, used) = match dim {
            Dimension::Tokens => (
                self.grant.max_tokens,
                self.grant.max_tokens - self.remaining(dim),
            ),
            Dimension::CostUnits => (
                self.grant.max_cost_units,
                self.grant.max_cost_units - self.remaining(dim),
            ),
            Dimension::WallMs => (
                self.grant.max_wall_ms,
                self.grant.max_wall_ms - self.remaining(dim),
            ),
            Dimension::Hops => (u64::from(self.grant.max_hops), u64::from(self.used.hops)),
            Dimension::Depth => (u64::from(self.grant.max_depth), 0),
            Dimension::Children => (
                u64::from(self.grant.max_children),
                u64::from(self.used.children),
            ),
        };
        BudgetTrip {
            dimension: dim,
            limit,
            used,
        }
    }
}

fn finite_remaining(grant: u64, used: u64, reserved: u64) -> u64 {
    if grant == UNLIMITED {
        return UNLIMITED.saturating_sub(used);
    }
    grant.saturating_sub(used).saturating_sub(reserved)
}

/// Remaining wall time (0006-032). Wall time is always finite: a sentinel grant can only come
/// from state that skipped validation, and it fails closed (no time) rather than unlimited.
fn wall_remaining(grant: u64, used: u64, reserved: u64) -> u64 {
    if grant == UNLIMITED {
        return 0;
    }
    finite_remaining(grant, used, reserved)
}

/// Lowercase hex of `bytes`.
fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Collapses each run of whitespace to one ASCII space (0006-044).
fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_run = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !in_run {
                out.push(' ');
            }
            in_run = true;
        } else {
            out.push(ch);
            in_run = false;
        }
    }
    out
}

/// Message fingerprint (0006-044): lowercase hex of the first 16 bytes of SHA-256 over
/// `<target agent id> LF <normalized text>`. Each text part is NFC-normalized, the parts are
/// concatenated, and whitespace runs collapse to one space. Only text parts count.
#[must_use]
pub fn fingerprint<'a>(
    target_agent: &str,
    text_parts: impl IntoIterator<Item = &'a str>,
) -> String {
    let mut normalized = String::new();
    for part in text_parts {
        normalized.extend(part.nfc());
    }
    let collapsed = collapse_whitespace(&normalized);
    let mut hasher = Sha256::new();
    hasher.update(target_agent.as_bytes());
    hasher.update(b"\n");
    hasher.update(collapsed.as_bytes());
    let digest = hasher.finalize();
    hex_lower(&digest[..16])
}

/// Lineage chain of a root task: one hop for the root owner (0006-043).
#[must_use]
pub fn root_chain(owner: &str, fingerprint: &str) -> Vec<Hop> {
    vec![Hop {
        agent_id: owner.to_owned(),
        fingerprint: fingerprint.to_owned(),
    }]
}

/// True when `(receiving, fingerprint)` already appears in the parent's chain (0006-045).
#[must_use]
pub fn chain_has_loop(parent_chain: &[Hop], receiving: &str, fingerprint: &str) -> bool {
    parent_chain
        .iter()
        .any(|hop| hop.agent_id == receiving && hop.fingerprint == fingerprint)
}

/// Ancestors of the receiving agent: the chain without its final hop, which names the
/// receiving agent itself (0006-043). `None` when the chain is empty or does not end with
/// `own_agent`.
#[must_use]
pub fn chain_before_self<'a>(chain: &'a [Hop], own_agent: &str) -> Option<&'a [Hop]> {
    let (last, ancestors) = chain.split_last()?;
    (last.agent_id == own_agent).then_some(ancestors)
}

/// True when `own_agent` already appears among the ancestors of the receiving agent (0006-059).
#[must_use]
pub fn is_reentrant(ancestors: &[Hop], own_agent: &str) -> bool {
    ancestors.iter().any(|hop| hop.agent_id == own_agent)
}

/// The child's chain: the parent's chain with one appended hop (0006-043). Never shortened.
#[must_use]
pub fn extend_chain(parent_chain: &[Hop], receiving: &str, fingerprint: &str) -> Vec<Hop> {
    let mut chain = Vec::with_capacity(parent_chain.len() + 1);
    chain.extend_from_slice(parent_chain);
    chain.push(Hop {
        agent_id: receiving.to_owned(),
        fingerprint: fingerprint.to_owned(),
    });
    chain
}

/// Breaker status of one origin (0006-048).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerStatus {
    /// Submissions are accepted.
    Closed,
    /// Submissions are refused with `RATE_LIMITED` until `until_ms`.
    Open {
        /// Local time at which the breaker closes again.
        until_ms: u64,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct OriginBreaker {
    failures_ms: Vec<u64>,
    open_until_ms: Option<u64>,
}

/// Per-origin breaker (0006-048). The state is serializable so the owning store can persist it
/// (RFC-0005 [0005-031]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Breakers {
    origins: BTreeMap<String, OriginBreaker>,
}

impl Breakers {
    /// Status of the breaker for `origin` at local time `now_ms`.
    #[must_use]
    pub fn status(&self, origin: &str, now_ms: u64) -> BreakerStatus {
        match self.origins.get(origin).and_then(|b| b.open_until_ms) {
            Some(until_ms) if until_ms > now_ms => BreakerStatus::Open { until_ms },
            _ => BreakerStatus::Closed,
        }
    }

    /// Records a task that ended FAILED. Only `BUDGET_EXHAUSTED` and `LOOP_DETECTED` count.
    /// Returns true when this failure opened the breaker.
    pub fn record_failure(&mut self, origin: &str, code: FailureCode, now_ms: u64) -> bool {
        if !matches!(
            code,
            FailureCode::BudgetExhausted | FailureCode::LoopDetected
        ) {
            return false;
        }
        let entry = self.origins.entry(origin.to_owned()).or_default();
        entry
            .failures_ms
            .retain(|t| now_ms.saturating_sub(*t) < BREAKER_WINDOW_MS);
        entry.failures_ms.push(now_ms);
        if entry.failures_ms.len() >= BREAKER_THRESHOLD {
            entry.failures_ms.clear();
            entry.open_until_ms = Some(now_ms.saturating_add(BREAKER_OPEN_MS));
            return true;
        }
        false
    }

    /// Records a successful task, which closes the breaker early (0006-048).
    pub fn record_success(&mut self, origin: &str) {
        if let Some(entry) = self.origins.get_mut(origin) {
            entry.open_until_ms = None;
            entry.failures_ms.clear();
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn grant() -> Allowance {
        Allowance {
            max_tokens: 1_000,
            max_cost_units: 500,
            max_wall_ms: 600_000,
            max_hops: 8,
            max_depth: 3,
            max_children: 8,
        }
    }

    fn metered() -> Metering {
        Metering {
            tokens: true,
            cost: true,
        }
    }

    #[test]
    fn req_0006_031_root_is_component_min_of_request_and_config() {
        let config = Allowance::default();
        let request = Allowance {
            max_tokens: 50,
            max_hops: 20,
            max_depth: 1,
            ..grant()
        };
        let root = Allowance::root(&config, Some(&request));
        assert_eq!(root.max_tokens, 50);
        assert_eq!(root.max_hops, DEFAULT_MAX_HOPS);
        assert_eq!(root.max_depth, 1);
        assert_eq!(root.max_wall_ms, 600_000);
        assert_eq!(Allowance::root(&config, None), config);
    }

    #[test]
    fn req_0006_031_defaults_match_rfc() {
        let d = Allowance::default();
        assert_eq!(d.max_wall_ms, 3_600_000);
        assert_eq!((d.max_hops, d.max_depth, d.max_children), (8, 3, 8));
        assert_eq!((d.max_tokens, d.max_cost_units), (UNLIMITED, UNLIMITED));
    }

    /// req_0006_032: the sentinel is valid for tokens and cost only, on the root and on
    /// delegated grants alike; wall time and the count dimensions never take it.
    #[test]
    fn req_0006_032_sentinel_on_tokens_and_cost_only() {
        let open = Allowance {
            max_tokens: UNLIMITED,
            max_cost_units: UNLIMITED,
            ..grant()
        };
        assert!(open.validate().is_ok());
        let wall = Allowance {
            max_wall_ms: UNLIMITED,
            ..grant()
        };
        assert_eq!(
            wall.validate(),
            Err(BudgetError::Sentinel(Dimension::WallMs))
        );
        let hops = Allowance {
            max_hops: u32::MAX,
            ..grant()
        };
        assert_eq!(hops.validate(), Err(BudgetError::Sentinel(Dimension::Hops)));
        let children = Allowance {
            max_children: u32::MAX,
            ..grant()
        };
        assert_eq!(
            children.validate(),
            Err(BudgetError::Sentinel(Dimension::Children))
        );
    }

    #[test]
    fn req_0006_032_zero_means_none_remaining() {
        let zero = Allowance {
            max_tokens: 0,
            ..grant()
        };
        let mut ledger = Ledger::child(zero, 1, metered()).expect("valid");
        let trip = ledger
            .start_turn()
            .expect_err("zero tokens refuse the turn");
        assert_eq!(trip.dimension, Dimension::Tokens);
        assert_eq!(trip.limit, 0);
    }

    #[test]
    fn req_0015_055_delegated_grant_keeps_unlimited_tokens_and_cost() {
        let delegated = Allowance {
            max_tokens: UNLIMITED,
            max_cost_units: UNLIMITED,
            ..grant()
        };
        let ledger =
            Ledger::child(delegated, 1, metered()).expect("sentinel kept on a delegated grant");
        assert_eq!(ledger.depth(), 1);
        let hops = Allowance {
            max_hops: u32::MAX,
            ..grant()
        };
        assert!(
            Ledger::child(hops, 1, metered()).is_err(),
            "count dimensions never take the sentinel"
        );
        let wall = Allowance {
            max_wall_ms: UNLIMITED,
            ..grant()
        };
        assert!(
            Ledger::child(wall, 1, metered()).is_err(),
            "wall time is always finite"
        );
    }

    #[test]
    fn req_0015_055_unlimited_parent_hands_child_the_sentinel() {
        let parent = Allowance {
            max_tokens: UNLIMITED,
            max_cost_units: UNLIMITED,
            ..grant()
        };
        let mut ledger = Ledger::root(parent, metered()).expect("valid");
        let child = ledger.delegate().expect("delegation allowed");
        assert_eq!(child.max_tokens, UNLIMITED);
        assert_eq!(child.max_cost_units, UNLIMITED);
    }

    #[test]
    fn req_0006_032_wall_sentinel_grants_no_time() {
        let ledger = Ledger::root(grant(), metered()).expect("valid");
        let mut value = serde_json::to_value(&ledger).expect("serialize");
        value["grant"]["max_wall_ms"] = serde_json::json!(UNLIMITED);
        let mut ledger: Ledger = serde_json::from_value(value).expect("deserialize");
        assert_eq!(ledger.remaining(Dimension::WallMs), 0);
        let trip = ledger.start_turn().expect_err("no wall time to run a turn");
        assert_eq!(trip.dimension, Dimension::WallMs);
    }

    #[test]
    fn req_0006_033_child_allowance_matches_formulas() {
        let mut parent = Ledger::root(grant(), metered()).expect("valid");
        parent.start_turn().expect("turn");
        parent.start_turn().expect("turn");
        let child = parent.delegate().expect("delegation allowed");
        assert_eq!(child.max_hops, 8 - 2 - 1);
        assert_eq!(child.max_depth, 2);
        assert_eq!(child.max_children, 7);
        assert_eq!(child.max_tokens, 1_000);
        assert_eq!(child.max_cost_units, 500);
        assert_eq!(child.max_wall_ms, 600_000 - WALL_MARGIN_MS);
        assert_eq!(parent.usage().children, 1);
    }

    #[test]
    fn req_0006_033_delegation_refused_at_each_limit() {
        let no_depth = Allowance {
            max_depth: 0,
            ..grant()
        };
        let mut ledger = Ledger::root(no_depth, metered()).expect("valid");
        assert_eq!(ledger.delegate(), Err(DelegationRefused::Depth));

        let one_hop = Allowance {
            max_hops: 1,
            ..grant()
        };
        let mut ledger = Ledger::root(one_hop, metered()).expect("valid");
        ledger.start_turn().expect("turn");
        assert_eq!(ledger.delegate(), Err(DelegationRefused::Hops));

        let no_children = Allowance {
            max_children: 0,
            ..grant()
        };
        let mut ledger = Ledger::root(no_children, metered()).expect("valid");
        assert_eq!(ledger.delegate(), Err(DelegationRefused::Children));
    }

    #[test]
    fn req_0006_034_reserve_limits_siblings_and_settle_returns_unused() {
        let mut parent = Ledger::root(grant(), metered()).expect("valid");
        let a = parent.delegate().expect("first child");
        assert_eq!(a.max_tokens, 1_000);
        assert_eq!(parent.remaining(Dimension::Tokens), 0);
        let b = parent
            .delegate()
            .expect("second child is allowed with zero share");
        assert_eq!(b.max_tokens, 0, "parent has no tokens left to reserve");

        parent.settle_child(
            &a,
            &Usage {
                tokens: 200,
                ..Usage::default()
            },
        );
        assert_eq!(parent.usage().tokens, 200);
        assert_eq!(parent.remaining(Dimension::Tokens), 800);
        assert_eq!(parent.reserved().tokens, 0);
    }

    #[test]
    fn req_0006_034_settle_does_not_double_count_reserve() {
        let mut parent = Ledger::root(grant(), metered()).expect("valid");
        let child = parent.delegate().expect("child");
        parent.settle_child(&child, &Usage::default());
        assert_eq!(parent.reserved().tokens, 0);
        assert_eq!(parent.reserved().wall_ms, 0);
        assert_eq!(parent.remaining(Dimension::Tokens), 1_000);
    }

    #[test]
    fn req_0006_034_monotonic_no_child_allowance_exceeds_parent() {
        let mut parent = Ledger::root(grant(), metered()).expect("valid");
        parent.charge_tokens(300);
        let child = parent.delegate().expect("child");
        assert!(child.max_tokens <= 700);
        assert!(child.max_wall_ms <= 600_000);
        assert!(child.max_hops < parent.grant().max_hops);
        assert!(child.max_depth < parent.grant().max_depth);
        assert!(child.max_children < parent.grant().max_children);
    }

    #[test]
    fn req_0006_035_wall_time_is_relative_and_margin_applied() {
        let mut parent = Ledger::root(
            Allowance {
                max_wall_ms: 1_500,
                ..grant()
            },
            metered(),
        )
        .expect("valid");
        let child = parent.delegate().expect("child");
        assert_eq!(child.max_wall_ms, 500);
        let tight = Allowance {
            max_wall_ms: 400,
            ..grant()
        };
        let mut parent = Ledger::root(tight, metered()).expect("valid");
        assert_eq!(parent.delegate().expect("child").max_wall_ms, 0);
    }

    #[test]
    fn req_0006_036_hops_counted_before_each_turn() {
        let mut ledger = Ledger::root(
            Allowance {
                max_hops: 2,
                ..grant()
            },
            metered(),
        )
        .expect("valid");
        ledger.start_turn().expect("turn 1");
        assert_eq!(ledger.usage().hops, 1);
        ledger.start_turn().expect("turn 2");
        let trip = ledger.start_turn().expect_err("turn 3 exceeds max_hops");
        assert_eq!(trip.dimension, Dimension::Hops);
        assert_eq!(trip.limit, 2);
        assert_eq!(ledger.usage().hops, 2);
    }

    #[test]
    fn req_0006_036_depth_is_hop_depth_from_root() {
        let root = Ledger::root(grant(), metered()).expect("valid");
        assert_eq!(root.depth(), 0);
        let child = Ledger::child(grant(), 2, metered()).expect("valid");
        assert_eq!(child.usage_proto().depth, 2);
    }

    #[test]
    fn req_0006_037_unmetered_tokens_not_advanced() {
        let mut ledger = Ledger::root(
            grant(),
            Metering {
                tokens: false,
                cost: true,
            },
        )
        .expect("valid");
        assert!(!ledger.charge_tokens(40));
        assert_eq!(ledger.usage().tokens, 0);
        assert!(!ledger.charge_tokens(0));
    }

    #[test]
    fn req_0006_037_cost_charges_increase_over_high_water() {
        let mut ledger = Ledger::root(grant(), metered()).expect("valid");
        assert_eq!(ledger.observe_cost(10), CostSample::Charged(10));
        assert_eq!(ledger.observe_cost(10), CostSample::NoChange);
        assert_eq!(ledger.observe_cost(25), CostSample::Charged(15));
        assert_eq!(ledger.usage().cost_units, 25);
    }

    #[test]
    fn req_0006_037_decreasing_cost_sample_charges_nothing_and_records_fault() {
        let mut ledger = Ledger::root(grant(), metered()).expect("valid");
        ledger.observe_cost(30);
        assert_eq!(ledger.observe_cost(20), CostSample::Decreased);
        assert_eq!(ledger.usage().cost_units, 30);
        assert_eq!(ledger.cost_faults(), 1);
        assert_eq!(ledger.observe_cost(35), CostSample::Charged(5));
    }

    #[test]
    fn req_0006_037_unmetered_cost_is_not_charged() {
        let mut ledger = Ledger::root(
            grant(),
            Metering {
                tokens: true,
                cost: false,
            },
        )
        .expect("valid");
        assert_eq!(ledger.observe_cost(99), CostSample::Unmetered);
        assert_eq!(ledger.usage().cost_units, 0);
    }

    #[test]
    fn req_0006_038_usage_sample_is_never_summed_as_spend() {
        // Context occupancy (ACP usage_update.used) is not a token count: the API has no way to
        // charge it, so repeated occupancy reports leave tokens_used at zero.
        let mut ledger = Ledger::root(grant(), metered()).expect("valid");
        assert_eq!(ledger.usage().tokens, 0);
        ledger.charge_tokens(0);
        ledger.charge_tokens(0);
        assert_eq!(ledger.usage().tokens, 0);
    }

    #[test]
    fn req_0006_039_check_after_usage_trips_on_exhausted_dimension() {
        let mut ledger = Ledger::root(
            Allowance {
                max_tokens: 100,
                ..grant()
            },
            metered(),
        )
        .expect("valid");
        ledger.charge_tokens(99);
        assert_eq!(ledger.check_after_usage(), None);
        ledger.charge_tokens(1);
        let trip = ledger.check_after_usage().expect("tokens exhausted");
        assert_eq!(trip.dimension, Dimension::Tokens);
        assert_eq!((trip.limit, trip.used), (100, 100));
    }

    #[test]
    fn req_0006_039_unmetered_tokens_do_not_trip() {
        let mut ledger = Ledger::root(
            Allowance {
                max_tokens: 0,
                ..grant()
            },
            Metering {
                tokens: false,
                cost: false,
            },
        )
        .expect("valid");
        assert_eq!(ledger.check_after_usage(), None);
        assert!(ledger.start_turn().is_ok());
    }

    #[test]
    fn req_0006_040_trip_detail_has_numbers_and_names_only() {
        let trip = BudgetTrip {
            dimension: Dimension::WallMs,
            limit: 600_000,
            used: 600_004,
        };
        let detail = trip.detail();
        assert_eq!(detail["dimension"], "max_wall_ms");
        assert_eq!(detail["limit"], "600000");
        assert_eq!(detail["used"], "600004");
        assert_eq!(trip.code(), FailureCode::BudgetExhausted);
        assert_eq!(detail.as_object().map(serde_json::Map::len), Some(3));
    }

    #[test]
    fn req_0006_041_usage_proto_round_trips() {
        let mut ledger = Ledger::root(grant(), metered()).expect("valid");
        ledger.charge_tokens(7);
        ledger.observe_cost(3);
        ledger.start_turn().expect("turn");
        let proto = ledger.usage_proto();
        assert_eq!(proto.tokens_used, 7);
        assert_eq!(proto.cost_units_used, 3);
        assert_eq!(proto.hops_used, 1);
        assert_eq!(Usage::from_proto(&proto), *ledger.usage());
    }

    #[test]
    fn req_0006_030_budget_proto_round_trips_allowance_and_chain() {
        let chain = root_chain("claude", "0123456789abcdef0123456789abcdef");
        let proto = grant().to_proto(chain.clone());
        assert_eq!(Allowance::from_proto(&proto), grant());
        assert_eq!(proto.chain, chain);
    }

    #[test]
    fn req_0006_043_root_chain_has_one_hop_and_extension_appends() {
        let root = root_chain("claude", "aa");
        assert_eq!(root.len(), 1);
        assert_eq!(root[0].agent_id, "claude");
        let child = extend_chain(&root, "omp", "bb");
        assert_eq!(child.len(), 2);
        assert_eq!(child[0], root[0], "parent chain is never rewritten");
        assert_eq!(child[1].agent_id, "omp");
    }

    #[test]
    fn req_0006_044_fingerprint_is_nfc_and_whitespace_insensitive() {
        let composed = fingerprint("omp", ["\u{e9}"]);
        let decomposed = fingerprint("omp", ["e\u{301}"]);
        assert_eq!(composed, decomposed);
        assert_eq!(
            fingerprint("omp", ["a  b\t\nc"]),
            fingerprint("omp", ["a b c"])
        );
    }

    #[test]
    fn req_0006_044_fingerprint_depends_on_target_and_text_only() {
        let base = fingerprint("omp", ["do the thing"]);
        assert_eq!(base.len(), 32, "16 bytes as lowercase hex");
        assert!(
            base.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_ne!(base, fingerprint("claude", ["do the thing"]));
        assert_ne!(base, fingerprint("omp", ["do the other thing"]));
        assert_eq!(base, fingerprint("omp", ["do ", "the thing"]));
    }

    #[test]
    fn req_0006_044_fingerprint_matches_manual_digest() {
        let mut hasher = Sha256::new();
        hasher.update(b"omp\ndo the thing");
        let expected = hex_lower(&hasher.finalize()[..16]);
        assert_eq!(fingerprint("omp", ["do   the\u{a0}thing"]), expected);
    }

    #[test]
    fn req_0006_045_loop_detected_on_repeat_of_receiving_pair() {
        let chain = extend_chain(&root_chain("claude", "aa"), "omp", "bb");
        assert!(chain_has_loop(&chain, "claude", "aa"));
        assert!(chain_has_loop(&chain, "omp", "bb"));
        assert!(
            !chain_has_loop(&chain, "omp", "aa"),
            "same text to a new agent is not a loop"
        );
        assert!(
            !chain_has_loop(&chain, "claude", "bb"),
            "other text to the same agent is not a loop"
        );
    }

    #[test]
    fn req_0006_046_ping_pong_with_different_text_is_not_a_loop() {
        let mut chain = root_chain("claude", "f1");
        chain = extend_chain(&chain, "omp", "f2");
        assert!(!chain_has_loop(&chain, "claude", "f3"));
        chain = extend_chain(&chain, "claude", "f3");
        assert!(!chain_has_loop(&chain, "omp", "f4"));
    }

    #[test]
    fn req_0006_043_chain_before_self_returns_ancestors_only() {
        let chain = extend_chain(&root_chain("claude", "f1"), "omp", "f2");
        assert_eq!(chain_before_self(&chain, "omp"), Some(&chain[..1]));
        assert_eq!(
            chain_before_self(&chain, "claude"),
            None,
            "a chain that does not end with the receiver has no ancestors view"
        );
        assert_eq!(chain_before_self(&[], "omp"), None);
    }

    #[test]
    fn req_0006_059_reentrant_is_a_return_to_an_ancestor() {
        let chain = extend_chain(&root_chain("claude", "f1"), "omp", "f2");
        let at_b = chain_before_self(&chain, "omp").expect("ends with own hop");
        assert!(!is_reentrant(at_b, "omp"), "A -> B is not a re-entry");
        let back = extend_chain(&chain, "claude", "f3");
        let at_a = chain_before_self(&back, "claude").expect("ends with own hop");
        assert!(is_reentrant(at_a, "claude"), "A -> B -> A is a re-entry");
    }

    #[test]
    fn req_0006_048_breaker_opens_on_third_budget_or_loop_failure_in_window() {
        let mut b = Breakers::default();
        assert!(!b.record_failure("alice", FailureCode::BudgetExhausted, 0));
        assert!(!b.record_failure("alice", FailureCode::LoopDetected, 1_000));
        assert!(b.record_failure("alice", FailureCode::BudgetExhausted, 2_000));
        assert_eq!(
            b.status("alice", 3_000),
            BreakerStatus::Open {
                until_ms: 2_000 + BREAKER_OPEN_MS
            }
        );
        assert_eq!(b.status("bob", 3_000), BreakerStatus::Closed, "per origin");
        assert_eq!(
            b.status("alice", 2_000 + BREAKER_OPEN_MS),
            BreakerStatus::Closed
        );
    }

    #[test]
    fn req_0006_048_breaker_ignores_other_codes_and_stale_failures() {
        let mut b = Breakers::default();
        assert!(!b.record_failure("alice", FailureCode::BackendError, 0));
        assert!(!b.record_failure("alice", FailureCode::BudgetExhausted, 0));
        assert!(!b.record_failure("alice", FailureCode::BudgetExhausted, 1));
        // Third failure falls outside the first two windows.
        assert!(!b.record_failure("alice", FailureCode::BudgetExhausted, BREAKER_WINDOW_MS + 5));
        assert_eq!(
            b.status("alice", BREAKER_WINDOW_MS + 5),
            BreakerStatus::Closed
        );
    }

    #[test]
    fn req_0006_048_success_closes_breaker_early() {
        let mut b = Breakers::default();
        for t in 0..3 {
            b.record_failure("alice", FailureCode::LoopDetected, t);
        }
        assert!(matches!(b.status("alice", 10), BreakerStatus::Open { .. }));
        b.record_success("alice");
        assert_eq!(b.status("alice", 10), BreakerStatus::Closed);
    }

    #[test]
    fn req_0006_048_breaker_state_is_serializable() {
        let mut b = Breakers::default();
        for t in 0..3 {
            b.record_failure("alice", FailureCode::BudgetExhausted, t);
        }
        let text = serde_json::to_string(&b).expect("serialize");
        let back: Breakers = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(back, b);
    }

    #[test]
    fn req_0006_050_child_settle_reserve_matches_child_grant_exactly() {
        let mut parent = Ledger::root(grant(), metered()).expect("valid");
        let child = parent.delegate().expect("child");
        let mut child_ledger = Ledger::child(child, parent.depth() + 1, metered()).expect("valid");
        child_ledger.charge_tokens(10);
        parent.settle_child(&child, child_ledger.usage());
        assert_eq!(parent.reserved().tokens, 0);
        assert_eq!(parent.usage().tokens, 10);
    }
}
