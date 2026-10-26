# NATS spike (RFC-0010 `[UNVERIFIED]` items)

Findings and verdicts: `docs/spikes/nats.md`. All scratch data lives in `.spikes/nats/` (gitignored).
Run every command from the repository root (seed and config paths are relative).

## Prerequisites

- Windows 11, PowerShell, Rust toolchain (`cargo`, MSVC target), and the built-in `certutil`, `icacls`, `netstat`, `tasklist`, `taskkill`.
- No admin rights and no system-wide install are needed.

## 1. Binaries (official releases, under `.spikes/nats/bin/`)

Versions: nats-server v2.15.2 and natscli v0.5.1. The download commands below use URL patterns inferred from the release asset names; the exact download command was not captured in the log `[UNOBSERVED]`.

```powershell
Invoke-WebRequest https://github.com/nats-io/nats-server/releases/download/v2.15.2/nats-server-v2.15.2-windows-amd64.zip -OutFile .spikes/nats/nats-server.zip
Invoke-WebRequest https://github.com/nats-io/natscli/releases/download/v0.5.1/nats-0.5.1-windows-amd64.zip -OutFile .spikes/nats/natscli.zip
certutil -hashfile .spikes/nats/nats-server.zip SHA256   # expect f6398d5d11093778f870db70242cb50d20a58134c937c805ba8d114e7e398766
certutil -hashfile .spikes/nats/natscli.zip SHA256       # expect 32bf9afbb1ee47b2ee0b869cce5440715c8f6d1ae70fe30e098ddf7b7fa1c894
Expand-Archive .spikes/nats/nats-server.zip -DestinationPath .spikes/nats/bin/srv
Expand-Archive .spikes/nats/natscli.zip -DestinationPath .spikes/nats/bin/cli
```

Resulting paths:
- `.spikes/nats/bin/srv/nats-server-v2.15.2-windows-amd64/nats-server.exe`
- `.spikes/nats/bin/cli/nats-0.5.1-windows-amd64/nats.exe`

## 2. Keys (not captured `[UNOBSERVED]`)

Seeds are `.spikes/nats/keys/<id>.seed` (ids: `omp`, `claude`, `tui`, `admin`); public keys are `.spikes/nats/keys/<id>.pub`. The generation command was not captured. Seeds are never printed or copied.

## 3. Server config

```powershell
powershell -ExecutionPolicy Bypass -File spikes/nats/gen-config.ps1
```

Writes `.spikes/nats/nats.conf` from the `.pub` files. Users: `omp`, `claude`, `tui` (read bundles), `admin` (full access). `max_payload` 1048576; `store_dir` `.spikes/nats/data`. There is no `http` key (RFC-0010 0010-003).

## 4. Streams and buckets

- `A2AB_EV_local` (RFC-0010 0010-024): definition in `spikes/nats/stream-ev.json`. The create command was not captured `[UNOBSERVED]`. Read back with `stream info` (see findings).
- KV buckets `a2ab-local-cards`, `a2ab-local-presence`, `a2ab-local-claims`: created outside the repo; the create commands were not captured `[UNOBSERVED]`. Their configuration was read back from the server (see findings).

## 5. Start / stop the server

```powershell
.spikes/nats/bin/srv/nats-server-v2.15.2-windows-amd64/nats-server.exe -c .spikes/nats/nats.conf -l .spikes/nats/logs/server.log -P .spikes/nats/nats-server.pid
```

No `-m` flag, so the monitoring port stays closed (0010-003). This form was used for the last sessions; the very first start command was not captured `[UNOBSERVED]`.

Stop (forced; the PID is in the pid file):

```powershell
taskkill /PID <pid> /F
netstat -ano | findstr ":4222 :4223 :4224 :8222"    # expect no LISTENING lines
tasklist | findstr /I nats-server                   # expect no output
```

Graceful console stop (CTRL_BREAK) was tested only inside the `job ctrlbreak` case.

## 6. Client probe (async-nats 0.50.0)

