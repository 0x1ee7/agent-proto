//! Subject grammar and operation tokens (RFC-0002 sections 4.1 and 4.3).
//!
//! Subjects are `a2a.v1.<tenant>.rpc.<agent>.<op>`, `a2a.v1.<tenant>.ev.<agent>.<task_id>` and
//! `a2a.v1.<tenant>.audit.<agent>`. Nothing else lives under the root.

use a2a_types::ids::{AgentId, TaskId, TenantId};
use bus_api::name::{InvalidName, Subject};

/// Fixed first two tokens of every A2A-Bus subject (RFC-0002 [0002-001]).
pub const ROOT_TOKENS: [&str; 2] = ["a2a", "v1"];
/// Request family token.
pub const RPC: &str = "rpc";
/// Event family token.
pub const EVENT: &str = "ev";
/// Audit family token.
pub const AUDIT: &str = "audit";

/// An operation token of the request family (RFC-0002 [0002-020]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Op {
    /// `send`: SendMessage.
    Send,
    /// `stream`: SendStreamingMessage, initial element on the reply.
    Stream,
    /// `get`: GetTask.
    Get,
    /// `list`: ListTasks.
    List,
    /// `cancel`: CancelTask.
    Cancel,
    /// `subscribe`: SubscribeToTask, snapshot on the reply.
    Subscribe,
    /// `push-create`: CreateTaskPushNotificationConfig.
    PushCreate,
    /// `push-get`: GetTaskPushNotificationConfig.
    PushGet,
    /// `push-list`: ListTaskPushNotificationConfigs.
    PushList,
    /// `push-delete`: DeleteTaskPushNotificationConfig.
    PushDelete,
    /// `card-ext`: GetExtendedAgentCard.
    CardExt,
}

impl Op {
    /// Every operation, in table order.
    pub const ALL: [Op; 11] = [
        Op::Send,
        Op::Stream,
        Op::Get,
        Op::List,
        Op::Cancel,
        Op::Subscribe,
        Op::PushCreate,
        Op::PushGet,
        Op::PushList,
        Op::PushDelete,
        Op::CardExt,
    ];

    /// The single lowercase subject token (RFC-0002 [0002-020]).
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Op::Send => "send",
            Op::Stream => "stream",
            Op::Get => "get",
            Op::List => "list",
            Op::Cancel => "cancel",
            Op::Subscribe => "subscribe",
            Op::PushCreate => "push-create",
            Op::PushGet => "push-get",
            Op::PushList => "push-list",
            Op::PushDelete => "push-delete",
            Op::CardExt => "card-ext",
        }
    }

    /// Finds the operation for a subject token. Aliases such as `message/send` are not tokens
    /// (RFC-0002 [0002-023]) and return `None`.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Op> {
        Op::ALL.into_iter().find(|op| op.token() == token)
    }

    /// Fully qualified protobuf name of the request body, carried in `A2AB-Payload-Type`.
    ///
    /// `push-create` carries `lf.a2a.v1.TaskPushNotificationConfig`: the A2A proto has no
    /// `CreateTaskPushNotificationConfigRequest` message (see the RFC feedback in the crate report).
    #[must_use]
    pub const fn request_type(self) -> &'static str {
        match self {
            Op::Send | Op::Stream => "lf.a2a.v1.SendMessageRequest",
            Op::Get => "lf.a2a.v1.GetTaskRequest",
            Op::List => "lf.a2a.v1.ListTasksRequest",
            Op::Cancel => "lf.a2a.v1.CancelTaskRequest",
            Op::Subscribe => "lf.a2a.v1.SubscribeToTaskRequest",
            Op::PushCreate => "lf.a2a.v1.TaskPushNotificationConfig",
            Op::PushGet => "lf.a2a.v1.GetTaskPushNotificationConfigRequest",
            Op::PushList => "lf.a2a.v1.ListTaskPushNotificationConfigsRequest",
            Op::PushDelete => "lf.a2a.v1.DeleteTaskPushNotificationConfigRequest",
            Op::CardExt => "lf.a2a.v1.GetExtendedAgentCardRequest",
        }
    }

    /// Fully qualified protobuf name of the success reply (RFC-0002 section 4.3).
    #[must_use]
    pub const fn reply_type(self) -> &'static str {
        match self {
            Op::Send => "lf.a2a.v1.SendMessageResponse",
            Op::Stream | Op::Subscribe => "lf.a2a.v1.StreamResponse",
            Op::Get | Op::Cancel => "lf.a2a.v1.Task",
            Op::List => "lf.a2a.v1.ListTasksResponse",
            Op::PushCreate | Op::PushGet => "lf.a2a.v1.TaskPushNotificationConfig",
            Op::PushList => "lf.a2a.v1.ListTaskPushNotificationConfigsResponse",
            Op::PushDelete => "google.protobuf.Empty",
            Op::CardExt => "lf.a2a.v1.AgentCard",
        }
    }

    /// True for the `push-*` operations, which are capability-gated (RFC-0002 [0002-024]).
    #[must_use]
    pub const fn is_push(self) -> bool {
        matches!(
            self,
            Op::PushCreate | Op::PushGet | Op::PushList | Op::PushDelete
        )
    }
}

