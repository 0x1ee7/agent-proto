//! Budget and lineage helpers for requesters (RFC-0006 section 4.3 and 4.4).
//!
//! The child allowance follows `a2a_core::budget::Ledger::delegate`: hops, depth, and children
//! are decremented; tokens and cost take the parent's remaining share; wall time keeps
//! `WALL_MARGIN_MS` back for the parent.

use a2a_core::budget::{
    Allowance, UNLIMITED, Usage, WALL_MARGIN_MS, chain_has_loop, extend_chain, fingerprint,
    root_chain,
};
use a2a_types::ids::AgentId;
use a2a_types::proto::a2abus::{Budget, BudgetUsage};
use pbjson_types::Struct;

use crate::error::ClientError;

/// Metadata key that carries the budget on a message (RFC-0006 0006-030).
pub(crate) const BUDGET_KEY: &str = a2a_types::vocab::ext::BUDGET;

/// Writes `budget` into `metadata` under the budget extension key.
pub(crate) fn attach(metadata: &mut Option<Struct>, budget: &Budget) -> Result<(), ClientError> {
    let value = serde_json::to_value(budget).map_err(|_| ClientError::Hook("budget encoding"))?;
    let wrapped = serde_json::json!({ BUDGET_KEY: value });
    let parsed: Struct =
        serde_json::from_value(wrapped).map_err(|_| ClientError::Hook("budget encoding"))?;
    metadata
        .get_or_insert_with(Struct::default)
        .fields
        .extend(parsed.fields);
    Ok(())
}

/// Builds the root budget that `owner` grants to a new task (RFC-0006 0006-031).
#[must_use]
pub fn root(owner: &AgentId, text_fingerprint: &str, allowance: &Allowance) -> Budget {
    allowance.to_proto(root_chain(owner.as_str(), text_fingerprint))
}

/// Computes the fingerprint of a task text sent to `target` (RFC-0006 0006-044).
#[must_use]
pub fn text_fingerprint(target: &AgentId, text: &str) -> String {
    fingerprint(target.as_str(), std::iter::once(text))
}

/// Derives the budget for a child delegated to `receiving` (RFC-0006 0006-033, 0006-045).
///
/// `consumed` must include both used and reserved amounts of the parent. Returns
/// `LoopDetected` when `(receiving, text_fingerprint)` already appears in the parent lineage.
pub fn child(
    parent: &Budget,
    consumed: &BudgetUsage,
    receiving: &AgentId,
    text_fingerprint: &str,
) -> Result<Budget, ClientError> {
    if chain_has_loop(&parent.chain, receiving.as_str(), text_fingerprint) {
        return Err(ClientError::LoopDetected);
    }
    let grant = Allowance::from_proto(parent);
    let used = Usage::from_proto(consumed);
    if grant.max_depth < 1 {
        return Err(ClientError::Delegation(
            a2a_core::budget::DelegationRefused::Depth,
        ));
    }
    if grant.max_hops.saturating_sub(used.hops) < 1 {
        return Err(ClientError::Delegation(
            a2a_core::budget::DelegationRefused::Hops,
        ));
    }
    if used.children >= grant.max_children {
        return Err(ClientError::Delegation(
            a2a_core::budget::DelegationRefused::Children,
        ));
    }
    let share = |limit: u64, spent: u64| {
        if limit == UNLIMITED {
            UNLIMITED
        } else {
            limit.saturating_sub(spent)
        }
    };
    let child = Allowance {
        max_tokens: share(grant.max_tokens, used.tokens),
        max_cost_units: share(grant.max_cost_units, used.cost_units),
        max_wall_ms: share(grant.max_wall_ms, used.wall_ms).saturating_sub(
            if grant.max_wall_ms == UNLIMITED {
                0
            } else {
                WALL_MARGIN_MS
            },
        ),
        max_hops: grant.max_hops - used.hops - 1,
        max_depth: grant.max_depth - 1,
        max_children: grant.max_children - 1,
    };
    Ok(child.to_proto(extend_chain(
        &parent.chain,
        receiving.as_str(),
        text_fingerprint,
    )))
}
