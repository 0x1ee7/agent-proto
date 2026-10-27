//! In-process runs of the RFC-0014 section 4.2 cases on bus-mem with the real crates.
//!
//! Each case names its RFC-0014 ID and the RFC requirements it exercises. A case is `Pass`,
//! `Fail`, or `NotRun`. `NotRun` carries the reason: the case needs a tier this runner does not
//! have (nats-server, real omp/claude/hermes, the HTTP gateway) or a hook the rig does not expose.
//! Live-tier cases that did run are in-process substitutes with a MockBackend, and each one says so.

mod cases;
mod delegation;
mod gateway;
mod rig;
mod wire;

/// Where a case runs in RFC-0014 section 4.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// bus-mem only.
    Mem,
    /// bus-mem with the nats binding required.
    Nats,
    /// Live backends or the gateway.
    Live,
}

/// Outcome of one case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaseStatus {
    Pass,
    Fail(String),
    NotRun(&'static str),
}

/// Result of one case, with the RFC references it cites.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseResult {
    pub id: &'static str,
    pub title: &'static str,
    pub tier: Tier,
    pub rfc: &'static [&'static str],
    pub status: CaseStatus,
    /// `"in-process"` when the case ran here, or the substitution note. Empty for `NotRun`.
    pub mode: &'static str,
}

/// Results of every case in the table, in table order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScenarioReport {
    cases: Vec<CaseResult>,
}

impl ScenarioReport {
    pub fn cases(&self) -> &[CaseResult] {
        &self.cases
    }

    pub fn case(&self, id: &str) -> Option<&CaseResult> {
        self.cases.iter().find(|c| c.id == id)
    }

    pub fn passed(&self) -> usize {
        self.count(|s| matches!(s, CaseStatus::Pass))
    }

    pub fn failed(&self) -> usize {
        self.count(|s| matches!(s, CaseStatus::Fail(_)))
    }

    pub fn not_run(&self) -> usize {
        self.count(|s| matches!(s, CaseStatus::NotRun(_)))
    }

    fn count(&self, pred: impl Fn(&CaseStatus) -> bool) -> usize {
        self.cases.iter().filter(|c| pred(&c.status)).count()
    }
}

const IN_PROCESS: &str = "in-process: MockBackend on bus-mem, real client/worker/bus crates";
const SUBSTITUTE: &str =
    "in-process substitute of the live tier: MockBackend, no real omp/claude/hermes process";

enum Run {
    Exec(fn() -> Result<(), String>),
    NotRun(&'static str),
}

struct Case {
    id: &'static str,
    title: &'static str,
    tier: Tier,
    rfc: &'static [&'static str],
    mode: &'static str,
    run: Run,
}

const NO_LIVE_BACKENDS: &str =
    "needs real omp/claude/hermes processes; the runner uses MockBackend only";
const NO_NATS: &str = "needs a running nats-server; the runner is bus-mem only";
const RESTART_SUBSTITUTE: &str = "in-process substitute for the nats tier: MockBackend on bus-mem; restart_bus stands in for killing and restarting nats-server";
const NO_GATEWAY: &str =
    "needs live omp and Hermes behind the HTTP gateway; the runner has no live backends";
const NO_PY_SDK: &str =
    "needs the pinned Python a2a-sdk client, which is not part of this workspace";
const NO_SUPERVISOR: &str =
    "needs a2ad running real child processes and an OS-level kill; the runner does not start a2ad";
const GATEWAY_SUBSTITUTE: &str = "in-process substitute: the gateway router on bus-mem, driven with tower::oneshot from a non-loopback source; no socket and no Python SDK";