/// What a valid A2A-Bus subject addresses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Family {
    /// A unary request to the Worker of `agent`.
    Rpc {
        /// Target agent.
        agent: AgentId,
        /// Requested operation.
        op: Op,
    },
    /// Ordered per-task events published by the Worker of `agent`.
    Event {
        /// Publishing agent.
        agent: AgentId,
        /// Task whose events these are.
        task: TaskId,
    },
    /// Audit records of `agent`.
    Audit {
        /// Agent whose audit records these are.
        agent: AgentId,
    },
}

/// A parsed A2A-Bus subject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address {
    /// Tenant token of the subject.
    pub tenant: TenantId,
    /// Family and its tokens.
    pub family: Family,
}

/// Why a subject is not a request this binding serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubjectError {
    /// Not an A2A-Bus subject, or a family this binding does not serve as a request. The
    /// message is dropped with no reply (RFC-0002 [0002-002]).
    Foreign,
    /// An A2A-Bus subject with an invalid token or token count. Answered `MALFORMED_MESSAGE`.
    Malformed,
    /// An `rpc` subject whose operation token is not in the table. Answered with code 12,
    /// reason `UNSUPPORTED_OPERATION` (RFC-0002 [0002-023]).
    UnknownOperation,
}

/// Parses a subject received on the bus.
///
/// # Errors
/// Returns [`SubjectError::Foreign`] for subjects outside `a2a.v1`, [`SubjectError::Malformed`]
/// for invalid tokens or token counts, and [`SubjectError::UnknownOperation`] for an `rpc`
/// subject with an unknown operation token.
pub fn parse(subject: &str) -> Result<Address, SubjectError> {
    let tokens: Vec<&str> = subject.split('.').collect();
    if tokens.len() < 2 || tokens[0] != ROOT_TOKENS[0] || tokens[1] != ROOT_TOKENS[1] {
        return Err(SubjectError::Foreign);
    }
    let tenant_token = tokens.get(2).ok_or(SubjectError::Malformed)?;
    let tenant = TenantId::parse(tenant_token).map_err(|_| SubjectError::Malformed)?;
    let family_token = tokens.get(3).ok_or(SubjectError::Malformed)?;
    let family = match *family_token {
        RPC => {
            if tokens.len() != 6 {
                return Err(SubjectError::Malformed);
            }
            let agent = parse_agent(tokens[4])?;
            let op = Op::from_token(tokens[5]).ok_or(SubjectError::UnknownOperation)?;
            Family::Rpc { agent, op }
        }
        EVENT => {
            if tokens.len() != 6 {
                return Err(SubjectError::Malformed);
            }
            let agent = parse_agent(tokens[4])?;
            let task = TaskId::parse(tokens[5]).map_err(|_| SubjectError::Malformed)?;
            if !task.is_uuidv7() {
                return Err(SubjectError::Malformed);
            }
            Family::Event { agent, task }
        }
        AUDIT => {
            if tokens.len() != 5 {
                return Err(SubjectError::Malformed);
            }
            Family::Audit {
                agent: parse_agent(tokens[4])?,
            }
        }
        _ => return Err(SubjectError::Foreign),
    };
    Ok(Address { tenant, family })
}

fn parse_agent(token: &str) -> Result<AgentId, SubjectError> {
    AgentId::parse(token).map_err(|_| SubjectError::Malformed)
}

