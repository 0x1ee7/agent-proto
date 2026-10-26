# Spike: NATS binding (RFC-0010 `[UNVERIFIED]` items)

Settles the `[UNVERIFIED]` items of `docs/rfcs/0010-nats-binding.md` (§9 M1 spike list) on Windows 11.
Reproduction: `spikes/nats/README.md`. Scratch: `.spikes/nats/` (gitignored). No RFC was edited.

Environment: nats-server v2.15.2 (`windows-amd64`, zip SHA256 `f6398d5d…8766`), natscli v0.5.1 (SHA256 `32bf9afb…c894`), async-nats 0.50.0 (nkeys feature, MSVC build), Windows 11.

## Question → verdict

| # | RFC item | Question | Verdict |
|---|---|---|---|
| 1 | 0010-003, §4.1 | Monitoring HTTP disabled when no `http` key and no `-m` | PASS |
| 2 | 0010-066, §9.6 | Default `_INBOX.<nuid>` denied; `_INBOX.<id>` prefix works | PASS |
| 3 | 0010-061, §4.11, §9.8 | `allow_responses` `max: 1, expires: "2m"` accepted; second and late replies denied | PASS |
| 4 | 0010-026 | `Nats-Msg-Id` dedup | PASS |
| 5 | 0010-031 | Per-key TTL on a fresh key (async-nats `create_with_ttl`) | PASS |
| 6 | 0010-031 | Per-key TTL on a purged key via async-nats `create_with_ttl` | FAIL (TTL dropped) |
| 7 | 0010-031, 0010-032 | Explicit `Nats-TTL` after purge, fenced by purge revision | PASS |
| 8 | 0010-032, 0010-033 | Create/update fence (`key already exists`, wrong/stale revision) | PASS |
| 9 | 0010-034, §4.6 | Initial-snapshot marker via async-nats `watch_all()` | FAIL (0 entries) |
| 10 | 0010-034, §4.6 | Initial snapshot via explicit push consumer (`LastPerSubject`, `pending == 0`) | PASS |
| 11 | 0010-014/015, §9.9 | Body + headers vs `max_payload` 1 MiB | PARTIAL |
| 12 | 0010-020, §4.4 | Subject length: server accepts 1000 bytes; the 255-byte cap is the RFC's own rule | PASS |
| 13 | 0010-017/065, §9.2 | Long store path (357 chars) with `LongPathsEnabled=1` | PASS (conditional) |
| 14 | 0010-010, §9.3 | Seed-file ACL mechanism (`icacls`) | PARTIAL (dummy file only; real seeds not checked) |
| 15 | 0010-070, §9.1 | Windows graceful stop (console CTRL_BREAK; job-object kill) | PARTIAL (`sc.exe` service stop `[UNOBSERVED]`) |
| 16 | §9.4 | async-nats 0.50.0 builds and runs on MSVC | PASS |
| 17 | 0010-059, §4.11, §9.5 | Read bundle | PARTIAL (works; minimal set not reduced) |
| 18 | §4.10, §9.7 | Advisory subject for max-deliveries | PASS |
| 19 | 0010-068, §4.13, §9.10 | Offline copy restore; online `stream backup` / `stream restore` | PASS (event stream and presence stream; scope below) |
| 20 | 0010-044, §9.11 | Store loss behaviour | BLOCKED (not run; doc-only item) |
| 21 | 0010-073/074, §9.12 | Domain-qualified replay | BLOCKED (post-M1, out of scope) |
| 22 | §4.7 | Object store subject encoding | BLOCKED (not run) |
| 23 | (supplementary) | Push-consumer idle heartbeat (2 s) | PASS (`heartbeats_in_7s=3`) |

## Method (summary)

- Server: `nats-server -c .spikes/nats/nats.conf -l .spikes/nats/logs/server.log -P <pid file>` (no `-m`). Config from `spikes/nats/gen-config.ps1`: four NKey users (`omp`, `claude`, `tui`, `admin`); `max_payload` 1048576.
- Client: in-process Rust probe `spikes/nats/probe` (async-nats 0.50.0). Each case prints what the client observed. Denials are not returned to the client (the server silently drops unauthorized publishes), so denials were checked in `logs/server.log`.
- Identities: `admin` used only where the test needs full access (fresh-key TTL, backup, advisory, subject length). Read bundle derived from CLI denial lines.

## Results

### 1. Monitoring (0010-003, §4.1) — PASS
Config has no `http` key (`grep` exit 1). Server started without `-m`. `netstat -ano | findstr LISTENING | findstr ":8222"` → no output (exit 1).

### 2. Inbox prefix (0010-066, §9.6) — PASS
Default inbox: `inbox-default: err: request timed out`. Server log:
```
Subscription Violation - Subject "_INBOX.qwRovmGq7fLO8IJbwwc6IA.*", SID 0
```
(client `v0.50.0:rust`, identity omp). Custom `custom_inbox_prefix("_INBOX.<id>")` works. Fallback 0010-067 not needed.

