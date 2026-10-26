# a2a-conformance

Owns: the RFC-0011 conformance suite. It is generic over a `BusFactory`, so any
binding can run it, and it defines the report format and claim rule.

Provides:
- `bus_contract::{BusFactory, Ctx, Gate, CheckSpec, CHECKS, run_suite, run_check}`.
  `BusFactory::clock()` selects the clock. `CHECKS` has one row per RFC-0011
  requirement ID in order.
- `fixtures::{Fixtures, StreamSpec, GrantSpec}`: the topology every check uses
  (two tenants, a client, a server, an outsider, a denied principal, streams,
  buckets). `Fixtures::new(clock)` scales `ACK_WAIT` and `OVERLOAD_RETRY_AFTER`
  through the clock; checks wrap nominal durations with `Fixtures::scaled`.
- `driver::{Clock, Timeline, drive, ready_now, join, STEP, BUDGET_STEPS, REAL_MIN_STEP,
  UNSUPPORTED}`. `Clock::Virtual` moves the binding's virtual clock through
  `BusControl::advance` and needs no sleeping. `Clock::Real { scale }` waits on the
  wall clock; nominal durations are multiplied by `scale` in (0, 1]. `Timeline` is the
  only path to time and fault controls; each control call returns `Result<_, String>`.
  A future that cannot finish within the budget is a stall, reported as a failure.
- `report::{Report, CheckResult, Outcome, Conformance, BindingInfo}`: `Report`
  implements `Display`; `Report::conformance()` applies the claim rule.
  `Conformance::Partial { unexercised }` means no row failed but some could not run.

Uses: `bus-api` (seam, `BusControl`, profile check), `a2a-types`, `bytes`, `sha2`.

## Rules

- A row whose capability is undeclared is `NotApplicable` with the reason
  `capability not declared: <names>`. A control operation the binding does not
  provide (`BusError::Unsupported`) is `Unexercised`, printed `N/A` with the reason
  `unsupported: <op>`; it is neither a pass nor a failure. Any other failed row is `Fail`.
- Static and meta rows (0011-001, 002, 025, 080 to 083, 090, 091) are `NotApplicable`
  with a reason. Nothing at run time observes them.
- The suite creates the bus through `BusFactory::create`. `withhold` names one flag to
  report as undeclared; the profile check must then fail naming that flag (0011-034,
  0011-035).

## Tests and the requirements they guard

The check bodies live in `bus_contract::{messaging, kv, profile, faults, tenancy,
errors}`. They are exercised by the integration test in `crates/bus-mem/tests/conformance.rs`
through the `bus-mem` binding (dev-dependency). Requirement IDs guarded there:

- Messaging: 0011-003, 010, 011, 012, 013, 014, 015, 019, 022, 023, 024, 032.
- KV and blobs: 0011-016, 017, 026, 052, 072.
- Profile and capabilities: 0011-018, 030, 031, 034, 035, 036, 037.
- Faults and restart: 0011-020, 021, 033, 060, 061, 062, 063, 064, 070, 071.
- Tenancy: 0011-040 (its body also asserts that a forged `A2AB-Sender` header or payload
  cannot change the request or delivery principal, RFC-0002 [0002-011]), 041, 042, 044.
- Errors: 0011-043, 050, 051, 053.

Known gaps, which the rows state as limits of what the suite can observe:
- 0011-010 cannot inject a kill between store and ack.
- 0011-012 single use is enforced by type, so the check covers only the runtime path.
- 0011-043 canary check inspects error text only.
- 0011-050 cannot trigger `DuplicateInProgress` or `Internal` from outside.