/// Builds the request subject `a2a.v1.<tenant>.rpc.<agent>.<op>`.
///
/// # Errors
/// Returns [`InvalidName`] when the bus rejects the built subject.
pub fn rpc(tenant: &TenantId, agent: &AgentId, op: Op) -> Result<Subject, InvalidName> {
    Subject::parse(&format!(
        "a2a.v1.{}.{RPC}.{}.{}",
        tenant.as_str(),
        agent.as_str(),
        op.token()
    ))
}

/// Builds the event subject `a2a.v1.<tenant>.ev.<agent>.<task_id>`.
///
/// # Errors
/// Returns [`InvalidName`] when the bus rejects the built subject.
pub fn event(tenant: &TenantId, agent: &AgentId, task: &TaskId) -> Result<Subject, InvalidName> {
    Subject::parse(&format!(
        "a2a.v1.{}.{EVENT}.{}.{}",
        tenant.as_str(),
        agent.as_str(),
        task.as_str()
    ))
}

/// Builds the audit subject `a2a.v1.<tenant>.audit.<agent>`.
///
/// # Errors
/// Returns [`InvalidName`] when the bus rejects the built subject.
pub fn audit(tenant: &TenantId, agent: &AgentId) -> Result<Subject, InvalidName> {
    Subject::parse(&format!(
        "a2a.v1.{}.{AUDIT}.{}",
        tenant.as_str(),
        agent.as_str()
    ))
}

