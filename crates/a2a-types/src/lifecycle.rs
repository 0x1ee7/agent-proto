//! Task state rules (RFC-0005 sec 4.2 and 4.9): wire decoding, emission rules, the Table 4.2
//! transition relation, and the failure-code pairing for FAILED and REJECTED.

use thiserror::Error;

use crate::failure::{FailureCode, FailureError, FailureReason};
use crate::proto::a2a::{TaskState, TaskStatus};

/// Errors from task state decoding, emission, and transitions.
#[derive(Debug, Error)]
pub enum LifecycleError {
    /// `TASK_STATE_UNSPECIFIED` was named; it is never emitted and is rejected on requests
    /// ([0005-005]).
    #[error("TASK_STATE_UNSPECIFIED is not a valid task state")]
    Unspecified,
    /// The wire value is not a known `TaskState`.
    #[error("unknown task state value {0}")]
    UnknownState(i32),
    /// The state exists but a worker must not emit it in M1 ([0005-009]).
    #[error("{0:?} must not be emitted in M1")]
    NotEmittable(TaskState),
    /// The transition is not in Table 4.2 ([0005-006]). `from` is `None` for task creation.
    #[error("transition {from:?} -> {to:?} is not in Table 4.2")]
    ForbiddenTransition {
        /// Previous state, or `None` on creation.
        from: Option<TaskState>,
        /// Requested state.
        to: TaskState,
    },
    /// A FAILED or REJECTED status carries no failure code ([0005-010]).
    #[error("{0:?} requires exactly one failure code")]
    MissingFailureCode(TaskState),
    /// A failure code is attached to a state that does not take one, or the code is not
    /// permitted on this transition (Table 4.2 and sec 4.9).
    #[error("failure code {code} is not permitted for {from:?} -> {to:?}")]
    ForbiddenFailureCode {
        /// Previous state, or `None` on creation.
        from: Option<TaskState>,
        /// Requested state.
        to: TaskState,
        /// The attached code.
        code: FailureCode,
    },
    /// The status message's failure metadata is malformed.
    #[error(transparent)]
    Failure(#[from] FailureError),
}

impl TaskState {
    /// Decodes a wire `TaskState`. `TASK_STATE_UNSPECIFIED` and unknown values are rejected
    /// ([0005-005]).
    pub fn from_wire(raw: i32) -> Result<Self, LifecycleError> {
        match Self::try_from(raw) {
            Ok(TaskState::Unspecified) => Err(LifecycleError::Unspecified),
            Ok(state) => Ok(state),
            Err(_) => Err(LifecycleError::UnknownState(raw)),
        }
    }

    /// True for COMPLETED, FAILED, CANCELED, and REJECTED ([0005-007]).
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskState::Completed | TaskState::Failed | TaskState::Canceled | TaskState::Rejected
        )
    }

    /// True for INPUT_REQUIRED, the interrupted and non-terminal state ([0005-008]).
    #[must_use]
    pub const fn is_interrupted(self) -> bool {
        matches!(self, TaskState::InputRequired)
    }
}

