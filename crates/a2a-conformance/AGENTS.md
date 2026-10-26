# a2a-conformance

Owns: the RFC-0011 conformance suite. It is generic over a `BusFactory`, so any
binding can run it, and it defines the report format and claim rule.

Provides:
- `bus_contract::{BusFactory, Ctx, Gate, CheckSpec, CHECKS, run_suite, run_check}`.
  `CHECKS` has one row per RFC-0011 requirement ID in order.
- `fixtures::{Fixtures, StreamSpec, GrantSpec}`: the topology every check uses
  (two tenants, a client, a server, an outsider, a denied principal, streams,
  buckets).
- `driver::{drive, ready_now, join, STEP, BUDGET_STEPS}`: runs futures on the
  binding's virtual clock. A future that cannot finish within the budget is a stall,
  reported as a failure.
- `report::{Report, CheckResult, Outcome, Conformance, BindingInfo}`: `Report`
  implements `Display`; `Report::conformance()` applies the claim rule.

Uses: `bus-api` (seam, `BusControl`, profile check), `a2a-types`, `bytes`, `sha2`.

## Rules

- A row whose capability is undeclared is `NotApplicable` with the reason
  `capability not declared: <names>`. Any other failed row is `Fail`.
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