### 3. allow_responses (0010-061, §4.11, §9.8) — PASS
Server accepted `allow_responses: {max: 1, expires: "2m"}`. Second reply denied (log): `Publish Violation - Subject "_INBOX.tui.hJDwbbehjbScNKCyLITapY.hJDwbbehjbScNKCyLITal1"`. Late reply after 2 m denied: client `err: request timed out: deadline has elapsed` at +125 s, client saw `sent=true`; log: `Publish Violation - Subject "_INBOX.tui.3bvMwqDgpdmpzQSMT3gTj3.3bvMwqDgpdmpzQSMT3gTfn"`. Request with no responders: `err: no responders`.

### 4. Dedup (0010-026) — PASS
```
dedup: first seq=4 dup=false
dedup: same-id seq=4 dup=true
dedup: other-id seq=5 dup=false
```

### 5–7. Per-key TTL (0010-031, 0010-032) — FAIL on purged-key path; PASS on fresh key and workaround
- Bucket config read back (`stream info KV_a2ab-local-presence --json`, trimmed): `"max_msgs_per_subject": 1`, `"max_msg_size": 4096`, `"discard": "new"`, `"duplicate_window": 120000000000`, `"deny_delete": true`, `"deny_purge": false`, `"allow_msg_ttl": true`, `"subject_delete_marker_ttl": 60000000000`.
- Fresh key, admin `create_with_ttl(3s)`: `+7s op=Purge rev=6` → TTL honoured.
- Purged key, omp `create_with_ttl(3s)`: `+7s op=Put rev=8` → TTL dropped. Cause: async-nats 0.50.0 `create_maybe_ttl` falls back to plain `update()` when the last entry is Purge/Delete (`kv/mod.rs`).
- Workaround, admin, `ttl-manual`: put after purge with `Nats-TTL: 3s` and `Nats-Expected-Last-Subject-Sequence` = purge revision:
```
ttl-manual: put after purge with explicit Nats-TTL ok seq=11 purge_rev=10
ttl-manual: +0s op=Some(Put)
ttl-manual: +7s op=Some(Purge)
```
- Option name: async-nats 0.50.0 `kv::Config` field is `limit_markers: Option<Duration>` (feature `server_2_11`), not `limit_marker_ttl`. Go name `LimitMarkerTTL` (`docs/interop/nats.md` §4.1). Observed server effect: stream `allow_msg_ttl: true` + `subject_delete_marker_ttl`. Bucket creation command `[UNOBSERVED]` (not captured in repo).

### 8. Fence (0010-032, 0010-033) — PASS
```
fence: put rev=4
fence: create on existing key => err: key already exists
fence: update with wrong revision => err: wrong last revision: wrong last sequence: wrong last sequence: 4 (code 400, error code 10071)
fence: update with current revision => ok rev=5
fence: update with stale revision => err: wrong last revision: wrong last sequence: wrong last sequence: 5 (code 400, error code 10071)
```

### 9–10. Initial snapshot marker (0010-034, §4.6) — FAIL via `watch_all()`; PASS via explicit push consumer
```
watch: cards next() timed out after 5s
watch: cards entries before/at marker=0
watch-diag: get(omp) value_bytes=1
watch-diag: plain push consumer (DeliverPolicy::All) delivered=1
watch-diag: plain push consumer (DeliverPolicy::LastPerSubject) delivered=1
watch-diag: admin watch_all first=timeout
watch-diag: watch-config push consumer (filter_subjects, Instant, FC, HB5s) delivered=1
watch-diag: delay=0ms stream_seq=3 pending=0
watch-diag: create-then-subscribe delay=0ms delivered=1
watch-diag: delay=1000ms stream_seq=3 pending=0
watch-diag: create-then-subscribe delay=1000ms delivered=1
watch: empty bucket => no entry (no initial marker) within 5s
```
Root cause inside async-nats's ordered push wrapper (`push.rs`, `kv/mod.rs` `Watch::poll_next`) `[INFERENCE]`, not isolated. Admin identity also times out, so it is not a permission issue. Workaround: explicit push consumer with `DeliverPolicy::LastPerSubject`, filter `$KV.<bucket>.>`; the message whose ACK reply has `pending == 0` ends the snapshot. Empty bucket: no marker, so check consumer `num_pending == 0` at creation.

