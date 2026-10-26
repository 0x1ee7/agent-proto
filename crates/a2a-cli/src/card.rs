//! Approval card: the fields an operator sees before deciding (RFC-0012 table 4.10.1, [0012-057]).

use a2a_types::proto::a2abus::{ApprovalOptionKind, ApprovalRequest, ApprovalTier};

use crate::output::paint;

/// Maximum characters of the argument summary ([0012-057] table 4.10.1).
pub const SUMMARY_MAX_CHARS: usize = 512;
/// Rendered for every card field the request does not carry.
pub const MISSING: &str = "missing";

/// Ordered card rows: label and value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    /// `(label, value)` pairs in table 4.10.1 order.
    pub rows: Vec<(&'static str, String)>,
}

/// Builds the card from an approval request. Fields absent from `ApprovalRequest` render as
/// [`MISSING`]; the proto has no budget or signature status.
#[must_use]
pub fn build(request: &ApprovalRequest) -> Card {
    let or_missing = |value: &str| {
        if value.is_empty() {
            MISSING.to_owned()
        } else {
            value.to_owned()
        }
    };
    let chain = if request.chain.is_empty() {
        MISSING.to_owned()
    } else {
        request.chain.join(" > ")
    };
    let tier = ApprovalTier::try_from(request.tier)
        .map(|tier| tier.as_str_name().to_owned())
        .unwrap_or_else(|_| MISSING.to_owned());
    let rows = vec![
        ("request id", or_missing(&request.request_id)),
        ("requesting agent", or_missing(&request.requester)),
        ("task chain", chain),
        ("tool", or_missing(&request.tool)),
        (
            "argument summary",
            truncate(&request.summary, SUMMARY_MAX_CHARS),
        ),
        ("working directory", or_missing(&request.working_directory)),
        ("workspace mode", or_missing(&request.workspace_mode)),
        ("risk tier", tier),
        ("budget remaining", MISSING.to_owned()),
        ("requested scope", scopes(request)),
        ("expires at", expires(request)),
        ("signature status", MISSING.to_owned()),
    ];
    Card { rows }
}

/// Renders the card, one row per line; labels are bold when `color` is set.
#[must_use]
pub fn render(card: &Card, color: bool) -> String {
    let width = card
        .rows
        .iter()
        .map(|(label, _)| label.len())
        .max()
        .unwrap_or(0);
    card.rows
        .iter()
        .map(|(label, value)| {
            format!(
                "{}  {value}",
                paint(&format!("{label:<width$}"), "1", color)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Scopes offered by the request's options. `once` needs an allow-once option; `task` needs an
/// allow-always option.
fn scopes(request: &ApprovalRequest) -> String {
    let offers = |kind: ApprovalOptionKind| {
        request
            .options
            .iter()
            .any(|option| option.kind == kind as i32)
    };
    let mut scopes = Vec::new();
    if offers(ApprovalOptionKind::AllowOnce) {
        scopes.push("once");
    }
    if offers(ApprovalOptionKind::AllowAlways) {
        scopes.push("task");
    }
    if scopes.is_empty() {
        MISSING.to_owned()
    } else {
        scopes.join(", ")
    }
}

fn expires(request: &ApprovalRequest) -> String {
    request
        .expires_at
        .as_ref()
        .and_then(|timestamp| serde_json::to_value(timestamp).ok())
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| MISSING.to_owned())
}

/// Truncates to `max` characters, marking the cut with `...` inside the limit.
#[must_use]
pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let kept: String = text.chars().take(max.saturating_sub(3)).collect();
    format!("{kept}...")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use a2a_types::proto::a2abus::ApprovalOption;

    fn request(summary: &str) -> ApprovalRequest {
        ApprovalRequest {
            request_id: "req-1".to_owned(),
            task_id: "task-1".to_owned(),
            tool: "shell".to_owned(),
            tier: ApprovalTier::Exec as i32,
            summary: summary.to_owned(),
            requester: "omp".to_owned(),
            chain: vec!["gateway".to_owned(), "omp".to_owned()],
            options: vec![ApprovalOption {
                option_id: "allow_once".to_owned(),
                name: "Allow once".to_owned(),
                kind: ApprovalOptionKind::AllowOnce as i32,
            }],
            expires_at: None,
            nonce: "n".to_owned(),
            working_directory: "/work/repo".to_owned(),
            workspace_mode: "per-task".to_owned(),
        }
    }

    fn value_of(card: &Card, label: &str) -> String {
        card.rows
            .iter()
            .find(|(name, _)| *name == label)
            .map(|(_, value)| value.clone())
            .unwrap_or_default()
    }

    // req_0012_057_card_fields_and_missing
    #[test]
    fn req_0012_057_card_fields_and_missing() {
        let card = build(&request("ls -la"));
        assert_eq!(card.rows.len(), 12);
        assert_eq!(value_of(&card, "request id"), "req-1");
        assert_eq!(value_of(&card, "requesting agent"), "omp");
        assert_eq!(value_of(&card, "task chain"), "gateway > omp");
        assert_eq!(value_of(&card, "risk tier"), "APPROVAL_TIER_EXEC");
        assert_eq!(value_of(&card, "requested scope"), "once");
        assert_eq!(value_of(&card, "working directory"), "/work/repo");
        assert_eq!(value_of(&card, "workspace mode"), "per-task");
        assert_eq!(value_of(&card, "budget remaining"), MISSING);
        assert_eq!(value_of(&card, "signature status"), MISSING);
        assert_eq!(value_of(&card, "expires at"), MISSING);
        let text = render(&card, false);
        assert!(text.contains("working directory  /work/repo"));
        assert!(!text.contains('\x1b'));
    }

    // req_0012_057_card_summary_truncated
    #[test]
    fn req_0012_057_card_summary_truncated() {
        let long = "x".repeat(600);
        let card = build(&request(&long));
        let summary = value_of(&card, "argument summary");
        assert_eq!(summary.chars().count(), SUMMARY_MAX_CHARS);
        assert!(summary.ends_with("..."));
        assert_eq!(truncate("short", SUMMARY_MAX_CHARS), "short");
    }

    // req_0012_057_card_scope_task_offered
    #[test]
    fn req_0012_057_card_scope_task_offered() {
        let mut req = request("ls");
        req.options.push(ApprovalOption {
            option_id: "always".to_owned(),
            name: "Allow always".to_owned(),
            kind: ApprovalOptionKind::AllowAlways as i32,
        });
        assert_eq!(value_of(&build(&req), "requested scope"), "once, task");
    }

    // req_0012_057_card_missing_chain_and_tier
    #[test]
    fn req_0012_057_card_missing_chain_and_tier() {
        let mut req = request("ls");
        req.chain.clear();
        req.tier = 999;
        req.requester.clear();
        let card = build(&req);
        assert_eq!(value_of(&card, "task chain"), MISSING);
        assert_eq!(value_of(&card, "risk tier"), MISSING);
        assert_eq!(value_of(&card, "requesting agent"), MISSING);
    }
}
