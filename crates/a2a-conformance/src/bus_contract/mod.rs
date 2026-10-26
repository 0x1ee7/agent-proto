//! RFC-0011 conformance suite for the `a2a-bus-m1` profile.
//!
//! Every row of RFC-0011 that a runtime check can observe has one entry in
//! [`CHECKS`]. Each check builds a fresh binding through [`BusFactory`], runs
//! against the virtual clock ([`crate::driver`]), and yields a
//! [`CheckResult`]. Rows that cannot be observed at run time (compile-time or
//! governance rules) report `NotApplicable` with the reason.

use std::future::Future;

use bus_api::bus::Bus;
use bus_api::capabilities::Capabilities;
use bus_api::control::BusControl;
use bus_api::error::BusError;
use bus_api::name::{Subject, SubjectPattern};

use crate::driver::drive;
use crate::fixtures::Fixtures;
use crate::report::{BindingInfo, CheckResult, Outcome, Report};

mod errors;
mod faults;
mod kv;
mod messaging;
mod profile;
mod support;
mod tenancy;

/// Creates bindings for the suite.
///
/// `create` must build the fixture topology from `fx` (principals, grants,
/// streams, buckets, `ack_wait`, retention, blob quota) and must start the
/// binding with `seed` driving every random decision. `withhold` names one
/// capability flag to declare `false`, used to test startup refusal.
pub trait BusFactory {
    /// The binding under test.
    type Bus: Bus;
    /// Its virtual clock and fault controls.
    type Control: BusControl;

    /// Name and version reported in the conformance report.
    fn binding(&self) -> BindingInfo;

    /// Starts a fresh binding.
    ///
    /// # Errors
    /// Returns the startup error when the binding refuses to start.
    fn create(
        &self,
        fx: &Fixtures,
        seed: u64,
        withhold: Option<&'static str>,
    ) -> Result<(Self::Bus, Self::Control), BusError>;
}

/// Everything a check needs: the binding, its control, fixtures and seed.
pub struct Ctx<'a, F: BusFactory> {
    /// Factory, for checks that need a second binding with a changed flag.
    pub factory: &'a F,
    /// The binding under test.
    pub bus: &'a F::Bus,
    /// Virtual clock and faults of [`Ctx::bus`].
    pub control: &'a F::Control,
    /// Shared fixture topology.
    pub fx: &'a Fixtures,
    /// Seed the binding was started with.
    pub seed: u64,
}

impl<F: BusFactory> std::fmt::Debug for Ctx<'_, F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ctx")
            .field("seed", &self.seed)
            .finish_non_exhaustive()
    }
}

/// When a check applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// Always run.
    Always,
    /// Run only when every listed capability is declared `true`; otherwise
    /// `NotApplicable` (RFC-0011 [0011-082]).
    Flags(&'static [&'static str]),
    /// Not observable at run time; reported `NotApplicable` with this reason.
    Static(&'static str),
    /// Computed from the other results: a declared flag must be exercised by
    /// at least one non-`NotApplicable` row.
    Coverage,
}

/// One registry entry.
#[derive(Debug, Clone, Copy)]
pub struct CheckSpec {
    /// Requirement id, for example `0011-010`.
    pub id: &'static str,
    /// Short description of the requirement.
    pub title: &'static str,
    /// Applicability.
    pub gate: Gate,
}