### 11. Payload and header budget (0010-014/015, §9.9) — PARTIAL
```
payload: body=1048575 no-headers max_payload=1048576 flush_ok=true
payload: body=1048576 no-headers max_payload=1048576 flush_ok=true
payload: body=1048000 headers~621 max_payload=1048576 publish_err=max payload size exceeded: Payload size limit of 1048576 exceeded by message size of 1048621
payload: body=1046000 headers~1621 max_payload=1048576 flush_ok=true
```
Headers count toward the limit in the async-nats client (client-side rejection). No `Payload`/`Maximum` violation lines in `server.log`. Server-side header counting (raw HPUB) `[UNOBSERVED]`.

### 12. Subject length (0010-020) — PASS
```
subjlen: subject_bytes=130 delivered=true
subjlen: subject_bytes=255 delivered=true
subjlen: subject_bytes=256 delivered=true
subjlen: subject_bytes=300 delivered=true
subjlen: subject_bytes=1000 delivered=true
```
The 255-byte cap is the RFC's own rule, not a server limit.

### 13. Long paths (0010-017/065) — PASS (conditional)
`LongPathsEnabled = 1`. Store path 357 chars (16 × `segment_0123456789ab/` + `store-<mode>`) works. Behaviour with `LongPathsEnabled=0` `[UNOBSERVED]`.

### 14. Seed ACL (0010-010) — PARTIAL (mechanism only)
Dummy file `.spikes/nats/acl/dummy.seed`. Before: inherited `NT AUTHORITY\SYSTEM:(I)(F)`, `BUILTIN\Administrators:(I)(F)`, `DX\0x1de:(I)(F)`. After `icacls <file> /inheritance:r /grant:r "<user>:F"`: only `DX\0x1de:(F)`. The spike's real `keys/*.seed` files were not ACL-checked.

### 15. Windows graceful stop (0010-070, §9.1) — PARTIAL
Probe `job <mode> <store>` (own server on 4223):
- `ctrlbreak` (`CREATE_NEW_PROCESS_GROUP` + `GenerateConsoleCtrlEvent`): `ok=true`, `child_exit=Some(0) port_open=false`.
- `kill` (`TerminateProcess`): exit `Some(1)`.
- `contained` (job object, `KILL_ON_JOB_CLOSE`): `CreateJobObject ok=true KILL_ON_JOB_CLOSE set=true assigned=true`, exit 0.
`sc.exe` service stop `[UNOBSERVED]`.

### 16. Client on MSVC (§9.4) — PASS
async-nats 0.50.0 with `nkeys` builds on the MSVC toolchain; all probe cases ran on Windows.

### 17. Read bundle (0010-059, §4.11, §9.5) — PARTIAL
The bundle in `gen-config.ps1` passed all probe cases. Denials derived from the CLI (log lines 55–121, e.g. `$JS.API.STREAM.INFO.A2AB_EV_local`, `$JS.API.STREAM.NAMES`, `$KV.a2ab-local-cards.tui`, `a2a.v1.local.ev.>`). The minimal set was not reduced `[UNOBSERVED]`.

### 18. Advisory subject (§4.10, §9.7) — PASS
Throwaway stream `A2AB_SPIKE_ADV` (`a2a.spike.adv.>`), push consumer `adv` (`max_deliver: 1`, `ack_wait: 1s`, explicit ack, no ack). First delivery `received=true`. Advisory observed:
`$JS.EVENT.ADVISORY.CONSUMER.MAX_DELIVERIES.A2AB_SPIKE_ADV.adv` (payload 198 bytes). Stream deleted afterwards.

### 19. Backup and restore (0010-068, §4.13, §9.10) — PASS (scope below)
- Online: `nats stream backup A2AB_EV_local .spikes/nats/bk/ev-online` (deprecated alias; `nats backup stream` is the new name): `Received 345 B compressed data in 1 chunks for stream "A2AB_EV_local" in 1ms, 433 B uncompressed`, exit 0.
- Offline: server stopped, `cp -r .spikes/nats/data .spikes/nats/bk/data-offline`; restored on port 4224. `A2AB_EV_local`: messages 3, first_seq 1, last_seq 3, `max_age` 604800000000000 (7 d). `KV_a2ab-local-presence`: messages 2, first_seq 8, last_seq 12, `allow_msg_ttl: true`.
- Online restore into the copy (stream deleted on the copy only): `Restored stream "A2AB_EV_local" in 0s`; messages 3, first_seq 1, last_seq 3, `max_age` preserved.
- Scope: `A2AB_EV_local` (online and offline) and the presence stream (offline). KV presence online backup and `stream snapshot` API `[UNOBSERVED]`.

### 20–23. Not run
- 20: store loss (0010-044): doc-only, no test defined.
- 21: domain-qualified replay (0010-073/074): post-M1.
- 22: object-store subject encoding (§4.7): not run.
- 23: heartbeat (supplementary): `heartbeats_in_7s=3 at_s=[2.0000584, 4.000659, 6.000911]`.

