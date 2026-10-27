//! Contract checks for any [`Backend`](crate::backend::Backend) event stream (RFC-0009 section 4.12).
//!
//! Drivers and the mock run these checks over the events they emit. The checks
//! are pure functions over a recorded event slice so that they can run in any
//! test harness. Only available with the `testkit` feature or in tests.

use std::collections::BTreeMap;

use crate::backend::{BackendEvent, Outcome, Protocol, TurnRef};

/// A violated contract rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    /// `event_no` did not increase by exactly one for the turn (0009-006).
    EventNoGap {
        /// Turn reference.
        turn: String,
        /// Expected event number.
        expected: u64,
        /// Observed event number.
        found: u64,
    },
    /// A turn-scoped event followed the turn's terminal (0009-007).
    EventAfterTerminal {
        /// Turn reference.
        turn: String,
    },
    /// A turn ended without a terminal event (0009-007).
    MissingTerminal {
        /// Turn reference.
        turn: String,
    },
    /// A reason token was emitted by a driver that does not produce it (0009-072).
    ReasonNotProducible {
        /// Turn reference.
        turn: String,
        /// Reason token text.
        reason: &'static str,
    },
}

/// Checks per-turn ordering, terminal uniqueness and reason producers over `events`.
///
/// A turn that is still open after the slice is not a violation; use
/// [`check_all_turns_closed`] when the scenario must have finished.
pub fn check_turn_stream(events: &[BackendEvent], protocol: Protocol) -> Result<(), Violation> {
    let mut expected: BTreeMap<&str, u64> = BTreeMap::new();
    let mut closed: BTreeMap<&str, bool> = BTreeMap::new();
    for event in events {
        let (Some(turn), Some(event_no)) = (event.turn(), event.event_no()) else {
            continue;
        };
        let name = turn.as_str();
        if closed.get(name).copied().unwrap_or(false) {
            return Err(Violation::EventAfterTerminal {
                turn: name.to_owned(),
            });
        }
        let want = expected.get(name).copied().unwrap_or(1);
        if event_no != want {
            return Err(Violation::EventNoGap {
                turn: name.to_owned(),
                expected: want,
                found: event_no,
            });
        }
        expected.insert(name, want + 1);
        if let BackendEvent::Terminal { outcome, .. } = event {
            closed.insert(name, true);
            let reason = match outcome {
                Outcome::Failed { reason, .. } | Outcome::Refused { reason, .. } => Some(*reason),
                Outcome::Completed | Outcome::Canceled => None,
            };
            if let Some(reason) = reason {
                if !reason.producible_by(protocol) {
                    return Err(Violation::ReasonNotProducible {
                        turn: name.to_owned(),
                        reason: reason.as_str(),
                    });
                }
            }
        }
    }
    Ok(())
}

/// Checks that every turn that appears in `events` ends with exactly one terminal (0009-007).
pub fn check_all_turns_closed(events: &[BackendEvent]) -> Result<(), Violation> {
    let mut closed: BTreeMap<&str, bool> = BTreeMap::new();
    for event in events {
        let Some(turn) = event.turn() else {
            continue;
        };
        let entry = closed.entry(turn.as_str()).or_insert(false);
        if matches!(event, BackendEvent::Terminal { .. }) {
            *entry = true;
        }
    }
    match closed.into_iter().find(|(_, done)| !done) {
        Some((turn, _)) => Err(Violation::MissingTerminal {
            turn: turn.to_owned(),
        }),
        None => Ok(()),
    }
}

/// Returns the turn references seen in `events`, in first-seen order.
pub fn turns_seen(events: &[BackendEvent]) -> Vec<TurnRef> {
    let mut seen: Vec<TurnRef> = Vec::new();
    for turn in events.iter().filter_map(BackendEvent::turn) {
        if !seen.contains(turn) {
            seen.push(turn.clone());
        }
    }
    seen
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::backend::{ReasonToken, StatusPayload};

    fn status(turn: &str, event_no: u64) -> BackendEvent {
        BackendEvent::Status {
            turn: TurnRef::new(turn),
            event_no,
            payload: StatusPayload::Progress("p".to_owned()),
        }
    }

    fn terminal(turn: &str, event_no: u64, outcome: Outcome) -> BackendEvent {
        BackendEvent::Terminal {
            turn: TurnRef::new(turn),
            event_no,
            outcome,
        }
    }

    /// Guards 0009-006: a gap in event numbering is a violation.
    #[test]
    fn gap_in_event_numbers_is_rejected() {
        let events = vec![status("t", 1), status("t", 3)];
        assert_eq!(
            check_turn_stream(&events, Protocol::Acp),
            Err(Violation::EventNoGap {
                turn: "t".to_owned(),
                expected: 2,
                found: 3,
            })
        );
    }

    /// Guards 0009-007: events after the terminal are rejected.
    #[test]
    fn event_after_terminal_is_rejected() {
        let events = vec![terminal("t", 1, Outcome::Completed), status("t", 2)];
        assert!(matches!(
            check_turn_stream(&events, Protocol::Acp),
            Err(Violation::EventAfterTerminal { .. })
        ));
    }

    /// Guards 0009-072: a driver may not emit a reason it does not produce.
    #[test]
    fn foreign_reason_token_is_rejected() {
        let events = vec![terminal(
            "t",
            1,
            Outcome::Refused {
                reason: ReasonToken::RateLimited,
                health_class: None,
            },
        )];
        assert!(matches!(
            check_turn_stream(&events, Protocol::Acp),
            Err(Violation::ReasonNotProducible {
                reason: "RATE_LIMITED",
                ..
            })
        ));
    }

    /// Guards 0009-007: an open turn is reported only by the closed check.
    #[test]
    fn open_turn_fails_closed_check_only() {
        let events = vec![status("t", 1)];
        assert!(check_turn_stream(&events, Protocol::Acp).is_ok());
        assert_eq!(
            check_all_turns_closed(&events),
            Err(Violation::MissingTerminal {
                turn: "t".to_owned()
            })
        );
    }

    /// Guards 0009-006 and 0009-007: independent turns are numbered independently.
    #[test]
    fn turns_are_numbered_independently() {
        let events = vec![
            status("a", 1),
            status("b", 1),
            terminal("a", 2, Outcome::Completed),
            terminal("b", 2, Outcome::Canceled),
        ];
        assert!(check_turn_stream(&events, Protocol::Acp).is_ok());
        assert!(check_all_turns_closed(&events).is_ok());
        assert_eq!(turns_seen(&events).len(), 2);
    }
}