/// Every RFC-0011 row, in id order.
pub const CHECKS: &[CheckSpec] = &[
    CheckSpec {
        id: "0011-001",
        title: "operations per profile; no technology types",
        gate: Gate::Static("compile-time: the seam is bus_api::Bus; not observable at run time"),
    },
    CheckSpec {
        id: "0011-002",
        title: "Bus seam trait names per RFC-0001",
        gate: Gate::Static(
            "compile-time: trait names are fixed by bus_api::Bus; signatures are not runtime-checkable",
        ),
    },
    CheckSpec {
        id: "0011-003",
        title: "subjects and headers pass through byte-exact",
        gate: Gate::Flags(&["headers"]),
    },
    CheckSpec {
        id: "0011-010",
        title: "publish acks only after durable store, with stream and seq",
        gate: Gate::Always,
    },
    CheckSpec {
        id: "0011-011",
        title: "request returns one reply, NoResponder, Timeout or error",
        gate: Gate::Flags(&["request_reply"]),
    },
    CheckSpec {
        id: "0011-012",
        title: "serve replies through a single-use handle",
        gate: Gate::Flags(&["request_reply"]),
    },
    CheckSpec {
        id: "0011-013",
        title: "append dedupes repeated msg_id inside the window",
        gate: Gate::Flags(&["durable_streams"]),
    },
    CheckSpec {
        id: "0011-014",
        title: "consume from_seq delivers at or after n, in order",
        gate: Gate::Flags(&["durable_streams", "replay_from_seq"]),
    },
    CheckSpec {
        id: "0011-015",
        title: "delivery settlement: ack, nak, term, ack_wait redelivery",
        gate: Gate::Flags(&["durable_streams"]),
    },
    CheckSpec {
        id: "0011-016",
        title: "KV put/get/create/update/delete and TTL expiry",
        gate: Gate::Flags(&["kv", "kv_ttl"]),
    },
    CheckSpec {
        id: "0011-017",
        title: "blob SHA-256 digest; Corrupt on mismatch",
        gate: Gate::Flags(&["blob_store"]),
    },
    CheckSpec {
        id: "0011-018",
        title: "capabilities stable for the binding lifetime",
        gate: Gate::Always,
    },
    CheckSpec {
        id: "0011-019",
        title: "close leaves unacked deliveries pending",
        gate: Gate::Flags(&["durable_streams"]),
    },
    CheckSpec {
        id: "0011-020",
        title: "at-least-once delivery under duplication",
        gate: Gate::Flags(&["durable_streams"]),
    },
    CheckSpec {
        id: "0011-021",
        title: "same-subject publish order preserved under reordering",
        gate: Gate::Flags(&["durable_streams", "ordered_per_subject"]),
    },
    CheckSpec {
        id: "0011-022",
        title: "sequential publish-acks on one subject stored in order",
        gate: Gate::Flags(&["durable_streams", "ordered_per_subject"]),
    },
    CheckSpec {
        id: "0011-023",
        title: "dedupe covers the window; outside it a second copy is allowed",
        gate: Gate::Flags(&["durable_streams"]),
    },
    CheckSpec {
        id: "0011-024",
        title: "retained from_seq delivers every message; else RETENTION_EXCEEDED",
        gate: Gate::Flags(&["durable_streams", "replay_from_seq"]),
    },
    CheckSpec {
        id: "0011-025",
        title: "no cross-subject ordering guarantee",
        gate: Gate::Static(
            "negative rule: nothing to observe; 0011-021 asserts same-subject order only",
        ),
    },
    CheckSpec {
        id: "0011-026",
        title: "KV watch: snapshot, one SnapshotEnd, then changes",
        gate: Gate::Flags(&["kv", "kv_snapshot_end"]),
    },
    CheckSpec {
        id: "0011-030",
        title: "capability table complete and typed",
        gate: Gate::Always,
    },
    CheckSpec {
        id: "0011-031",
        title: "dedupe_window at least 3630 s",
        gate: Gate::Always,
    },
    CheckSpec {
        id: "0011-032",
        title: "max_message_size at least 262144 and enforced",
        gate: Gate::Always,
    },
    CheckSpec {
        id: "0011-033",
        title: "partition_tolerant claim passes level P",
        gate: Gate::Flags(&["partition_tolerant"]),
    },
    CheckSpec {
        id: "0011-034",
        title: "a2a-bus-m1 profile satisfied; each withheld requirement fails startup",
        gate: Gate::Always,
    },
    CheckSpec {
        id: "0011-035",
        title: "startup refusal names the missing capability",
        gate: Gate::Always,
    },
    CheckSpec {
        id: "0011-036",
        title: "no declared capability without an exercised implementation",
        gate: Gate::Coverage,
    },
    CheckSpec {
        id: "0011-037",
        title: "profile names validated against the registry",
        gate: Gate::Always,
    },
    CheckSpec {
        id: "0011-040",
        title: "every operation carries a principal; anonymous rejected",
        gate: Gate::Always,
    },
    CheckSpec {
        id: "0011-041",
        title: "publish and consume permissions per subject and stream",
        gate: Gate::Flags(&["durable_streams"]),
    },
    CheckSpec {
        id: "0011-042",
        title: "tenant isolation for KV, watch and blobs",
        gate: Gate::Flags(&["kv", "blob_store"]),
    },
    CheckSpec {
        id: "0011-043",
        title: "no planted secret in error text",
        gate: Gate::Always,
    },
    CheckSpec {
        id: "0011-044",
        title: "denied and absent give indistinguishable PermissionDenied",
        gate: Gate::Flags(&["kv", "blob_store"]),
    },
    CheckSpec {
        id: "0011-050",
        title: "every operation returns a typed error variant",
        gate: Gate::Always,
    },
    CheckSpec {
        id: "0011-051",
        title: "Duplicate is success for the append caller",
        gate: Gate::Flags(&["durable_streams"]),
    },
    CheckSpec {
        id: "0011-052",
        title: "Conflict never resolved by overwrite",
        gate: Gate::Flags(&["kv"]),
    },
    CheckSpec {
        id: "0011-053",
        title: "append retry reuses msg_id; stored exactly once",
        gate: Gate::Flags(&["durable_streams"]),
    },
    CheckSpec {
        id: "0011-060",
        title: "every fault switch accepted and observable",
        gate: Gate::Always,
    },
    CheckSpec {
        id: "0011-061",
        title: "fault behaviour deterministic under the seed",
        gate: Gate::Always,
    },
    CheckSpec {
        id: "0011-062",
        title: "under faults, taxonomy and stored content unchanged",
        gate: Gate::Flags(&["durable_streams"]),
    },
    CheckSpec {
        id: "0011-063",
        title: "restart keeps acked appends; unacked redelivered",
        gate: Gate::Flags(&["durable_streams"]),
    },
    CheckSpec {
        id: "0011-064",
        title: "partition: unknown outcome is Timeout or Unavailable, never silent success",
        gate: Gate::Flags(&["durable_streams", "request_reply"]),
    },
    CheckSpec {
        id: "0011-070",
        title: "blob quota enforced with Overloaded; no silent drop",
        gate: Gate::Flags(&["blob_store"]),
    },
    CheckSpec {
        id: "0011-071",
        title: "unacked deliveries bounded by max_pending",
        gate: Gate::Flags(&["durable_streams"]),
    },
    CheckSpec {
        id: "0011-072",
        title: "KV value and blob sizes bounded and reported",
        gate: Gate::Flags(&["kv", "blob_store"]),
    },
    CheckSpec {
        id: "0011-080",
        title: "suite runs offline",
        gate: Gate::Static("meta: the driver performs no I/O; bus-mem tests run the suite offline"),
    },
    CheckSpec {
        id: "0011-081",
        title: "report fields complete and reproducible from seed",
        gate: Gate::Static(
            "meta: reproducibility is asserted by bus-mem tests (run twice, compare)",
        ),
    },
    CheckSpec {
        id: "0011-082",
        title: "no conformance claim when a required row fails",
        gate: Gate::Static(
            "meta: claim withholding is a Report rule, asserted by a2a-conformance unit tests",
        ),
    },
    CheckSpec {
        id: "0011-083",
        title: "report format defined by a2a-conformance",
        gate: Gate::Static("meta: the format is defined in a2a_conformance::report"),
    },
    CheckSpec {
        id: "0011-090",
        title: "non-conforming technology not used for M1",
        gate: Gate::Static("governance: technology selection, not a runtime property"),
    },
    CheckSpec {
        id: "0011-091",
        title: "future binding passes the suite before registration",
        gate: Gate::Static("governance: registration gate, enforced outside the suite"),
    },
];

