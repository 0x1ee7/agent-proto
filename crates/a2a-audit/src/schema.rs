//! RFC-0013 event schema: table 4.2 (event types, emitter kinds and `details` keys) and the value
//! rules of 0013-008. Body-bearing content (R-A1) has no key in the table, so it is refused here.

use a2a_types::proto::a2abus::{AuditEvent, AuditEventType, EmitterKind};
use serde_json::{Map, Value};

use crate::clock::is_audit_timestamp;

/// Schema violation of an audit event.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SchemaError {
    /// A required event field is empty or malformed (0013-002, 0013-005).
    #[error("event field {0} is missing or malformed")]
    Field(&'static str),
    /// The `details` key is not listed for the type in table 4.2 (0013-008).
    #[error("{event}: details key {key} is not in RFC-0013 table 4.2")]
    UnknownDetailKey { event: &'static str, key: String },
    /// A key marked `*` in table 4.2 is absent (0013-008).
    #[error("{event}: required details key {key} is missing")]
    MissingDetailKey {
        event: &'static str,
        key: &'static str,
    },
    /// A value is not a string, boolean, list of strings or objects, or object (0013-008).
    #[error("details key {0} has a value type that RFC-0013 0013-008 does not admit")]
    BadDetailValue(String),
}

/// One `details` key of table 4.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    /// Key name as written in table 4.2.
    pub name: &'static str,
    /// True for keys marked `*`.
    pub required: bool,
}

const fn opt(name: &'static str) -> Key {
    Key {
        name,
        required: false,
    }
}

const fn req(name: &'static str) -> Key {
    Key {
        name,
        required: true,
    }
}

/// One row of table 4.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventSpec {
    /// Event type.
    pub ty: AuditEventType,
    /// Emitter kinds allowed to emit this type.
    pub kinds: &'static [EmitterKind],
    /// Keys admitted in `details`.
    pub keys: &'static [Key],
}

const SUP: &[EmitterKind] = &[EmitterKind::Supervisor];
const WRK: &[EmitterKind] = &[EmitterKind::Worker];
const GW: &[EmitterKind] = &[EmitterKind::Gateway];
const OPR: &[EmitterKind] = &[EmitterKind::Operator];
const WRK_GW: &[EmitterKind] = &[EmitterKind::Worker, EmitterKind::Gateway];
const WRK_SUP: &[EmitterKind] = &[EmitterKind::Worker, EmitterKind::Supervisor];
const ANY: &[EmitterKind] = &[
    EmitterKind::Supervisor,
    EmitterKind::Worker,
    EmitterKind::Gateway,
    EmitterKind::Operator,
];