```powershell
cargo build --manifest-path spikes/nats/probe/Cargo.toml
spikes/nats/probe/target/debug/nats-spike-probe.exe <case>
```

Cases (requires the server running on 4222 with the config above):

| case | checks |
|---|---|
| `inbox-default` | 0010-066: default `_INBOX.<nuid>` denied; custom `_INBOX.<id>` works |
| `rpc` | 0010-061: `allow_responses` second reply denied |
| `rpc-expiry` | 0010-061: `allow_responses` `max: 1, expires: "2m"` syntax; late reply denied; no-responders error |
| `dedup` | 0010-026: `Nats-Msg-Id` duplicate detection |
| `ttl` | 0010-031: per-key TTL via `create_with_ttl` on a fresh key (PASS) and on a purged key (FAIL) |
| `ttl-manual` | 0010-031/032: explicit `Nats-TTL` header after purge, fenced by the purge revision |
| `fence` | 0010-032/033: create on existing key; wrong revision; current revision |
| `payload` | 0010-014/015: body and header size against `max_payload` |
| `watch` | 0010-034: `watch_all()` initial snapshot; explicit push-consumer workaround |
| `heartbeat` | push-consumer idle heartbeat (2 s) |
| `subjlen` | 0010-020: subjects of 130, 255, 256, 300, 1000 bytes |
| `advisory` | §4.10: `MAX_DELIVERIES` advisory subject (throwaway stream `A2AB_SPIKE_ADV`, deleted) |
| `job <mode> <store>` | 0010-070: Windows stop mechanisms. `mode` = `contained` (job object), `kill` (`TerminateProcess`), `ctrlbreak` (`GenerateConsoleCtrlEvent`). Starts its own server on 4223 with a config in `.spikes/nats/q1/`. Example: `job ctrlbreak .spikes/nats/q1/store-ctrlbreak` |

Permission denials are not returned to the client; they appear only in `.spikes/nats/logs/server.log` (grep the `Violation` lines).

## 7. Backup and restore (§4.13 / 0010-068)

Online backup (admin identity; the deprecated `stream backup` alias works in natscli 0.5.1):

```powershell
.spikes/nats/bin/cli/nats-0.5.1-windows-amd64/nats.exe --server nats://127.0.0.1:4222 --nkey .spikes/nats/keys/admin.seed stream backup A2AB_EV_local .spikes/nats/bk/ev-online
```

Offline copy (server stopped first):

```powershell
cp -r .spikes/nats/data .spikes/nats/bk/data-offline
```

Restore copy on port 4224 (scratch config; only the port and store dir change):

```bash
sed 's/4222/4224/; s#\.spikes/nats/data#.spikes/nats/bk/data-offline#' .spikes/nats/nats.conf > .spikes/nats/bk/restore.conf
.spikes/nats/bin/srv/nats-server-v2.15.2-windows-amd64/nats-server.exe -c .spikes/nats/bk/restore.conf -l .spikes/nats/bk/restore.log -P .spikes/nats/bk/restore.pid
```

Restore from the online backup into the copy (deletes the stream on the copy only):

```powershell
nats.exe --server nats://127.0.0.1:4224 --nkey .spikes/nats/keys/admin.seed stream rm A2AB_EV_local -f
nats.exe --server nats://127.0.0.1:4224 --nkey .spikes/nats/keys/admin.seed stream restore .spikes/nats/bk/ev-online
```

Verify with `stream info <name> --json` (messages, first_seq, last_seq, max_age, allow_msg_ttl). Stop the copy with `taskkill /PID <pid> /F`.

## 8. Not covered by these scripts

- Windows `sc.exe` service stop; behaviour with `LongPathsEnabled=0`; server-side header-budget check (raw HPUB); object-store subject encoding; monitoring HTTP endpoint (only "port not listening" was observed); seed ACL on the real `keys/` seeds (only a dummy file was tested with `icacls /inheritance:r /grant:r "<user>:F"`); store loss (0010-044); domain-qualified replay (0010-073/074, post-M1).