/// Flags with the RFC-0011 rows that exercise them, for [`Gate::Coverage`].
const FLAG_GUARDS: &[(&str, &[&str])] = &[
    ("headers", &["0011-003"]),
    ("request_reply", &["0011-011", "0011-012"]),
    ("no_responder_signal", &["0011-011"]),
    (
        "durable_streams",
        &["0011-013", "0011-015", "0011-020", "0011-024"],
    ),
    ("replay_from_seq", &["0011-014", "0011-024"]),
    ("ordered_per_subject", &["0011-021", "0011-022"]),
    ("kv", &["0011-016", "0011-026"]),
    ("kv_ttl", &["0011-016"]),
    ("kv_snapshot_end", &["0011-026"]),
    ("blob_store", &["0011-017"]),
    ("partition_tolerant", &["0011-033"]),
];

/// Runs every row against fresh bindings and returns the report.
///
/// # Errors
/// Returns an error when the fixtures or the first binding cannot be built.
pub fn run_suite<F: BusFactory>(factory: &F, seed: u64) -> Result<Report, String> {
    let fx = Fixtures::new()?;
    let (bus, control) = factory
        .create(&fx, seed, None)
        .map_err(|e| format!("binding failed to start: {e:?}"))?;
    let capabilities: Capabilities = bus.capabilities().clone();
    drop(bus);
    drop(control);

    let mut results = Vec::with_capacity(CHECKS.len());
    for spec in CHECKS {
        if spec.gate == Gate::Coverage {
            results.push(CheckResult::new(spec.id, Outcome::Pass, ""));
            continue;
        }
        results.push(run_spec(factory, &fx, seed, spec, &capabilities));
    }
    coverage(&capabilities, &mut results);

    Ok(Report {
        profile: bus_api::capabilities::A2A_BUS_M1.name(),
        binding: factory.binding(),
        capabilities,
        seed,
        results,
    })
}