/// Every RFC-0014 section 4.2 case, then the extra negatives named in the milestone assignment.
fn table() -> Vec<Case> {
    vec![
        Case {
            id: "0014-010",
            title: "S1: omp asks Claude Code to review a file",
            tier: Tier::Live,
            rfc: &["0014-010", "0015"],
            mode: SUBSTITUTE,
            run: Run::Exec(delegation::s1_delegation_round_trip),
        },
        Case {
            id: "0014-011",
            title: "S2 Gateway streaming end to end",
            tier: Tier::Live,
            rfc: &["0014-011", "0009"],
            mode: "",
            run: Run::NotRun(NO_GATEWAY),
        },
        Case {
            id: "0014-012",
            title: "S3 Hermes backend task",
            tier: Tier::Live,
            rfc: &["0014-012"],
            mode: "",
            run: Run::NotRun(NO_LIVE_BACKENDS),
        },
        Case {
            id: "0014-013",
            title: "S4a Operator approves a write",
            tier: Tier::Live,
            rfc: &["0014-013", "0006-020"],
            mode: SUBSTITUTE,
            run: Run::Exec(cases::s4a_operator_approves),
        },
        Case {
            id: "0014-014",
            title: "S4b Operator denies a write",
            tier: Tier::Live,
            rfc: &["0014-014", "0006-019"],
            mode: SUBSTITUTE,
            run: Run::Exec(cases::s4b_operator_denies),
        },
        Case {
            id: "0014-015",
            title: "S4c Approval expires",
            tier: Tier::Live,
            rfc: &["0014-015", "0006-017"],
            mode: SUBSTITUTE,
            run: Run::Exec(cases::s4c_approval_expires),
        },
        Case {
            id: "0014-016",
            title: "S5: two agents delegate back and forth",
            tier: Tier::Mem,
            rfc: &["0014-016", "0015"],
            mode: IN_PROCESS,
            run: Run::Exec(delegation::s5_loop_detected),
        },
        Case {
            id: "0014-017",
            title: "S6: kill the bus mid-task and restart",
            tier: Tier::Nats,
            rfc: &["0014-017", "0004"],
            mode: RESTART_SUBSTITUTE,
            run: Run::Exec(cases::s6_bus_restart),
        },
        Case {
            id: "0014-018",
            title: "S7: duplicate submission delivery",
            tier: Tier::Mem,
            rfc: &["0014-018", "0002-031"],
            mode: IN_PROCESS,
            run: Run::Exec(cases::s7_duplicate_submission),
        },
        Case {
            id: "0014-019",
            title: "S8 Cancel a long task",
            tier: Tier::Live,
            rfc: &["0014-019", "0005-037"],
            mode: SUBSTITUTE,
            run: Run::Exec(cases::s8_cancel_long_task),
        },
        Case {
            id: "0014-020",
            title: "S9: stock a2a-sdk client uses omp through the gateway",
            tier: Tier::Live,
            rfc: &["0014-020"],
            mode: "",
            run: Run::NotRun(NO_PY_SDK),
        },
        Case {
            id: "0014-021",
            title: "N1 Publish without a grant",
            tier: Tier::Nats,
            rfc: &["0014-021", "0011-044"],
            mode: "in-process substitute on bus-mem; the nats tier was not run",
            run: Run::Exec(cases::n1_publish_without_grant),
        },
        Case {
            id: "0014-022",
            title: "N2 Shared workspace checkout",
            tier: Tier::Live,
            rfc: &["0014-022"],
            mode: "",
            run: Run::NotRun("needs live coding backends on a real workspace checkout"),
        },
        Case {
            id: "0014-023",
            title: "N3: unauthenticated gateway request",
            tier: Tier::Live,
            rfc: &["0014-023"],
            mode: GATEWAY_SUBSTITUTE,
            run: Run::Exec(gateway::n3_unauthenticated_refused),
        },
        Case {
            id: "0014-024",
            title: "N4: unsupported operation on the bus",
            tier: Tier::Mem,
            rfc: &["0014-024", "0002-023"],
            mode: IN_PROCESS,
            run: Run::Exec(cases::n4_unsupported_operation),
        },
        Case {
            id: "0014-025",
            title: "N5: stale request",
            tier: Tier::Mem,
            rfc: &["0014-025", "0002-034", "0002-072"],
            mode: IN_PROCESS,
            run: Run::Exec(cases::n5_stale_request),
        },
        Case {
            id: "0014-026",
            title: "N6: duplicate agent id",
            tier: Tier::Nats,
            rfc: &["0014-026", "0004-025"],
            mode: "",
            run: Run::NotRun(NO_NATS),
        },
        Case {
            id: "0014-027",
            title: "N7: card-driven mode escalation",
            tier: Tier::Live,
            rfc: &["0014-027", "0009-064", "0009-065"],
            mode: "",
            run: Run::NotRun(NO_LIVE_BACKENDS),
        },
        Case {
            id: "0014-028",
            title: "N8: supervisor killed",
            tier: Tier::Live,
            rfc: &["0014-028", "0012-018", "0012-020"],
            mode: "",
            run: Run::NotRun(NO_SUPERVISOR),
        },
        Case {
            id: "0014-029",
            title: "N9: worker crash",
            tier: Tier::Live,
            rfc: &["0014-029", "0012-011", "0012-013"],
            mode: "",
            run: Run::NotRun(NO_SUPERVISOR),
        },
        Case {
            id: "N-OVERSIZE",
            title: "Payload above the inline limit is refused",
            tier: Tier::Mem,
            rfc: &["0002-113"],
            mode: IN_PROCESS,
            run: Run::Exec(cases::oversize_payload_refused),
        },
        Case {
            id: "N-UNKNOWN-PEER",
            title: "Request to an unserved agent is refused",
            tier: Tier::Mem,
            rfc: &["0002-093"],
            mode: IN_PROCESS,
            run: Run::Exec(cases::unknown_peer_refused),
        },
        Case {
            id: "N-VERSION",
            title: "Unsupported protocol version is refused",
            tier: Tier::Mem,
            rfc: &["0002-080"],
            mode: IN_PROCESS,
            run: Run::Exec(cases::n_version_refused),
        },
        Case {
            id: "N-EXTENSION",
            title: "Required extension missing is refused",
            tier: Tier::Mem,
            rfc: &["0002-081"],
            mode: IN_PROCESS,
            run: Run::Exec(cases::n_extension_required),
        },
        Case {
            id: "N-SIGNATURE",
            title: "Bad message signature is refused",
            tier: Tier::Mem,
            rfc: &["0007"],
            mode: "",
            run: Run::NotRun(
                "message signing and verification are disabled in M1 (RFC-0007); a2a-client Config.verify is None, so there is no verification path to exercise",
            ),
        },
        Case {
            id: "N-QUEUE-FULL",
            title: "Same-context queue over quota is refused",
            tier: Tier::Mem,
            rfc: &["0005-028"],
            mode: IN_PROCESS,
            run: Run::Exec(cases::n_queue_full_rejected),
        },
    ]
}