/// Table 4.2 of RFC-0013, one row per `AUDIT_EVENT_TYPE_*` value. `*` keys are required.
/// `APPROVAL_REQUESTED.args_summary` is admitted because 0013-023 (R-A2) names it as a `details`
/// field of that type; table 4.2 does not list it (RFC feedback in the crate AGENTS.md).
pub static SPECS: &[EventSpec] = &[
    EventSpec {
        ty: AuditEventType::AgentStarted,
        kinds: SUP,
        keys: &[opt("pid"), opt("backend"), opt("workspace_mode")],
    },
    EventSpec {
        ty: AuditEventType::AgentStopped,
        kinds: SUP,
        keys: &[opt("reason"), opt("exit_code")],
    },
    EventSpec {
        ty: AuditEventType::AgentRestarted,
        kinds: SUP,
        keys: &[opt("attempt"), opt("backoff_ms"), opt("previous_reason")],
    },
    EventSpec {
        ty: AuditEventType::AgentAbandoned,
        kinds: SUP,
        keys: &[opt("failures_in_window"), opt("window_s")],
    },
    EventSpec {
        ty: AuditEventType::AgentPresenceChanged,
        kinds: WRK,
        keys: &[opt("from_state"), opt("to_state")],
    },
    EventSpec {
        ty: AuditEventType::DangerousModeActive,
        kinds: SUP,
        keys: &[opt("approval_mode"), opt("reason")],
    },
    EventSpec {
        ty: AuditEventType::StartRefused,
        kinds: SUP,
        keys: &[opt("reason")],
    },
    EventSpec {
        ty: AuditEventType::TaskSubmitted,
        kinds: WRK,
        keys: &[opt("origin"), opt("peer")],
    },
    EventSpec {
        ty: AuditEventType::TaskStateChanged,
        kinds: WRK,
        keys: &[req("from_state"), req("to_state"), opt("seq")],
    },
    EventSpec {
        ty: AuditEventType::TaskCancelRequested,
        kinds: WRK,
        keys: &[opt("requested_by")],
    },
    EventSpec {
        ty: AuditEventType::DelegationIssued,
        kinds: WRK,
        keys: &[req("to_agent"), opt("budget_remaining_hop"), opt("depth")],
    },
    EventSpec {
        ty: AuditEventType::ApprovalRequested,
        kinds: WRK,
        keys: &[
            req("request_id"),
            opt("tool"),
            opt("risk_tier"),
            opt("expires_at"),
            opt("args_summary"),
        ],
    },
    EventSpec {
        ty: AuditEventType::ApprovalDecided,
        kinds: WRK,
        keys: &[
            req("request_id"),
            opt("decision"),
            opt("decision_id"),
            opt("signature_status"),
            opt("key_id"),
        ],
    },
    EventSpec {
        ty: AuditEventType::BudgetDebited,
        kinds: WRK,
        keys: &[req("budget_kind"), req("amount"), opt("remaining")],
    },
    EventSpec {
        ty: AuditEventType::BudgetTripped,
        kinds: WRK,
        keys: &[req("budget_kind"), opt("limit"), opt("reason")],
    },
    EventSpec {
        ty: AuditEventType::LoopTripped,
        kinds: WRK,
        keys: &[opt("limit"), opt("count")],
    },
    EventSpec {
        ty: AuditEventType::SignatureFailed,
        kinds: WRK_GW,
        keys: &[req("component"), opt("key_id"), opt("reason")],
    },
    EventSpec {
        ty: AuditEventType::PeerRejected,
        kinds: GW,
        keys: &[opt("reason"), opt("peer"), opt("count")],
    },
    EventSpec {
        ty: AuditEventType::WorkspaceCreated,
        kinds: WRK,
        keys: &[opt("mode"), opt("kind"), opt("base_commit")],
    },
    EventSpec {
        ty: AuditEventType::WorkspaceRetained,
        kinds: WRK_SUP,
        keys: &[opt("reason")],
    },
    EventSpec {
        ty: AuditEventType::WorkspaceRemoved,
        kinds: WRK_SUP,
        keys: &[opt("reason")],
    },
    EventSpec {
        ty: AuditEventType::WorkspaceRefused,
        kinds: WRK_SUP,
        keys: &[opt("reason")],
    },
    EventSpec {
        ty: AuditEventType::OperatorAction,
        kinds: OPR,
        keys: &[opt("action"), opt("decision_id"), opt("outcome")],
    },
    EventSpec {
        ty: AuditEventType::RedactionApplied,
        kinds: WRK_GW,
        keys: &[opt("rules")],
    },
    EventSpec {
        ty: AuditEventType::AuditGap,
        kinds: ANY,
        keys: &[req("first_seq"), req("last_seq")],
    },
    EventSpec {
        ty: AuditEventType::AuditCheckpoint,
        kinds: SUP,
        keys: &[opt("emitters"), opt("signed_at")],
    },
    EventSpec {
        ty: AuditEventType::CardDecommissioned,
        kinds: WRK,
        keys: &[req("agent_id")],
    },
    EventSpec {
        ty: AuditEventType::TaskTransitionRefused,
        kinds: WRK,
        keys: &[req("task_id"), req("from_state"), req("attempted_state")],
    },
    EventSpec {
        ty: AuditEventType::LateOutputDropped,
        kinds: WRK,
        keys: &[req("task_id"), req("state")],
    },
    EventSpec {
        ty: AuditEventType::SubmissionDropped,
        kinds: WRK_GW,
        keys: &[req("message_id"), opt("reason")],
    },
    EventSpec {
        ty: AuditEventType::ApprovalFlood,
        kinds: WRK,
        keys: &[req("task_id"), req("open_count")],
    },
    EventSpec {
        ty: AuditEventType::ApprovalNotApplicable,
        kinds: WRK,
        keys: &[req("request_id"), opt("reason")],
    },
    EventSpec {
        ty: AuditEventType::ApprovalAutoAllowed,
        kinds: WRK,
        keys: &[req("task_id"), req("tool"), req("tier"), req("mode")],
    },
    EventSpec {
        ty: AuditEventType::TrustReloadRejected,
        kinds: SUP,
        keys: &[req("file"), req("reason")],
    },
    EventSpec {
        ty: AuditEventType::MessageRejected,
        kinds: WRK_GW,
        keys: &[req("reason")],
    },
    EventSpec {
        ty: AuditEventType::PermissionViolation,
        kinds: SUP,
        keys: &[req("subject"), opt("identity")],
    },
    EventSpec {
        ty: AuditEventType::ProcessLeftover,
        kinds: SUP,
        keys: &[req("agent_id"), req("pid")],
    },
];