/// Runs one row by id against a fresh binding.
///
/// # Errors
/// Returns an error for an unknown id or when the binding cannot start.
pub fn run_check<F: BusFactory>(factory: &F, id: &str, seed: u64) -> Result<CheckResult, String> {
    let spec = CHECKS
        .iter()
        .find(|spec| spec.id == id)
        .ok_or_else(|| format!("unknown requirement id {id}"))?;
    let fx = Fixtures::new()?;
    let (bus, _) = factory
        .create(&fx, seed, None)
        .map_err(|e| format!("binding failed to start: {e:?}"))?;
    let capabilities = bus.capabilities().clone();
    drop(bus);
    if spec.gate == Gate::Coverage {
        return Err("0011-036 is computed over a full run; use run_suite".to_owned());
    }
    Ok(run_spec(factory, &fx, seed, spec, &capabilities))
}

fn run_spec<F: BusFactory>(
    factory: &F,
    fx: &Fixtures,
    seed: u64,
    spec: &CheckSpec,
    capabilities: &Capabilities,
) -> CheckResult {
    match spec.gate {
        Gate::Static(reason) => return CheckResult::new(spec.id, Outcome::NotApplicable, reason),
        Gate::Flags(flags) => {
            let missing: Vec<&str> = flags
                .iter()
                .copied()
                .filter(|flag| capabilities.flag(flag) != Some(true))
                .collect();
            if !missing.is_empty() {
                let reason = format!("capability not declared: {}", missing.join(", "));
                return CheckResult::new(spec.id, Outcome::NotApplicable, reason);
            }
        }
        Gate::Always | Gate::Coverage => {}
    }
    let (bus, control) = match factory.create(fx, seed, None) {
        Ok(pair) => pair,
        Err(e) => {
            return CheckResult::new(
                spec.id,
                Outcome::Fail,
                format!("binding failed to start: {e:?}"),
            );
        }
    };
    let ctx = Ctx {
        factory,
        bus: &bus,
        control: &control,
        fx,
        seed,
    };
    match dispatch(spec.id, &ctx) {
        Ok(()) => CheckResult::new(spec.id, Outcome::Pass, spec.title),
        Err(reason) => CheckResult::new(spec.id, Outcome::Fail, reason),
    }
}

