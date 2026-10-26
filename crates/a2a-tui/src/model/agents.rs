//! Agent roster from the directory (RFC-0004 presence, RFC-0012 4.10.1 agents screen).

use std::collections::BTreeMap;

use serde_json::Value;

use crate::redact;

/// Presence state as shown to the operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    Starting,
    Ready,
    Draining,
}

impl Presence {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Starting => "STARTING",
            Self::Ready => "READY",
            Self::Draining => "DRAINING",
        }
    }
}

/// One directory observation, already reduced to the fields the console shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed {
    pub agent: String,
    pub card_name: Option<String>,
    pub card_version: Option<String>,
    pub presence: Option<Presence>,
    pub active_tasks: u32,
    /// The `approvalMode` declared on the card, if any.
    pub approval_mode: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    pub agent: String,
    pub card_name: String,
    pub card_version: String,
    /// `None` when the agent is absent from presence (shown as `absent`).
    pub presence: Option<Presence>,
    pub active_tasks: u32,
    pub approval_mode: String,
    /// RFC-0012 0012-051: set for modes classified as dangerous by RFC-0009 [0009-063].
    pub dangerous: bool,
    pub seen_ms: u64,
}

#[derive(Debug, Default)]
pub struct Roster {
    rows: BTreeMap<String, AgentRow>,
}

impl Roster {
    /// The directory is authoritative: agents missing from `observed` disappear.
    pub fn replace(&mut self, now_ms: u64, observed: Vec<Observed>) {
        let mut rows = BTreeMap::new();
        for item in observed {
            let mode = item.approval_mode.unwrap_or_default();
            let dangerous = is_dangerous_mode(&mode);
            let row = AgentRow {
                card_name: redact::truncate_chars(
                    &redact::escape(item.card_name.as_deref().unwrap_or("")),
                    64,
                ),
                card_version: redact::truncate_chars(
                    &redact::escape(item.card_version.as_deref().unwrap_or("")),
                    32,
                ),
                approval_mode: redact::truncate_chars(&redact::escape(&mode), 32),
                agent: redact::truncate_chars(&redact::escape(&item.agent), 128),
                presence: item.presence,
                active_tasks: item.active_tasks,
                dangerous,
                seen_ms: now_ms,
            };
            rows.insert(item.agent, row);
        }
        self.rows = rows;
    }

    /// Agents sorted by id.
    pub fn iter(&self) -> impl Iterator<Item = &AgentRow> {
        self.rows.values()
    }

    #[must_use]
    pub fn get(&self, agent: &str) -> Option<&AgentRow> {
        self.rows.get(agent)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// Reads `approvalMode` anywhere in a card (RFC-0009 [0009-066] records it on the
/// approval extension parameters).
#[must_use]
pub fn approval_mode_of(card: &Value) -> Option<String> {
    match card {
        Value::Object(map) => {
            if let Some(Value::String(mode)) = map.get("approvalMode") {
                return Some(mode.clone());
            }
            map.values().find_map(approval_mode_of)
        }
        Value::Array(items) => items.iter().find_map(approval_mode_of),
        _ => None,
    }
}

/// Dangerous canonical modes (RFC-0009 [0009-063]). `bypass` canonicalises to
/// `bypassPermissions` before the check.
#[must_use]
pub fn is_dangerous_mode(mode: &str) -> bool {
    let canonical = if mode == "bypass" {
        "bypassPermissions"
    } else {
        mode
    };
    matches!(
        canonical,
        "write" | "yolo" | "acceptEdits" | "bypassPermissions" | "dontAsk" | "auto"
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn observed(agent: &str, mode: Option<&str>) -> Observed {
        Observed {
            agent: agent.to_owned(),
            card_name: Some("Worker".to_owned()),
            card_version: Some("1".to_owned()),
            presence: Some(Presence::Ready),
            active_tasks: 2,
            approval_mode: mode.map(str::to_owned),
        }
    }

    #[test]
    fn req_0012_051_dangerous_modes_are_flagged() {
        let mut roster = Roster::default();
        roster.replace(
            5,
            vec![
                observed("claude", Some("bypass")),
                observed("omp", Some("yolo")),
                observed("safe", Some("default")),
            ],
        );
        assert!(roster.get("claude").expect("row").dangerous);
        assert!(roster.get("omp").expect("row").dangerous);
        assert!(!roster.get("safe").expect("row").dangerous);
        assert!(is_dangerous_mode("acceptEdits"));
        assert!(!is_dangerous_mode("always-ask-cached"));
    }

    #[test]
    fn req_0004_presence_absent_agents_are_listed_without_presence() {
        let mut roster = Roster::default();
        let mut offline = observed("gone", None);
        offline.presence = None;
        roster.replace(7, vec![offline]);
        let row = roster.get("gone").expect("row");
        assert_eq!(row.presence, None);
        assert_eq!(row.seen_ms, 7);
    }

    #[test]
    fn req_0004_presence_labels_match_rfc_states() {
        assert_eq!(Presence::Starting.label(), "STARTING");
        assert_eq!(Presence::Ready.label(), "READY");
        assert_eq!(Presence::Draining.label(), "DRAINING");
    }

    #[test]
    fn req_0012_051_approval_mode_is_found_in_card_extensions() {
        let card = serde_json::json!({
            "capabilities": {"extensions": [{"params": {"approvalMode": "yolo"}}]}
        });
        assert_eq!(approval_mode_of(&card).as_deref(), Some("yolo"));
        assert_eq!(approval_mode_of(&serde_json::json!({})), None);
    }
}
