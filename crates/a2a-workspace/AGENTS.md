# a2a-workspace

Owns: workspace isolation for supervisor-run workers (RFC-0012 sec 4.8-4.9): repository
discovery, lease acquisition and release, the lease registry with owner liveness, retention and
garbage collection, and diff artifacts. Layer L2 (RFC-0001 sec 4.2), no dependency on other
workspace crates. Git is driven through the `git` CLI as a child process.
Provides: a synchronous `Workspace` API (`Send + Sync`) with `open`, `acquire`, `release`,
`gc`, `write_artifacts`, and the types `WorkspaceConfig`, `AcquireRequest`, `Lease`,
`ReleaseOutcome`, `GcReport`, `ArtifactSummary`, `WorkspaceError` / `ErrorKind`.
Uses: `sha2` (artifact digest), `sysinfo` (process liveness), `serde` / `serde_json` (registry
index and summaries), `tempfile` (tests only).

## Rules

- Blocking I/O only; callers in async runtimes wrap calls in `spawn_blocking`.
- Refusals (shared checkout, non-git source, path budget, scope/mode mismatch) happen before any
  lock or workspace is created.
- Removal never follows links; a workspace containing links is retained.
- Operator branches and the operator HEAD are never modified.
- Error kinds are stable tokens exposed through `ErrorKind::token()`.

## Requirements guarded by tests

- `tests/lifecycle.rs`: `req_0012_021_*` (live holder blocks, dead holder reclaimed),
  `req_0012_028_*` (session reuse), `req_0012_029_*` (scope/mode), `req_0012_030_*` (worktree
  location), `req_0012_031_*` (shared checkout refused; separate worktrees per session),
  `req_0012_032_*` (base commit, base_ref, operator untouched, existing branch),
  `req_0012_034_*` (copy policy, limits, link rejection), `req_0012_035_*` (clean vs dirty
  release, link safety), `req_0012_036_*` (retention age and byte budget), `req_0012_037_*`
  (Windows path budget), `req_0012_038_*` (config validation), `req_0014_022_*` (non-git refusal
  before any lock).
- `tests/artifacts.rs`: `req_0012_040_*` (diff artifact bytes and untracked block),
  `req_0012_041_*` (patch cap, truncation marker, minimum cap), `req_0012_042_*` (summary has no
  absolute paths).
- Unit tests in `src/`: `req_0012_034_*` (default copy limits), `req_0012_036_*` (default
  retention), `req_0012_037_*` (path length budget, Windows), `req_0012_039_*` (git invocation
  hardening), `req_0012_041_*` (default patch cap), `req_0012_021_*` (pid and start-time owner
  liveness).
- Requirement IDs `req_0012_033_*` (serialization) are the worker's job and are not guarded here.