fn dispatch<F: BusFactory>(id: &str, ctx: &Ctx<'_, F>) -> Result<(), String> {
    match id {
        "0011-003" => messaging::r0011_003(ctx),
        "0011-010" => messaging::r0011_010(ctx),
        "0011-011" => messaging::r0011_011(ctx),
        "0011-012" => messaging::r0011_012(ctx),
        "0011-013" => messaging::r0011_013(ctx),
        "0011-014" => messaging::r0011_014(ctx),
        "0011-015" => messaging::r0011_015(ctx),
        "0011-016" => kv::r0011_016(ctx),
        "0011-017" => kv::r0011_017(ctx),
        "0011-018" => profile::r0011_018(ctx),
        "0011-019" => messaging::r0011_019(ctx),
        "0011-020" => faults::r0011_020(ctx),
        "0011-021" => faults::r0011_021(ctx),
        "0011-022" => messaging::r0011_022(ctx),
        "0011-023" => messaging::r0011_023(ctx),
        "0011-024" => messaging::r0011_024(ctx),
        "0011-026" => kv::r0011_026(ctx),
        "0011-030" => profile::r0011_030(ctx),
        "0011-031" => profile::r0011_031(ctx),
        "0011-032" => messaging::r0011_032(ctx),
        "0011-033" => faults::r0011_033(ctx),
        "0011-034" => profile::r0011_034(ctx),
        "0011-035" => profile::r0011_035(ctx),
        "0011-037" => profile::r0011_037(ctx),
        "0011-040" => tenancy::r0011_040(ctx),
        "0011-041" => tenancy::r0011_041(ctx),
        "0011-042" => tenancy::r0011_042(ctx),
        "0011-043" => errors::r0011_043(ctx),
        "0011-044" => tenancy::r0011_044(ctx),
        "0011-050" => errors::r0011_050(ctx),
        "0011-051" => errors::r0011_051(ctx),
        "0011-052" => kv::r0011_052(ctx),
        "0011-053" => errors::r0011_053(ctx),
        "0011-060" => faults::r0011_060(ctx),
        "0011-061" => faults::r0011_061(ctx),
        "0011-062" => faults::r0011_062(ctx),
        "0011-063" => faults::r0011_063(ctx),
        "0011-064" => faults::r0011_064(ctx),
        "0011-070" => faults::r0011_070(ctx),
        "0011-071" => faults::r0011_071(ctx),
        "0011-072" => kv::r0011_072(ctx),
        other => Err(format!("no check body for {other}")),
    }
}

/// Applies [`Gate::Coverage`]: a declared flag with no exercised guard row fails.
fn coverage(capabilities: &Capabilities, results: &mut [CheckResult]) {
    let outcomes: Vec<(&'static str, Outcome)> = results
        .iter()
        .map(|result| (result.id, result.outcome))
        .collect();
    let mut failures = Vec::new();
    for (flag, guards) in FLAG_GUARDS {
        if capabilities.flag(flag) != Some(true) {
            continue;
        }
        let exercised = guards.iter().any(|guard| {
            outcomes
                .iter()
                .any(|(id, outcome)| id == guard && *outcome != Outcome::NotApplicable)
        });
        if !exercised {
            failures.push(*flag);
        }
    }
    let Some(result) = results.iter_mut().find(|result| result.id == "0011-036") else {
        return;
    };
    if failures.is_empty() {
        result.outcome = Outcome::Pass;
        result.reason = "every declared capability is exercised by a required row".to_owned();
    } else {
        result.outcome = Outcome::Fail;
        result.reason = format!("declared but not exercised: {}", failures.join(", "));
    }
}

/// Runs `fut` on the binding's virtual clock, failing when it stalls.
pub(crate) fn settle<C, T>(
    control: &C,
    fut: impl Future<Output = Result<T, String>>,
) -> Result<T, String>
where
    C: BusControl + ?Sized,
{
    drive(control, fut).unwrap_or_else(|| {
        Err("operation did not complete within the virtual-time budget".to_owned())
    })
}

/// Maps a binding error into a labelled failure string.
pub(crate) fn bx<T>(what: &str, result: Result<T, BusError>) -> Result<T, String> {
    result.map_err(|e| format!("{what}: unexpected error {e:?}"))
}

/// Requires `result` to fail with a variant accepted by `accept`.
pub(crate) fn expect_variant<T: std::fmt::Debug>(
    what: &str,
    result: Result<T, BusError>,
    accept: impl Fn(&BusError) -> bool,
    wanted: &str,
) -> Result<(), String> {
    match result {
        Err(e) if accept(&e) => Ok(()),
        Err(e) => Err(format!("{what}: expected {wanted}, got {e:?}")),
        Ok(value) => Err(format!("{what}: expected {wanted}, got Ok({value:?})")),
    }
}

/// Requires `PermissionDenied`.
pub(crate) fn expect_denied<T: std::fmt::Debug>(
    what: &str,
    result: Result<T, BusError>,
) -> Result<(), String> {
    expect_variant(
        what,
        result,
        |e| matches!(e, BusError::PermissionDenied),
        "PermissionDenied",
    )
}

/// Parses a subject, labelling the failure.
pub(crate) fn subj(value: &str) -> Result<Subject, String> {
    Subject::parse(value).map_err(|e| format!("subject {value}: {e}"))
}

/// Parses a subject pattern, labelling the failure.
pub(crate) fn pat(value: &str) -> Result<SubjectPattern, String> {
    SubjectPattern::parse(value).map_err(|e| format!("pattern {value}: {e}"))
}