/// Which failure codes an edge accepts.
#[derive(Debug, Clone, Copy)]
enum CodeRule {
    /// The edge takes no failure code.
    None,
    /// Any code whose final state matches the edge target (Table 4.2 "see 4.9").
    Any,
    /// Only the listed codes.
    Only(&'static [FailureCode]),
}

/// Table 4.2 ([0005-006]). `None` as the source marks task creation.
const EDGES: &[(Option<TaskState>, TaskState, CodeRule)] = &[
    (None, TaskState::Submitted, CodeRule::None),
    (
        None,
        TaskState::Rejected,
        CodeRule::Only(&[FailureCode::RateLimited, FailureCode::QueueFull]),
    ),
    (
        None,
        TaskState::Failed,
        CodeRule::Only(&[FailureCode::LoopDetected]),
    ),
    (
        Some(TaskState::Submitted),
        TaskState::Working,
        CodeRule::None,
    ),
    (
        Some(TaskState::Submitted),
        TaskState::Failed,
        CodeRule::Only(&[
            FailureCode::BackendUnavailable,
            FailureCode::DeadlineExceeded,
        ]),
    ),
    (
        Some(TaskState::Submitted),
        TaskState::Canceled,
        CodeRule::None,
    ),
    (
        Some(TaskState::Working),
        TaskState::InputRequired,
        CodeRule::None,
    ),
    (
        Some(TaskState::Working),
        TaskState::Completed,
        CodeRule::None,
    ),
    (Some(TaskState::Working), TaskState::Failed, CodeRule::Any),
    (
        Some(TaskState::Working),
        TaskState::Canceled,
        CodeRule::None,
    ),
    (
        Some(TaskState::InputRequired),
        TaskState::Working,
        CodeRule::None,
    ),
    (
        Some(TaskState::InputRequired),
        TaskState::Failed,
        CodeRule::Only(&[
            FailureCode::ApprovalTimeout,
            FailureCode::DeadlineExceeded,
            FailureCode::BackendCrashed,
        ]),
    ),
    (
        Some(TaskState::InputRequired),
        TaskState::Canceled,
        CodeRule::None,
    ),
];

/// Rejects states a worker must never emit: `TASK_STATE_UNSPECIFIED` ([0005-005]) and
/// `AUTH_REQUIRED` in M1 ([0005-009]).
pub fn check_emittable(state: TaskState) -> Result<(), LifecycleError> {
    match state {
        TaskState::Unspecified => Err(LifecycleError::Unspecified),
        TaskState::AuthRequired => Err(LifecycleError::NotEmittable(state)),
        _ => Ok(()),
    }
}

/// Checks a proposed transition against Table 4.2 and the failure-code rules of sec 4.9.
///
/// `from` is `None` for task creation. `code` is the failure code carried by the new status,
/// if any. A FAILED or REJECTED target requires a code; every other target forbids one.
pub fn check_transition(
    from: Option<TaskState>,
    to: TaskState,
    code: Option<FailureCode>,
) -> Result<(), LifecycleError> {
    check_emittable(to)?;
    let (_, _, rule) = EDGES
        .iter()
        .find(|(edge_from, edge_to, _)| *edge_from == from && *edge_to == to)
        .ok_or(LifecycleError::ForbiddenTransition { from, to })?;
    match (to, code) {
        (TaskState::Failed | TaskState::Rejected, None) => {
            Err(LifecycleError::MissingFailureCode(to))
        }
        (TaskState::Failed | TaskState::Rejected, Some(code)) => {
            let permitted = code.final_state() == to
                && match rule {
                    CodeRule::None => false,
                    CodeRule::Any => true,
                    CodeRule::Only(codes) => codes.contains(&code),
                };
            if permitted {
                Ok(())
            } else {
                Err(LifecycleError::ForbiddenFailureCode { from, to, code })
            }
        }
        (_, None) => Ok(()),
        (_, Some(code)) => Err(LifecycleError::ForbiddenFailureCode { from, to, code }),
    }
}

/// Checks a whole status before it is committed: decodes its state, reads its failure reason
/// from the status message, and applies [`check_transition`].
pub fn check_status(from: Option<TaskState>, status: &TaskStatus) -> Result<(), LifecycleError> {
    let to = TaskState::from_wire(status.state)?;
    let code = match &status.message {
        None => None,
        Some(message) => FailureReason::from_message(message)?.map(|reason| reason.code),
    };
    check_transition(from, to, code)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::proto::a2a::Message;

    const ALL_STATES: [TaskState; 8] = [
        TaskState::Submitted,
        TaskState::Working,
        TaskState::Completed,
        TaskState::Failed,
        TaskState::Canceled,
        TaskState::InputRequired,
        TaskState::Rejected,
        TaskState::AuthRequired,
    ];

    fn status(state: TaskState, code: Option<FailureCode>) -> TaskStatus {
        let message = code.map(|code| {
            let mut message = Message::default();
            FailureReason::new(code)
                .apply_to_message(&mut message)
                .unwrap();
            message
        });
        TaskStatus {
            state: state as i32,
            message,
            timestamp: None,
        }
    }

    // [0005-005] TASK_STATE_UNSPECIFIED is rejected on decode; unknown values are rejected too.
    #[test]
    fn req_0005_005_unspecified_and_unknown_states_are_rejected() {
        assert!(matches!(
            TaskState::from_wire(0),
            Err(LifecycleError::Unspecified)
        ));
        assert!(matches!(
            TaskState::from_wire(99),
            Err(LifecycleError::UnknownState(99))
        ));
        assert_eq!(TaskState::from_wire(2).unwrap(), TaskState::Working);
    }

    // [0005-005] a request naming UNSPECIFIED cannot be emitted either.
    #[test]
    fn req_0005_005_unspecified_is_never_emitted() {
        assert!(matches!(
            check_emittable(TaskState::Unspecified),
            Err(LifecycleError::Unspecified)
        ));
    }

    // [0005-009] AUTH_REQUIRED is not emitted in M1; every other state is emittable.
    #[test]
    fn req_0005_009_auth_required_is_not_emitted() {
        assert!(matches!(
            check_emittable(TaskState::AuthRequired),
            Err(LifecycleError::NotEmittable(TaskState::AuthRequired))
        ));
        for state in ALL_STATES
            .into_iter()
            .filter(|s| *s != TaskState::AuthRequired)
        {
            assert!(check_emittable(state).is_ok(), "{state:?}");
        }
    }

    // [0005-007] exactly COMPLETED, FAILED, CANCELED, REJECTED are terminal.
    #[test]
    fn req_0005_007_terminal_states_are_exactly_four() {
        let terminal: Vec<TaskState> = ALL_STATES.into_iter().filter(|s| s.is_terminal()).collect();
        assert_eq!(
            terminal,
            vec![
                TaskState::Completed,
                TaskState::Failed,
                TaskState::Canceled,
                TaskState::Rejected
            ]
        );
    }

    // [0005-008] INPUT_REQUIRED is the one interrupted, non-terminal state.
    #[test]
    fn req_0005_008_input_required_is_interrupted_not_terminal() {
        assert!(TaskState::InputRequired.is_interrupted());
        assert!(!TaskState::InputRequired.is_terminal());
        assert!(
            ALL_STATES
                .into_iter()
                .filter(|s| s.is_interrupted())
                .count()
                == 1
        );
    }

    // [0005-006] every Table 4.2 edge is accepted with its permitted code.
    #[test]
    fn req_0005_006_table_edges_are_accepted() {
        let plain = [
            (None, TaskState::Submitted),
            (Some(TaskState::Submitted), TaskState::Working),
            (Some(TaskState::Submitted), TaskState::Canceled),
            (Some(TaskState::Working), TaskState::InputRequired),
            (Some(TaskState::Working), TaskState::Completed),
            (Some(TaskState::Working), TaskState::Canceled),
            (Some(TaskState::InputRequired), TaskState::Working),
            (Some(TaskState::InputRequired), TaskState::Canceled),
        ];
        for (from, to) in plain {
            assert!(
                check_transition(from, to, None).is_ok(),
                "{from:?} -> {to:?}"
            );
        }
    }

    // [0005-006] transitions outside Table 4.2 are refused, including out of terminal states.
    #[test]
    fn req_0005_006_non_table_transitions_are_refused() {
        let forbidden = [
            (None, TaskState::Working),
            (None, TaskState::Completed),
            (Some(TaskState::Submitted), TaskState::Completed),
            (Some(TaskState::Submitted), TaskState::InputRequired),
            (Some(TaskState::Working), TaskState::Submitted),
            (Some(TaskState::InputRequired), TaskState::Completed),
            (Some(TaskState::Completed), TaskState::Working),
            (Some(TaskState::Failed), TaskState::Working),
            (Some(TaskState::Canceled), TaskState::Canceled),
            (Some(TaskState::Rejected), TaskState::Failed),
        ];
        for (from, to) in forbidden {
            assert!(
                matches!(
                    check_transition(from, to, None),
                    Err(LifecycleError::ForbiddenTransition { .. })
                ),
                "{from:?} -> {to:?} must be refused"
            );
        }
    }

    // [0005-007] a terminal task never leaves its terminal state, whatever the target.
    #[test]
    fn req_0005_007_terminal_states_have_no_outgoing_edges() {
        for from in ALL_STATES.into_iter().filter(|s| s.is_terminal()) {
            for to in ALL_STATES {
                assert!(
                    check_transition(Some(from), to, None).is_err(),
                    "{from:?} must not move to {to:?}"
                );
            }
        }
    }

    // [0005-010] FAILED and REJECTED require exactly one code; a missing code is refused.
    #[test]
    fn req_0005_010_failed_and_rejected_require_a_code() {
        assert!(matches!(
            check_transition(Some(TaskState::Working), TaskState::Failed, None),
            Err(LifecycleError::MissingFailureCode(TaskState::Failed))
        ));
        assert!(matches!(
            check_transition(None, TaskState::Rejected, None),
            Err(LifecycleError::MissingFailureCode(TaskState::Rejected))
        ));
    }

    // [0005-010] a code is refused on states that take none, and on the wrong final state.
    #[test]
    fn req_0005_010_codes_are_refused_where_they_do_not_belong() {
        assert!(matches!(
            check_transition(
                Some(TaskState::Working),
                TaskState::Completed,
                Some(FailureCode::DeadlineExceeded)
            ),
            Err(LifecycleError::ForbiddenFailureCode { .. })
        ));
        // A REJECTED-final code cannot make a FAILED status.
        assert!(matches!(
            check_transition(
                Some(TaskState::Working),
                TaskState::Failed,
                Some(FailureCode::QueueFull)
            ),
            Err(LifecycleError::ForbiddenFailureCode { .. })
        ));
    }

    // [0005-048] creation can end REJECTED only with RATE_LIMITED or QUEUE_FULL.
    #[test]
    fn req_0005_048_admission_refusal_takes_only_overload_codes() {
        assert!(
            check_transition(None, TaskState::Rejected, Some(FailureCode::RateLimited)).is_ok()
        );
        assert!(check_transition(None, TaskState::Rejected, Some(FailureCode::QueueFull)).is_ok());
        assert!(matches!(
            check_transition(
                None,
                TaskState::Rejected,
                Some(FailureCode::DeadlineExceeded)
            ),
            Err(LifecycleError::ForbiddenFailureCode { .. })
        ));
    }

    // [0005-048] the creation-time loop refusal is FAILED with LOOP_DETECTED only.
    #[test]
    fn req_0005_048_loop_detection_at_creation_is_failed_with_loop_code() {
        assert!(check_transition(None, TaskState::Failed, Some(FailureCode::LoopDetected)).is_ok());
        assert!(matches!(
            check_transition(None, TaskState::Failed, Some(FailureCode::BudgetExhausted)),
            Err(LifecycleError::ForbiddenFailureCode { .. })
        ));
    }

    // [0005-048] SUBMITTED to FAILED takes BACKEND_UNAVAILABLE or DEADLINE_EXCEEDED only.
    #[test]
    fn req_0005_048_submitted_failure_codes_are_restricted() {
        for code in [
            FailureCode::BackendUnavailable,
            FailureCode::DeadlineExceeded,
        ] {
            assert!(
                check_transition(Some(TaskState::Submitted), TaskState::Failed, Some(code)).is_ok()
            );
        }
        assert!(matches!(
            check_transition(
                Some(TaskState::Submitted),
                TaskState::Failed,
                Some(FailureCode::BackendCrashed)
            ),
            Err(LifecycleError::ForbiddenFailureCode { .. })
        ));
    }

    // [0005-048] WORKING to FAILED accepts any FAILED-final code of the registry.
    #[test]
    fn req_0005_048_working_failure_accepts_every_failed_code() {
        for code in FailureCode::ALL {
            let result = check_transition(Some(TaskState::Working), TaskState::Failed, Some(code));
            if code.final_state() == TaskState::Failed {
                assert!(result.is_ok(), "{code}");
            } else {
                assert!(result.is_err(), "{code}");
            }
        }
    }

    // [0005-048] INPUT_REQUIRED to FAILED takes APPROVAL_TIMEOUT, DEADLINE_EXCEEDED, or
    // BACKEND_CRASHED only.
    #[test]
    fn req_0005_048_interrupted_failure_codes_are_restricted() {
        for code in [
            FailureCode::ApprovalTimeout,
            FailureCode::DeadlineExceeded,
            FailureCode::BackendCrashed,
        ] {
            assert!(
                check_transition(
                    Some(TaskState::InputRequired),
                    TaskState::Failed,
                    Some(code)
                )
                .is_ok()
            );
        }
        assert!(matches!(
            check_transition(
                Some(TaskState::InputRequired),
                TaskState::Failed,
                Some(FailureCode::ApprovalDenied)
            ),
            Err(LifecycleError::ForbiddenFailureCode { .. })
        ));
    }

    // [0005-010] check_status reads the code from the status message and applies the rules.
    #[test]
    fn req_0005_010_check_status_reads_the_code_from_the_message() {
        assert!(
            check_status(
                Some(TaskState::Working),
                &status(TaskState::Failed, Some(FailureCode::DeadlineExceeded))
            )
            .is_ok()
        );
        assert!(matches!(
            check_status(Some(TaskState::Working), &status(TaskState::Failed, None)),
            Err(LifecycleError::MissingFailureCode(TaskState::Failed))
        ));
        assert!(matches!(
            check_status(
                Some(TaskState::Working),
                &status(TaskState::Completed, Some(FailureCode::DeadlineExceeded))
            ),
            Err(LifecycleError::ForbiddenFailureCode { .. })
        ));
    }

    // [0005-005] check_status refuses an UNSPECIFIED state on the wire.
    #[test]
    fn req_0005_005_check_status_refuses_unspecified_state() {
        let wire = TaskStatus {
            state: 0,
            message: None,
            timestamp: None,
        };
        assert!(matches!(
            check_status(None, &wire),
            Err(LifecycleError::Unspecified)
        ));
    }
}
