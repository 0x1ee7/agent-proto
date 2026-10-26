//! Profile rows: capability stability, typed table, M1 requirements, registry.

use bus_api::bus::Bus;
use bus_api::capabilities::{A2A_BUS_M1, FLAG_NAMES, M1_MIN_DEDUPE_WINDOW, Profile, ProfileError};
use bytes::Bytes;

use super::support::put;
use super::{BusFactory, Ctx, bx, settle};

/// Capability flags that the M1 profile requires; each must be checked by 0011-034.
const M1_REQUIRED_FLAGS: &[&str] = &[
    "headers",
    "request_reply",
    "no_responder_signal",
    "durable_streams",
    "replay_from_seq",
    "ordered_per_subject",
    "kv",
    "kv_ttl",
    "kv_snapshot_end",
    "blob_store",
];

/// [0011-018] capabilities do not change over the life of the process.
pub(super) fn r0011_018<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    let fx = ctx.fx;
    let bus = ctx.bus;
    let before = bus.capabilities().clone();
    settle(ctx.control, async {
        put(bus, &fx.client, "suite.cap.a", b"x").await?;
        bx(
            "blob_put",
            bus.blob_put(&fx.client, &fx.local, Bytes::from_static(b"cap"))
                .await,
        )?;
        Ok::<(), String>(())
    })?;
    ctx.control.restart_bus()?;
    if bus.capabilities() != &before {
        return Err("capabilities changed after operations and restart".to_owned());
    }
    Ok(())
}

/// [0011-030] the capability table is complete and typed.
pub(super) fn r0011_030<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    let caps = ctx.bus.capabilities();
    for name in FLAG_NAMES {
        if caps.flag(name).is_none() {
            return Err(format!("capability flag {name} missing from the table"));
        }
    }
    if caps.dedupe_window.is_zero() {
        return Err("dedupe_window is zero".to_owned());
    }
    for (name, value) in [
        ("max_message_size", caps.max_message_size),
        ("max_pending", caps.max_pending),
        ("kv_max_value_size", caps.kv_max_value_size),
        ("blob_max_size", caps.blob_max_size),
    ] {
        if value == 0 {
            return Err(format!("numeric capability {name} is zero"));
        }
    }
    Ok(())
}

/// [0011-031] dedupe window at least the profile minimum.
pub(super) fn r0011_031<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    let window = ctx.bus.capabilities().dedupe_window;
    if window < M1_MIN_DEDUPE_WINDOW {
        return Err(format!(
            "dedupe_window {window:?} below the M1 minimum {M1_MIN_DEDUPE_WINDOW:?}"
        ));
    }
    Ok(())
}

/// [0011-034] the M1 profile is satisfied by the declared capabilities, and
/// withholding any required flag makes the profile check name that flag.
pub(super) fn r0011_034<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    A2A_BUS_M1
        .check(ctx.bus.capabilities())
        .map_err(|e| format!("declared capabilities fail the M1 profile: {e}"))?;
    for flag in M1_REQUIRED_FLAGS {
        let (bus, _control) = bx(
            "create withheld",
            ctx.factory.create(ctx.fx, ctx.seed, Some(flag)),
        )?;
        match A2A_BUS_M1.check(bus.capabilities()) {
            Err(ProfileError::Unsatisfied { missing, .. }) if missing.contains(flag) => {}
            other => {
                return Err(format!(
                    "withholding {flag}: expected Unsatisfied naming it, got {other:?}"
                ));
            }
        }
    }
    Ok(())
}

/// [0011-035] startup refuses a missing capability, names it, and gives no
/// degraded mode.
pub(super) fn r0011_035<F: BusFactory>(ctx: &Ctx<'_, F>) -> Result<(), String> {
    let flag = "kv_ttl";
    let (bus, _control) = bx(
        "create withheld",
        ctx.factory.create(ctx.fx, ctx.seed, Some(flag)),
    )?;
    match A2A_BUS_M1.check(bus.capabilities()) {
        Err(err @ ProfileError::Unsatisfied { .. }) => {
            let text = err.to_string();
            if !text.contains(flag) {
                return Err(format!("refusal message does not name {flag}: {text}"));
            }
            Ok(())
        }
        Err(other) => Err(format!("unexpected refusal: {other}")),
        Ok(()) => Err(format!("profile accepted a binding without {flag}")),
    }
}

/// [0011-037] profile names are validated against the registry.
pub(super) fn r0011_037<F: BusFactory>(_ctx: &Ctx<'_, F>) -> Result<(), String> {
    let found: &Profile = match Profile::by_name("a2a-bus-m1") {
        Ok(profile) => profile,
        Err(e) => return Err(format!("registered profile not found: {e}")),
    };
    if found.name() != A2A_BUS_M1.name() {
        return Err("registry returned a different profile for a2a-bus-m1".to_owned());
    }
    match Profile::by_name("a2a-bus-m9") {
        Err(ProfileError::UnknownProfile { name }) if name == "a2a-bus-m9" => Ok(()),
        other => Err(format!(
            "unknown profile name: expected UnknownProfile, got {other:?}"
        )),
    }
}