/// Table row for `ty`, or `None` when table 4.2 does not list the type (0013-007).
pub fn spec(ty: AuditEventType) -> Option<&'static EventSpec> {
    SPECS.iter().find(|row| row.ty == ty)
}

/// Checks `details` against the row for its type: only listed keys, every `*` key present, and
/// value types per 0013-008.
pub fn check_details(spec: &EventSpec, details: &Map<String, Value>) -> Result<(), SchemaError> {
    let event = spec.ty.as_str_name();
    for (key, value) in details {
        if !spec.keys.iter().any(|k| k.name == key.as_str()) {
            return Err(SchemaError::UnknownDetailKey {
                event,
                key: key.clone(),
            });
        }
        if !admitted_value(value) {
            return Err(SchemaError::BadDetailValue(key.clone()));
        }
    }
    for key in spec.keys.iter().filter(|k| k.required) {
        if !details.contains_key(key.name) {
            return Err(SchemaError::MissingDetailKey {
                event,
                key: key.name,
            });
        }
    }
    Ok(())
}

/// 0013-008: strings, booleans, lists of strings or objects, and objects whose values follow the
/// same rules. Numbers and nulls are refused, so integers travel as decimal strings.
fn admitted_value(value: &Value) -> bool {
    match value {
        Value::String(_) | Value::Bool(_) => true,
        Value::Array(items) => items.iter().all(|item| {
            matches!(item, Value::String(_) | Value::Object(_)) && admitted_value(item)
        }),
        Value::Object(map) => map.values().all(admitted_value),
        Value::Null | Value::Number(_) => false,
    }
}

