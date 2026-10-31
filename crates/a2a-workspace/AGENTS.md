# a2a-workspace

Owns: workspace isolation for supervisor-run workers (RFC-0012 sec 4.8-4.9): repository
discovery, lease acquisition and release, the lease registry with owner liveness, retention and
garbage collection, diff artifacts, and copy-mode exclusion rules. Layer L2 (RFC-0001 sec 4.2), no
dependency on other workspace crates. Git is driven through the `git` CLI as a child process.
Provides: a synchronous `Workspace` API (`Send + Sync`) with `open`, `acquire`, `release`,
`gc`, `write_artifacts`, and the types `WorkspaceConfig`, `AcquireRequest`, `Lease` (with
`excluded_count()`), `ReleaseOutcome`, `GcReport`, `ArtifactSummary`, `WorkspaceError` /
`ErrorKind` (`ErrorKind::ALL`, `ErrorKind::token()`).
Uses: `globset` (copy exclusion patterns), `sha2` (artifact digest), `sysinfo` (process liveness),
`serde` / `serde_json` (registry index and summaries). Dev-only: `a2a-types` (token parity test),
`tempfile`.

## Rules

- Blocking I/O only; callers in async runtimes wrap calls in `spawn_blocking`.
- Refusals (shared checkout, non-git source, path budget, scope/mode mismatch) happen before any
  lock or workspace is created.
- Removal never follows links; a workspace containing links is retained.
- Operator branches and the operator HEAD are never modified.
- Error kinds are stable tokens exposed through `ErrorKind::token()`; `ErrorKind::ALL` lists all 14
  RFC-0012 tokens and must equal the `a2a-types` wire vocabulary.
- Copy mode never stages or copies git-ignored files; `git add` never uses `-f`.
- Copy mode never copies entries matched by the built-in denylist or by `copy_exclude`. Only the
  count is kept (`excluded_count`); names are never recorded.
- The directory and branch name is the 48-bit short key. A live lease is reused only when the
  canonical key is equal; a different canonical key with the same short key is refused with
  `shared_checkout` and never shares the workspace (a UUIDv7 short key is its millisecond
  timestamp, so two keys created in one millisecond collide).

## Requirements guarded by tests

- `tests/lifecycle.rs`: `req_0012_021_*` (live holder blocks, dead holder reclaimed),
  `req_0012_028_*` (session reuse), `req_0012_029_*` (scope/mode), `req_0012_030_*` (worktree
  location), `req_0012_031_*` (shared checkout refused; separate worktrees per session),
  `req_0012_032_*` (base commit, base_ref, operator untouched, existing branch),
  `req_0012_034_*` (copy policy, limits, link rejection), `req_0012_035_*` (clean vs dirty
  release, link safety), `req_0012_036_*` (retention age and byte budget), `req_0012_037_*`
  (Windows path budget), `req_0012_038_*` (config validation), `req_0014_022_*` (non-git refusal
  before any lock).
- `tests/copy_exclusion.rs`: `req_0012_073_*` (git-ignored files absent from copy and baseline;
  nested `.gitignore`; ignored `.env` absent from a git worktree), `req_0012_074_*` (denylist,
  excluded directories counted per file, `copy_exclude` name and path patterns, invalid pattern is
  `config`, excluded entries do not count toward limits, counts recorded without names).
- `tests/tokens.rs`: `req_0012_067_*` (`ErrorKind` tokens equal the `a2a-types` table).
- `tests/artifacts.rs`: `req_0012_040_*` (diff artifact bytes and untracked block),
  `req_0012_041_*` (patch cap, truncation marker, minimum cap), `req_0012_042_*` (summary has no
  absolute paths).
- Unit tests in `src/`: `req_0012_034_*` (default copy limits), `req_0012_036_*` (default
  retention), `req_0012_037_*` (path length budget, Windows), `req_0012_039_*` (git invocation
  hardening), `req_0012_041_*` (default patch cap), `req_0012_021_*` (pid and start-time owner
  liveness), `req_0012_074_*` (exclusion matching, case-insensitive, `copy_exclude` validation).
- Requirement IDs `req_0012_033_*` (serialization) are the worker's job and are not guarded here.
- `tests/key_collision.rs`: `req_0012_031_*` (short-key collision refused, original holder
  unaffected).
