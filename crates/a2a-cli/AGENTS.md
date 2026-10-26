# a2a-cli

Owns: the operator command-line surface (layer L5, RFC-0012 4.11): the `a2a` binary and the
library behind it. Commands cover discovery, send, watch, cancel, approvals, audit verification,
and config resolution. Output is proto-JSON with `--json` and human text otherwise.

Provides:
- `cli`: clap definitions (`Cli`, `Command`, argument structs, `Scope`).
- `config`: `Settings`, `Overrides`, file parsing, precedence (flag, env, file, default).
- `commands`: `Session<B: Bus>`, `send`, `discover`, `watch`, `cancel`, and print helpers.
- `approvals`: `list`, `decide`; confirmation card and `--yes` rules.
- `audit`: `verify_files`, trust checkpoints, exit mapping.
- `card`: discovery card rendering.
- `output`: `Io` streams, color and VT detection.
- `app`: runtime-independent dispatch (`build_session`, `offline`, `dispatch`).
- `error`: `Exit` codes (0 to 6) and `CliError`, with `classify` for client errors.

Uses: `a2a-client` (requester runtime, signing hook), `a2a-audit` (chain verification),
`a2a-core` (subjects, envelopes, approval constants), `a2a-security` (signer loading, trust store),
`a2a-types` (ids, proto), `bus-api` (`Bus`), `bus-nats` (only in `src/main.rs`), `clap`, `serde`,
`serde_json`, `thiserror`, `toml`, `tokio`. Dev-dependencies: `bus-mem`, `pbjson-types`, `tokio`.

## Rules

- Layer rule [0001-001]: only `src/main.rs` names a concrete bus (`bus_nats::connect`). Every other
  module is generic over `bus_api::bus::Bus`. Tests run on `bus-mem` with a fake worker.
- Validation runs before bus traffic: invalid agent ids are usage errors (exit 2) and send nothing
  ([0012-054]).
- Exit codes are fixed: 0 success, 1 failure, 2 usage, 3 not found, 4 rejected, 5 timeout,
  6 unavailable ([0012-055]). Client errors are classified by cause in `error::classify`.
- `--json` output is proto-JSON (camelCase fields, enum names). Human text is never parsed by tools
  ([0012-056]).
- Approval decisions show the request card on stderr before any prompt. Without a terminal, a
  decision needs `--yes`; otherwise nothing is decided ([0012-057]).
- Decisions are sent only through an operator signing hook. The signer is loaded only for approve
  and deny, so read-only commands work without a key. An unsigned session never decides
  ([0012-058]).
- The crate never reads or names provider API keys ([0012-058]).
- Color is ANSI only on a terminal, with `NO_COLOR` unset. There is no color flag ([0012-059]).
- Audit verification exits 0 on a clean chain and 1 on the first broken line, naming that line
  ([0013-028]).
- No `unwrap`/`expect`/`panic` outside tests. No `unsafe`.

## Tests and the requirements they guard

Test names carry the requirement ID (`req_0012_NNN_*`, `req_0013_NNN_*`).

- 0012-054: config precedence, defaults, `--timeout`, default config path, usage errors before bus
  traffic, `start`/`stop` validation.
- 0012-055: exit codes, client error classification, `send --wait` rejection exit.
- 0012-056: proto-JSON output for tasks and messages.
- 0012-057: card contents and truncation, confirmation without a terminal, `--yes` decision
  routing, expired request refusal, `--note` refusal.
- 0012-058: unsigned sessions never decide; no provider keys in sources.
- 0012-059: color rules (`NO_COLOR`, non-terminal, VT detection).
- 0013-028: audit verification report, clean and broken chains, missing file, checkpoint keys.

## Deviations from RFC text

- `start` and `stop` have no supervisor control transport in any RFC. They validate the agent id
  and then fail with exit 1. Reported as RFC feedback; not faked.
- `watch` and `cancel` take an optional `--agent`; without it, configured agents are searched by
  `get_task`. The spec gives no agent id for these.
- `audit verify --json` is not supported: no proto schema exists for the verification report.
- `deny --note` is refused (exit 2): `ApprovalDecision` has no note field (RFC-0006).
- The approval card shows budget remaining and signature status as `missing`: the request proto
  carries neither field. Working directory and workspace mode come from the request.