/// Runs every case in the table and returns the report. Never panics.
pub fn run_all() -> ScenarioReport {
    let cases = table()
        .into_iter()
        .map(|case| {
            let (status, mode) = match case.run {
                Run::Exec(f) => match f() {
                    Ok(()) => (CaseStatus::Pass, case.mode),
                    Err(why) => (CaseStatus::Fail(why), case.mode),
                },
                Run::NotRun(reason) => (CaseStatus::NotRun(reason), ""),
            };
            CaseResult {
                id: case.id,
                title: case.title,
                tier: case.tier,
                rfc: case.rfc,
                status,
                mode,
            }
        })
        .collect();
    ScenarioReport { cases }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// Every RFC-0014 section 4.2 case ID (S1..S9, N1..N9).
    const SECTION_4_2_IDS: [&str; 20] = [
        "0014-010", "0014-011", "0014-012", "0014-013", "0014-014", "0014-015", "0014-016",
        "0014-017", "0014-018", "0014-019", "0014-020", "0014-021", "0014-022", "0014-023",
        "0014-024", "0014-025", "0014-026", "0014-027", "0014-028", "0014-029",
    ];

    fn run_case(id: &str) -> CaseStatus {
        let report = run_all();
        report.case(id).expect("case in table").status.clone()
    }

    #[test]
    fn req_0014_report_lists_every_section_4_2_case() {
        let report = run_all();
        for id in SECTION_4_2_IDS {
            assert!(report.case(id).is_some(), "missing RFC-0014 case {id}");
        }
        for id in [
            "N-OVERSIZE",
            "N-UNKNOWN-PEER",
            "N-VERSION",
            "N-EXTENSION",
            "N-SIGNATURE",
            "N-QUEUE-FULL",
        ] {
            assert!(report.case(id).is_some(), "missing extra negative {id}");
        }
        assert_eq!(report.cases().len(), 26);
    }

    #[test]
    fn req_0014_no_case_fails() {
        let report = run_all();
        let failures: Vec<_> = report
            .cases()
            .iter()
            .filter_map(|c| match &c.status {
                CaseStatus::Fail(why) => Some(format!("{}: {why}", c.id)),
                _ => None,
            })
            .collect();
        assert!(failures.is_empty(), "failed cases: {failures:?}");
    }

    #[test]
    fn req_0014_s4a_operator_approves_passes() {
        assert_eq!(run_case("0014-013"), CaseStatus::Pass);
    }

    #[test]
    fn req_0014_s4b_operator_denies_passes() {
        assert_eq!(run_case("0014-014"), CaseStatus::Pass);
    }

    #[test]
    fn req_0014_s4c_approval_expires_passes() {
        assert_eq!(run_case("0014-015"), CaseStatus::Pass);
    }

    #[test]
    fn req_0014_010_delegation_round_trip_passes() {
        assert_eq!(run_case("0014-010"), CaseStatus::Pass);
    }

    #[test]
    fn req_0014_016_loop_detected_passes() {
        assert_eq!(run_case("0014-016"), CaseStatus::Pass);
    }

    #[test]
    fn req_0014_017_bus_restart_passes() {
        assert_eq!(run_case("0014-017"), CaseStatus::Pass);
    }

    #[test]
    fn req_0014_s7_duplicate_submission_passes() {
        assert_eq!(run_case("0014-018"), CaseStatus::Pass);
    }

    #[test]
    fn req_0014_s8_cancel_passes() {
        assert_eq!(run_case("0014-019"), CaseStatus::Pass);
    }

    #[test]
    fn req_0014_n1_publish_without_grant_refused() {
        assert_eq!(run_case("0014-021"), CaseStatus::Pass);
    }

    #[test]
    fn req_0002_113_oversize_payload_refused() {
        assert_eq!(run_case("N-OVERSIZE"), CaseStatus::Pass);
    }

    #[test]
    fn req_0002_093_unknown_peer_refused() {
        assert_eq!(run_case("N-UNKNOWN-PEER"), CaseStatus::Pass);
    }

    #[test]
    fn req_0014_023_unauthenticated_refused_passes() {
        assert_eq!(run_case("0014-023"), CaseStatus::Pass);
    }

    #[test]
    fn req_0014_024_unsupported_operation_passes() {
        assert_eq!(run_case("0014-024"), CaseStatus::Pass);
    }

    #[test]
    fn req_0014_025_stale_request_passes() {
        assert_eq!(run_case("0014-025"), CaseStatus::Pass);
    }

    #[test]
    fn req_0002_080_unsupported_version_passes() {
        assert_eq!(run_case("N-VERSION"), CaseStatus::Pass);
    }

    #[test]
    fn req_0002_081_required_extension_missing_passes() {
        assert_eq!(run_case("N-EXTENSION"), CaseStatus::Pass);
    }

    #[test]
    fn req_0005_028_queue_full_passes() {
        assert_eq!(run_case("N-QUEUE-FULL"), CaseStatus::Pass);
    }

    #[test]
    fn req_0014_live_cases_name_their_substitution() {
        let report = run_all();
        for c in report.cases() {
            if c.status == CaseStatus::Pass && c.tier == Tier::Live {
                assert!(
                    c.mode.starts_with("in-process substitute"),
                    "{} ran live-tier without naming the substitute",
                    c.id
                );
            }
        }
    }

    #[test]
    fn req_0014_not_run_cases_carry_a_reason() {
        let report = run_all();
        for c in report.cases() {
            if let CaseStatus::NotRun(reason) = c.status {
                assert!(!reason.is_empty(), "{} has an empty NotRun reason", c.id);
            }
        }
    }
}
