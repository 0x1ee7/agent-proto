//! Conformance report (RFC-0011 [0011-081], [0011-082], [0011-083]).
//!
//! A [`Report`] lists every row with its outcome and reason, plus the profile,
//! binding identity, declared capability values and seed. Its
//! [`Report::conformance`] applies the claim rule: any failed row withholds the
//! claim. Rows that are not applicable (capability undeclared) do not block it.

use std::fmt::{self, Display, Formatter};

use bus_api::capabilities::Capabilities;

/// Result of one row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The row's behaviour was observed as required.
    Pass,
    /// The row's behaviour was observed as violated, or the check stalled.
    Fail,
    /// The row does not apply: a capability is undeclared, or the row is not
    /// observable at run time (the reason says which).
    NotApplicable,
}

impl Outcome {
    /// Upper-case label used in the text report.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Fail => "FAIL",
            Self::NotApplicable => "N/A",
        }
    }
}

/// Result of one row with its reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    /// Requirement id, for example `0011-010`.
    pub id: &'static str,
    /// Outcome.
    pub outcome: Outcome,
    /// Human-readable reason: what was observed, or why the row does not apply.
    pub reason: String,
}

impl CheckResult {
    /// Builds a result.
    pub fn new(id: &'static str, outcome: Outcome, reason: impl Into<String>) -> Self {
        Self {
            id,
            outcome,
            reason: reason.into(),
        }
    }
}

/// Identity of the binding under test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingInfo {
    /// Binding name, for example `bus-mem`.
    pub name: &'static str,
    /// Binding version.
    pub version: &'static str,
}

/// Claim status derived from a report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Conformance {
    /// Every applicable row passed.
    Claimed,
    /// At least one row failed; lists the failed ids.
    Withheld {
        /// Ids of failed rows, in report order.
        failed: Vec<&'static str>,
    },
}

/// A complete suite run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// Profile name, `a2a-bus-m1`.
    pub profile: &'static str,
    /// Binding identity.
    pub binding: BindingInfo,
    /// Declared capability values.
    pub capabilities: Capabilities,
    /// Seed the run used.
    pub seed: u64,
    /// One entry per row, in id order.
    pub results: Vec<CheckResult>,
}

impl Report {
    /// Applies the claim rule: any failed row withholds the claim.
    #[must_use]
    pub fn conformance(&self) -> Conformance {
        let failed: Vec<&'static str> = self
            .results
            .iter()
            .filter(|result| result.outcome == Outcome::Fail)
            .map(|result| result.id)
            .collect();
        if failed.is_empty() {
            Conformance::Claimed
        } else {
            Conformance::Withheld { failed }
        }
    }
}

impl Display for Report {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        writeln!(f, "profile: {}", self.profile)?;
        writeln!(f, "binding: {} {}", self.binding.name, self.binding.version)?;
        writeln!(f, "seed: {}", self.seed)?;
        writeln!(f, "capabilities: {:?}", self.capabilities)?;
        for result in &self.results {
            writeln!(
                f,
                "{} {} {}",
                result.id,
                result.outcome.label(),
                result.reason
            )?;
        }
        match self.conformance() {
            Conformance::Claimed => writeln!(f, "conformance: claimed"),
            Conformance::Withheld { failed } => {
                writeln!(f, "conformance: withheld (failed: {})", failed.join(", "))
            }
        }
    }
}