/// Checks the required envelope fields of an event (0013-002) and that its type and emitter kind
/// are allowed by table 4.2. Returns the event type.
pub fn validate_event(event: &AuditEvent) -> Result<AuditEventType, SchemaError> {
    if event.event_id.is_empty() {
        return Err(SchemaError::Field("event_id"));
    }
    if !is_audit_timestamp(&event.time) {
        return Err(SchemaError::Field("time"));
    }
    if event.seq == 0 {
        return Err(SchemaError::Field("seq"));
    }
    if event.tenant.is_empty() {
        return Err(SchemaError::Field("tenant"));
    }
    if event.emitter.is_empty() {
        return Err(SchemaError::Field("emitter"));
    }
    let kind = EmitterKind::try_from(event.emitter_kind)
        .ok()
        .filter(|k| *k != EmitterKind::Unspecified)
        .ok_or(SchemaError::Field("emitter_kind"))?;
    let ty = AuditEventType::try_from(event.r#type).map_err(|_| SchemaError::Field("type"))?;
    let row = spec(ty).ok_or(SchemaError::Field("type"))?;
    if !row.kinds.contains(&kind) {
        return Err(SchemaError::Field("emitter_kind"));
    }
    Ok(ty)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use serde_json::json;

    use super::*;

    /// 0013-008: table 4.2 has one row per enum value, and no row for the unspecified value.
    #[test]
    fn req_0013_008_table_covers_every_enum_value() {
        for value in 1..=37 {
            let ty = AuditEventType::try_from(value).unwrap();
            assert!(spec(ty).is_some(), "value {value} has no table row");
        }
        assert_eq!(SPECS.len(), 37);
        assert!(spec(AuditEventType::Unspecified).is_none());
    }

    /// 0013-008: unknown keys, numbers, nulls and body-bearing keys are refused (R-A1 structural).
    #[test]
    fn req_0013_008_refuses_unknown_keys_and_numbers() {
        let row = spec(AuditEventType::BudgetDebited).unwrap();
        let ok = json!({"budget_kind": "tokens", "amount": "12"});
        assert!(check_details(row, ok.as_object().unwrap()).is_ok());

        let number = json!({"budget_kind": "tokens", "amount": 12});
        assert_eq!(
            check_details(row, number.as_object().unwrap()),
            Err(SchemaError::BadDetailValue("amount".into()))
        );

        let body = json!({"budget_kind": "tokens", "amount": "1", "text": "hello"});
        assert!(matches!(
            check_details(row, body.as_object().unwrap()),
            Err(SchemaError::UnknownDetailKey { .. })
        ));
    }

    /// 0013-008: a `*` key that is absent is refused.
    #[test]
    fn req_0013_008_missing_required_key_is_refused() {
        let row = spec(AuditEventType::AuditGap).unwrap();
        let partial = json!({"first_seq": "4"});
        assert!(matches!(
            check_details(row, partial.as_object().unwrap()),
            Err(SchemaError::MissingDetailKey {
                key: "last_seq",
                ..
            })
        ));
    }

    /// 0013-008: nested objects and lists of objects are admitted; null inside them is not.
    #[test]
    fn req_0013_008_nested_values_follow_the_same_rules() {
        assert!(admitted_value(
            &json!({"a": {"seq": "1", "chain_hash": "x"}})
        ));
        assert!(admitted_value(&json!([{"k": "v"}, "s"])));
        assert!(!admitted_value(&json!({"a": null})));
        assert!(!admitted_value(&json!([1])));
    }

    /// 0013-002: an event missing an envelope field is rejected.
    #[test]
    fn req_0013_002_envelope_fields_are_required() {
        let mut event = AuditEvent {
            event_id: "0190c0de-0000-7000-8000-000000000000".into(),
            time: "2026-10-26T09:14:03.218Z".into(),
            seq: 1,
            tenant: "local".into(),
            emitter: "omp".into(),
            emitter_kind: EmitterKind::Worker as i32,
            r#type: AuditEventType::AgentPresenceChanged as i32,
            ..Default::default()
        };
        assert_eq!(
            validate_event(&event),
            Ok(AuditEventType::AgentPresenceChanged)
        );

        event.event_id.clear();
        assert_eq!(validate_event(&event), Err(SchemaError::Field("event_id")));
        event.event_id = "x".into();
        event.seq = 0;
        assert_eq!(validate_event(&event), Err(SchemaError::Field("seq")));
        event.seq = 1;
        event.emitter_kind = EmitterKind::Unspecified as i32;
        assert_eq!(
            validate_event(&event),
            Err(SchemaError::Field("emitter_kind"))
        );
    }

    /// 0013-007: an unknown enum value is not a table row; validation reports it rather than
    /// guessing an emitter kind.
    #[test]
    fn req_0013_007_unknown_type_has_no_row() {
        assert!(AuditEventType::try_from(999).is_err());
    }
}