/// Builds the wildcard that subscribes to every event of `agent`:
/// `a2a.v1.<tenant>.ev.<agent>.>` (RFC-0002 section 6, subject permissions).
///
/// # Errors
/// Returns [`InvalidName`] when the bus rejects the built pattern.
pub fn events_of(
    tenant: &TenantId,
    agent: &AgentId,
) -> Result<bus_api::name::SubjectPattern, InvalidName> {
    bus_api::name::SubjectPattern::parse(&format!(
        "a2a.v1.{}.{EVENT}.{}.>",
        tenant.as_str(),
        agent.as_str()
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const TASK: &str = "0196f3a2-7c1e-7d4a-9b2e-5f1c8a3d6e21";

    fn tenant() -> TenantId {
        TenantId::parse("local").expect("valid fixture tenant")
    }

    fn agent(value: &str) -> AgentId {
        AgentId::parse(value).expect("valid fixture agent")
    }

    #[test]
    fn req_0002_020_every_op_token_round_trips() {
        for op in Op::ALL {
            assert_eq!(Op::from_token(op.token()), Some(op), "{}", op.token());
            assert!(
                op.token()
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b == b'-')
            );
        }
        assert_eq!(Op::ALL.len(), 11, "RFC-0002 table 4.3 has eleven rows");
    }

    #[test]
    fn req_0002_023_aliases_are_not_tokens() {
        assert_eq!(Op::from_token("message/send"), None);
        assert_eq!(Op::from_token("SEND"), None);
        assert_eq!(Op::from_token("push_create"), None);
        assert_eq!(Op::from_token(""), None);
    }

    #[test]
    fn req_0002_021_payload_names_match_the_table() {
        assert_eq!(Op::Send.request_type(), "lf.a2a.v1.SendMessageRequest");
        assert_eq!(Op::Stream.request_type(), "lf.a2a.v1.SendMessageRequest");
        assert_eq!(Op::Stream.reply_type(), "lf.a2a.v1.StreamResponse");
        assert_eq!(Op::Subscribe.reply_type(), "lf.a2a.v1.StreamResponse");
        assert_eq!(Op::PushDelete.reply_type(), "google.protobuf.Empty");
        assert_eq!(Op::CardExt.reply_type(), "lf.a2a.v1.AgentCard");
        assert!(Op::PushGet.is_push());
        assert!(!Op::Get.is_push());
    }

    #[test]
    fn req_0002_001_rpc_subject_parses_to_op() {
        let parsed = parse("a2a.v1.local.rpc.omp.send");
        assert_eq!(
            parsed,
            Ok(Address {
                tenant: tenant(),
                family: Family::Rpc {
                    agent: agent("omp"),
                    op: Op::Send,
                },
            })
        );
    }

    #[test]
    fn req_0002_001_event_subject_parses_to_task() {
        let address = parse(&format!("a2a.v1.local.ev.omp.{TASK}")).expect("event subject parses");
        assert!(matches!(
            address.family,
            Family::Event { ref task, .. } if task.as_str() == TASK
        ));
    }

    #[test]
    fn req_0002_001_audit_subject_parses() {
        assert!(matches!(
            parse("a2a.v1.local.audit.gateway"),
            Ok(Address {
                family: Family::Audit { .. },
                ..
            })
        ));
    }

    #[test]
    fn req_0002_002_foreign_subjects_are_dropped() {
        assert_eq!(parse("foo.bar.baz"), Err(SubjectError::Foreign));
        assert_eq!(
            parse("a2a.v2.local.rpc.omp.send"),
            Err(SubjectError::Foreign)
        );
        assert_eq!(
            parse("a2a.v1.local.bogus.omp.x"),
            Err(SubjectError::Foreign)
        );
        assert_eq!(parse("a2a"), Err(SubjectError::Foreign));
    }

    #[test]
    fn req_0002_001_malformed_subjects_are_detected() {
        assert_eq!(parse("a2a.v1"), Err(SubjectError::Malformed), "no tenant");
        assert_eq!(
            parse("a2a.v1.LOCAL.rpc.omp.send"),
            Err(SubjectError::Malformed)
        );
        assert_eq!(parse("a2a.v1.local.rpc.omp"), Err(SubjectError::Malformed));
        assert_eq!(
            parse("a2a.v1.local.rpc.omp.send.extra"),
            Err(SubjectError::Malformed)
        );
        assert_eq!(
            parse("a2a.v1.local.rpc.Omp.send"),
            Err(SubjectError::Malformed)
        );
        assert_eq!(
            parse("a2a.v1.local.rpc.*.send"),
            Err(SubjectError::Malformed)
        );
        assert_eq!(parse("a2a.v1.local.rpc.>"), Err(SubjectError::Malformed));
        assert_eq!(
            parse("a2a.v1.local.ev.omp.not-a-uuid"),
            Err(SubjectError::Malformed)
        );
        assert_eq!(
            parse("a2a.v1.local.ev.omp.0196F3A2-7C1E-7D4A-9B2E-5F1C8A3D6E21"),
            Err(SubjectError::Malformed),
            "task ids are canonical lowercase"
        );
        assert_eq!(
            parse("a2a.v1.local.ev.omp.550e8400-e29b-41d4-a716-446655440000"),
            Err(SubjectError::Malformed),
            "task ids are UUIDv7"
        );
        assert_eq!(
            parse("a2a.v1.local.audit.omp.extra"),
            Err(SubjectError::Malformed)
        );
    }

    #[test]
    fn req_0002_023_unknown_operation_token_is_its_own_error() {
        assert_eq!(
            parse("a2a.v1.local.rpc.omp.message/send"),
            Err(SubjectError::UnknownOperation)
        );
        assert_eq!(
            parse("a2a.v1.local.rpc.omp.frobnicate"),
            Err(SubjectError::UnknownOperation)
        );
    }

    #[test]
    fn req_0002_001_built_subjects_round_trip_through_parse() {
        let t = tenant();
        let a = agent("claude");
        let task = TaskId::parse(TASK).expect("valid fixture task");
        let rpc_subject = rpc(&t, &a, Op::PushCreate);
        assert_eq!(
            rpc_subject.as_ref().map(Subject::as_str).ok(),
            Some("a2a.v1.local.rpc.claude.push-create")
        );
        let event_subject = event(&t, &a, &task);
        assert_eq!(
            event_subject.as_ref().map(Subject::as_str).ok(),
            Some("a2a.v1.local.ev.claude.0196f3a2-7c1e-7d4a-9b2e-5f1c8a3d6e21")
        );
        let audit_subject = audit(&t, &a);
        assert_eq!(
            audit_subject.as_ref().map(Subject::as_str).ok(),
            Some("a2a.v1.local.audit.claude")
        );
        let pattern = events_of(&t, &a);
        assert_eq!(
            pattern
                .as_ref()
                .map(bus_api::name::SubjectPattern::as_str)
                .ok(),
            Some("a2a.v1.local.ev.claude.>")
        );
        let parsed = parse("a2a.v1.local.rpc.claude.push-create");
        assert!(matches!(
            parsed,
            Ok(Address {
                family: Family::Rpc {
                    op: Op::PushCreate,
                    ..
                },
                ..
            })
        ));
    }
}
