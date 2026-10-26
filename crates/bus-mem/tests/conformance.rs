//! Runs the RFC-0011 conformance suite against the in-memory binding.

use a2a_conformance::bus_contract::{BusFactory, CHECKS, run_suite};
use a2a_conformance::driver::Clock;
use a2a_conformance::fixtures::Fixtures;
use a2a_conformance::report::{BindingInfo, Conformance, Outcome, Report};
use bus_api::error::BusError;
use bus_mem::{ConfigError, Grant, MemBus, MemConfig, MemControl, StreamConfig, m1_capabilities};
use std::time::Duration;

/// Builds `bus-mem` bindings from the suite fixtures.
#[derive(Debug)]
struct MemFactory;

impl BusFactory for MemFactory {
    type Bus = MemBus;
    type Control = MemControl;

    fn binding(&self) -> BindingInfo {
        BindingInfo {
            name: "bus-mem",
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    fn clock(&self) -> Clock {
        Clock::Virtual
    }

    fn create(
        &self,
        fx: &Fixtures,
        seed: u64,
        withhold: Option<&'static str>,
    ) -> Result<(MemBus, MemControl), BusError> {
        let mut capabilities = m1_capabilities();
        if let Some(flag) = withhold {
            capabilities = capabilities
                .with_flag(flag, false)
                .ok_or(BusError::Internal)?;
        }
        let config = MemConfig {
            seed,
            capabilities,
            streams: fx
                .streams
                .iter()
                .map(|spec| StreamConfig {
                    name: spec.name.clone(),
                    subjects: spec.subjects.clone(),
                    retention_messages: spec.retention,
                })
                .collect(),
            kv_buckets: fx.buckets.clone(),
            grants: fx
                .grants
                .iter()
                .map(|spec| Grant {
                    principal: spec.principal.clone(),
                    tenant: spec.tenant.clone(),
                    publish: spec.publish.clone(),
                    serve: spec.serve.clone(),
                    streams: spec.streams.clone(),
                })
                .collect(),
            ack_wait: fx.ack_wait,
            overload_retry_after: fx.overload_retry_after,
            blob_tenant_quota: fx.blob_quota,
        };
        let bus = MemBus::new(config).map_err(|_| BusError::Internal)?;
        let control = bus.control();
        Ok((bus, control))
    }
}

fn failures(report: &Report) -> Vec<String> {
    report
        .results
        .iter()
        .filter(|result| result.outcome == Outcome::Fail)
        .map(|result| format!("{}: {}", result.id, result.reason))
        .collect()
}

/// [0011-080] the suite runs offline against bus-mem and reports one row per
/// check, with the M1 profile claimed when nothing fails.
#[test]
fn req_0011_080_suite_runs_offline_against_bus_mem() -> Result<(), String> {
    let report = run_suite(&MemFactory, 1)?;
    if report.results.len() != CHECKS.len() {
        return Err(format!(
            "expected {} rows, report has {}",
            CHECKS.len(),
            report.results.len()
        ));
    }
    let failed = failures(&report);
    if !failed.is_empty() {
        return Err(failed.join("\n"));
    }
    match report.conformance() {
        Conformance::Claimed => Ok(()),
        Conformance::Withheld { failed } => Err(format!("claim withheld: {failed:?}")),
        Conformance::Partial { unexercised } => {
            Err(format!("claim partial, unexercised: {unexercised:?}"))
        }
    }
}

/// [0011-081] the report names the profile, binding, declared capability
/// values and seed; two runs from the same seed give identical reports.
#[test]
fn req_0011_081_report_reproducible_from_seed() -> Result<(), String> {
    let first = run_suite(&MemFactory, 7)?;
    let second = run_suite(&MemFactory, 7)?;
    if first.profile != "a2a-bus-m1" {
        return Err(format!("profile {}", first.profile));
    }
    if first.binding.name != "bus-mem" {
        return Err(format!("binding {}", first.binding.name));
    }
    if first.capabilities != m1_capabilities() {
        return Err("report capabilities differ from declared capabilities".to_owned());
    }
    if first.seed != 7 {
        return Err(format!("seed {}", first.seed));
    }
    if first != second {
        return Err("reports from the same seed differ".to_owned());
    }
    Ok(())
}

/// [0011-061] fault injection is deterministic under the seed, and the suite
/// holds across several seeds rather than for one lucky schedule.
#[test]
fn req_0011_061_suite_holds_across_seeds() -> Result<(), String> {
    for seed in [2_u64, 3, 42, 0xA2A] {
        let report = run_suite(&MemFactory, seed)?;
        let failed = failures(&report);
        if !failed.is_empty() {
            return Err(format!("seed {seed}:\n{}", failed.join("\n")));
        }
    }
    Ok(())
}

/// [0011-082] a failed row withholds the conformance claim.
#[test]
fn req_0011_082_failed_row_withholds_claim() -> Result<(), String> {
    let mut report = run_suite(&MemFactory, 1)?;
    let row = report
        .results
        .iter_mut()
        .find(|result| result.id == "0011-010")
        .ok_or("row 0011-010 missing")?;
    row.outcome = Outcome::Fail;
    match report.conformance() {
        Conformance::Withheld { failed } if failed == ["0011-010"] => Ok(()),
        other => Err(format!("expected withheld on 0011-010, got {other:?}")),
    }
}

/// [0011-071] the pending bound is a real limit: a binding whose `max_pending`
/// is zero refuses to start instead of silently accepting no deliveries.
#[test]
fn req_0011_071_zero_max_pending_refused() -> Result<(), String> {
    let mut capabilities = m1_capabilities();
    capabilities.max_pending = 0;
    let config = MemConfig {
        seed: 0,
        capabilities,
        streams: Vec::new(),
        kv_buckets: Vec::new(),
        grants: Vec::new(),
        ack_wait: Duration::from_secs(5),
        overload_retry_after: Duration::from_secs(1),
        blob_tenant_quota: 1,
    };
    match MemBus::new(config) {
        Err(ConfigError::ZeroMaxPending) => Ok(()),
        Err(other) => Err(format!("wrong refusal: {other}")),
        Ok(_) => Err("zero max_pending accepted".to_owned()),
    }
}