## Unobserved (must not be read as PASS)
- `sc.exe` service stop; `LongPathsEnabled=0` behaviour; server-side header-budget check (raw HPUB); object-store encoding (§4.7); monitoring HTTP endpoint (only "not listening" observed); real `keys/*.seed` ACL; retention of a KV bucket through online backup; store loss (0010-044); domain replay (0010-073/074).
- Key generation, stream/bucket creation commands, and the first server start command were not captured in the log (the README gives the forms used).

## Blockers
None hard. Open item: async-nats `watch_all()` root cause is not isolated (workaround recorded). `docs/interop/nats.md` §10 items (Windows `sc.exe`, `store_dir` default, memory footprint) remain open; no figure is claimed.

## RFC changes required (exact edits; RFC not edited)

- **RFC-0010 0010-015** (§4.3, line 82): replace the `[UNVERIFIED]` header-budget text with: "Headers count toward `max_payload` in async-nats 0.50.0 (client-side rejection observed: body 1048000 + headers ~621 rejected at 1048621 bytes). Server-side header counting `[UNOBSERVED]`. The 512 KiB body and 1 MiB total rule stays."
- **RFC-0010 0010-020** (§4.4, line 90): replace "(`[UNVERIFIED]` server limit)" with "The 255-byte cap is this RFC's own rule. The server accepted subjects of 1000 bytes (spike `subjlen`)."
- **RFC-0010 0010-031** (§4.6, line 118): replace the option name `limit_marker_ttl` with `limit_markers` (async-nats `kv::Config`; Go `LimitMarkerTTL`; server effect `allow_msg_ttl: true` + `subject_delete_marker_ttl`). Add: "Writers MUST set `Nats-TTL` explicitly on every put, including the put that re-creates a purged key. Do not use async-nats `create_with_ttl` on a purged key (the TTL is dropped in async-nats 0.50.0). Fence that re-create with `Nats-Expected-Last-Subject-Sequence` = the purge revision."
- **RFC-0010 0010-034** (§4.6, line 121): replace "(KV watch initial-values marker, `[UNVERIFIED]` client API)" with: "Watchers MUST use an explicit push consumer (`DeliverPolicy::LastPerSubject`, filter `$KV.<bucket>.>`). The message whose ACK reply has `pending == 0` marks the end of the snapshot. An empty bucket delivers no marker: check consumer `num_pending == 0` at creation. async-nats 0.50.0 `watch_all()` delivered 0 entries on a 1-entry bucket (spike `watch`)."
- **RFC-0010 §4.10** (line 283): replace "(exact subject `[UNVERIFIED]`)" with "(`$JS.EVENT.ADVISORY.CONSUMER.MAX_DELIVERIES.<stream>.<consumer>` observed, spike `advisory`)."
- **RFC-0010 0010-059** (§4.11, line 289): keep the list; add "minimal set `[UNOBSERVED]`; the list was derived from server denial logs."
- **RFC-0010 0010-061** (§4.11, line 310): replace "# syntax [UNVERIFIED]" with "# syntax accepted by nats-server v2.15.2 (spike `rpc-expiry`)."
- **RFC-0010 0010-010** (§9 item 3, line ~466): replace the seed ACL item with "Mechanism verified: `icacls <file> /inheritance:r /grant:r "<user>:F"` leaves only the owner. Real seed files `[UNOBSERVED]`."
- **RFC-0010 0010-070** (§9 item 1, lines 329 and 466): add "Windows graceful stop verified via `GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT)` on a process group created with `CREATE_NEW_PROCESS_GROUP` (exit 0). `sc.exe` service stop `[UNOBSERVED]`."
- **RFC-0010 0010-017/065** (§9 item 2, line 467): add "357-character store path verified with `LongPathsEnabled=1`. Behaviour with `LongPathsEnabled=0` `[UNOBSERVED]`."
- **RFC-0010 §9 item 4** (line 469): PASS: "async-nats 0.50.0 builds and runs on MSVC."
- **RFC-0010 §9 item 6** (line 471): PASS; fallback 0010-067 not needed.
- **RFC-0010 §9 item 8** (line 473): PASS for syntax and expiry.
- **RFC-0010 §9 item 9** (line 474): PARTIAL (see 0010-015).
- **RFC-0010 0010-068 / §9 item 10** (lines 327, 475): add "Offline copy and online `stream backup`/`stream restore` verified for `A2AB_EV_local` (procedure: `spikes/nats/README.md` §7). `stream snapshot` API and KV online backup `[UNOBSERVED]`."
- **RFC-0010 0010-073 / §9 item 12** (line 337): post-M1, `[UNOBSERVED]`.
- **RFC-0010 0010-003 / §4.1** (line 69, line 386): add "Verified: no `http` key; no listener on 8222 when the server runs without `-m`."
- **RFC-0010 §4.7** (object-store subject encoding): `[UNOBSERVED]`; keep `[UNVERIFIED]`.
