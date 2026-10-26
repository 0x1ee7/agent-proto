# NATS / JetStream findings for the A2A-over-bus binding

Scope: facts needed to write the NATS binding RFC (subject taxonomy, stream/KV layout, permissions). Versions checked: nats-server 2.15 docs; nats-server, nats.go and nats.rs `main` branches as fetched in 2026.
Convention: every claim has a source URL. `[UNVERIFIED]` = not confirmed from a primary source.
Source shorthand:
- `DOCS/` = https://docs.nats.io/
- `SRV/` = https://raw.githubusercontent.com/nats-io/nats-server/main/server/
- `GO/` = https://raw.githubusercontent.com/nats-io/nats.go/main/jetstream/
- `RS/` = https://raw.githubusercontent.com/nats-io/nats.rs/main/

---

## 1. Server install, service, storage, memory

| Item | Finding | Source |
|---|---|---|
| Latest docs version | Docs nav shows "2.15 (latest)" | DOCS/running-a-nats-service/introduction/installation |
| Release artifacts | Releases on GitHub `nats-io/nats-server/releases/`; Docker image `nats` on Docker Hub | https://github.com/nats-io/nats-server/releases/ , https://github.com/nats-io/nats-server (README badges) |
| Binary flags | `-p/--port` (default 4222), `-m/--http_port`, `-c/--config`, `-n/--name`, `-P/--pid`, `-js/--jetstream`, `-sd/--store_dir`, `-DV`, `-DVV`, `-sl/--signal` (ldm, stop, quit, term, reopen, reload), `-s/--syslog` ("Log to syslog or windows event log") | SRV/../main.go (https://raw.githubusercontent.com/nats-io/nats-server/main/main.go) |
| Windows service | `server/service_windows.go` wraps the server with `golang.org/x/sys/windows/svc`; `serviceName = "nats-server"`, `SetServiceName(name)`; `Run(server)` calls `svc.IsWindowsService()` and only then `svc.Run`. Handles Stop/Shutdown, ParamChange (reload), reopen-log. No install/uninstall flag was found in `main.go`. | SRV/service_windows.go |
| Windows service registration | Registering the service with the Windows SCM (`sc.exe create ...`) is not documented in the fetched pages. `[UNVERIFIED]` exact procedure. The `DOCS/.../nats_admin/windows_srv` URL returned the Monitoring chapter, not Windows install docs. | DOCS/running-a-nats-service/nats_admin/windows_srv (wrong page) |
| Windows cert store | TLS `cert_store`: "Load the certificate from an OS certificate store rather than a file. Windows only." | DOCS/reference/config/tls |
| JetStream store_dir default | `store_dir`: default `/tmp/nats/jetstream`, Reloadable "only when JetStream is not currently enabled". Windows default path `[UNVERIFIED]` (the Go code uses a temp dir when config is nil; see jetstream.go `EnableJetStream` comment "A nil configuration will dynamically choose the limits and temporary file storage directory"). | DOCS/reference/config/jetstream ; SRV/jetstream.go |
| `max_memory_store` | Default "75% of available memory" (code: `sysmem.Memory()`, lowered by `GOMEMLIMIT`). Reload: 2.12/2.14 increases only while JetStream enabled; 2.11 never. | DOCS/reference/config/jetstream/max_memory_store ; SRV/jetstream.go |
| `max_file_store` | Default "up to 1TB if available" (code: `diskAvailable(StoreDir)` when unset, `maxStorePending`). | DOCS/reference/config/jetstream/max_file_store ; SRV/jetstream.go |
| `domain` | "The JetStream domain the server is part of." Requires Restart. | DOCS/reference/config/jetstream/domain |
| `encryption_key` | Enables JetStream filestore encryption, "a key length of at least 32 bytes is recommended". | DOCS/reference/config/jetstream |
| Memory footprint per server / per stream | `[UNVERIFIED]` no measured figure found. Do not claim a number in RFC. | — |

Config reference (selected defaults, from reference config pages):

| Key | Default | Source |
|---|---|---|
| `max_payload` | 1MB (`MAX_PAYLOAD_SIZE`); hard max `MAX_PAYLOAD_MAX_SIZE` 8MB (warning threshold) | SRV/const.go ; DOCS/reference/config |
| Violation error | `Maximum Payload Violation` (protocol `-ERR`), `ErrMaxPayload` | SRV/client.go |
| `max_pending` | 64MB (`MAX_PENDING_SIZE`) | SRV/const.go ; DOCS/reference/config |
| `max_control_line` | 4KB (`MAX_CONTROL_LINE_SIZE` 4096) | SRV/const.go |
| `max_connections` | 64K (`DEFAULT_MAX_CONNECTIONS`) | SRV/const.go |
| `ping_interval` | 2m; `ping_max` 2 | SRV/const.go |
| `write_deadline` | 10s (`DEFAULT_FLUSH_DEADLINE`) | SRV/const.go |
| `lame_duck_duration` | min 30s; `lame_duck_grace_period` 10s | DOCS/reference/config |
| Client port / monitor port | 4222 / 8222 | SRV/const.go |
| Leaf node port | 7422 (`host` default `0.0.0.0`) | DOCS/reference/config/leafnodes/ |
| Auth timeout | `AUTH_TIMEOUT` 2s | SRV/const.go |

Monitoring: monitoring port 8222 queried for server, client, cluster state (`/varz`-style JSON). Endpoints named in DOCS/running-a-nats-service/nats_admin/... (monitoring chapter). `[UNVERIFIED]` exact endpoint list (not read).

---

## 2. JetStream

### 2.1 Streams

| Item | Finding | Source |
|---|---|---|
| Delivery semantics | Core NATS at-most-once; JetStream stores messages and provides at-least-once | DOCS/nats-concepts/jetstream (concept); DOCS/learn/jetstream/your-first-stream |
| StreamConfig JSON fields | `name`, `subjects`, `retention`, `max_consumers`, `max_msgs`, `max_bytes`, `max_age`, `max_msgs_per_subject`, `max_msg_size`, `discard`, `storage`, `num_replicas`, `no_ack`, `duplicate_window`, `placement`, `mirror`, `sources`, `compression`, `first_seq`, `subject_transform`, `republish`, `allow_direct`, `mirror_direct`, `discard_new_per_subject`, `sealed`, `deny_delete`, `deny_purge`, `allow_rollup_hdrs`, `consumer_limits`, `allow_msg_ttl`, `allow_msg_counter` | SRV/stream.go |
| Name rules | Case-sensitive; no `.`, `*`, `>`, space, `/`, `\`, tab/newline (`isValidAssetName`) | DOCS/learn/jetstream/your-first-stream ; SRV/jetstream.go (`isValidAssetName`) |
| Subject overlap | Error `subjects overlap with an existing stream (10065)` | DOCS/learn/jetstream (streams page) |
| Defaults | `num_replicas` 1, `storage` File, `retention` Limits, `discard` Old, limits unlimited | DOCS/learn/jetstream/your-first-stream ; DOCS/learn/jetstream/retention-policies |
| Retention | Limits (keep until limits), Interest (keep while consumers need), WorkQueue (delete on ack; one consumer per message) | DOCS/learn/jetstream/retention-policies |
| `max_msgs_per_subject` | Keeps last N per subject; used by KV history | DOCS/learn/jetstream/shaping-the-stream |
| Mirrors / sources | Mirror = single upstream copy; sources = aggregate multiple streams | DOCS/learn/jetstream/mirrors-and-sources |
| Subject transform | Stream-level input→output mapping | DOCS/learn/jetstream/subject-mapping |
| Message TTL | `allow_msg_ttl` in StreamConfig; per-message header `Nats-TTL` | SRV/stream.go ; SRV/const.go header constants |
| Purge/delete | `deny_delete`, `deny_purge`, `sealed` flags | SRV/stream.go |

### 2.2 Publishing, dedup, expected-sequence headers

| Header (constant) | Purpose | Source |
|---|---|---|
| `Nats-Msg-Id` | Dedup key; within `duplicate_window` (default `StreamDefaultDuplicatesWindow = 2 * time.Minute`) a repeated ID returns `duplicate: true` in PubAck | DOCS/learn/jetstream/publishing ; SRV/stream.go |
| `Nats-Expected-Stream` | Assert target stream name | SRV/const.go / stream.go |
| `Nats-Expected-Last-Sequence` | Optimistic concurrency on stream seq | SRV/const.go |
| `Nats-Expected-Last-Subject-Sequence` | Optimistic concurrency per subject | SRV/const.go |
| `Nats-Expected-Last-Subject-Sequence-Subject` | Subject for the above check | SRV/const.go |
| `Nats-Expected-Last-Msg-Id` | Expect last message ID | SRV/const.go |
| `Nats-Rollup` | Rollup (purge subject / all) | SRV/const.go |
| `Nats-TTL` | Per-message TTL | SRV/const.go |
| `Nats-Marker-Reason` | Marker messages | SRV/const.go |

Publish result (PubAck) includes `duplicate` boolean. Source: DOCS/learn/jetstream/publishing, DOCS/learn/jetstream/advanced-publishing.

### 2.3 Consumers

| Field / behavior | Default / rule | Source |
|---|---|---|
| `ack_policy` | `Explicit` (pull example); others exist | DOCS/learn/jetstream/delivery-and-acknowledgment |
| `ack_wait` | 30s (`JsAckWaitDefault`) | SRV/consumer.go ; DOCS/learn/jetstream/delivery-and-acknowledgment |
| `max_ack_pending` | 1000 (`JsDefaultMaxAckPending`); error text "out of maximum 1,000"; use `max_ack_pending 1` for strict ordering | SRV/consumer.go ; DOCS/learn/jetstream/delivery-and-acknowledgment |
| `max_deliver` | -1 = unlimited (default). Without it, un-acked messages redeliver forever | SRV/consumer.go ; DOCS/learn/jetstream/delivery-and-acknowledgment |
| `backoff` | Array of durations; `backoff[0]` must equal `ack_wait` (pedantic check) | SRV/consumer.go |
| `inactive_threshold` | Ephemeral consumer cleanup | SRV/consumer.go |
| `num_replicas` | Consumer replicas | SRV/consumer.go |
| `deliver_policy` | `All` (default in examples) | DOCS/learn/jetstream/delivery-and-acknowledgment, DOCS/learn/jetstream/reading-back |
| `filter_subject(s)` | Subset filter on stream subjects | DOCS/learn/jetstream/filtering |
| Pinned TTL (priority groups) | `JsDefaultPinnedTTL` 2m | SRV/consumer.go ; DOCS/learn/jetstream/priority-groups |

Ack semantics:
- Ack types: `+ACK`, NAK (redeliver), `+TERM`, `+WPI` (in progress) `[UNVERIFIED]` exact wire tokens not re-checked here; client libs expose `Ack`, `AckSync`, `Nak`, `Term`, `InProgress`.
- Double ack: `DoubleAck`/`ackAck`/`ack_sync`/`ackSync`/`AckAsync`/`double_ack` (client method names) waits for server confirmation. Source: DOCS/learn/jetstream/delivery-and-acknowledgment.
- Ack reply subject (v2), built from `ackReplyT` and `ackSubj`: `$JS.ACK.<domain>.<accHash>.<stream>.<consumer>.<deliveryCount>.<streamSeq>.<consumerSeq>.<timestamp>.<pending>`. Source: SRV/consumer.go (`jsAckTv2 = "$JS.ACK.%s.%s.%s.%s"`, `ackReplyT`); parser `ackReplyInfo` in SRV/consumer.go.
- Forgetting to ack with default `max_deliver` = unlimited → infinite redelivery. Source: DOCS/learn/jetstream/delivery-and-acknowledgment.
- Pull consumer: `fetch`/`consume` (client); request subject `$JS.API.CONSUMER.MSG.NEXT.<stream>.<consumer>`. Source: SRV/jetstream_api.go (`JSApiRequestNextT`), DOCS/learn/jetstream/pull-consumers.
- Pull request timeouts/idle heartbeat: status `408 Request Timeout`, `100 Idle Heartbeat`, `409 Consumer Deleted`, `409 Consumer is push based`. Source: SRV/consumer.go.
- Ordered consumer (client-side, no ack, recreated on gap): DOCS/learn/jetstream/ordered-consumer, GO/ `OrderedConsumer`.
- Pausing: DOCS/learn/jetstream/pausing (API exists; field name `[UNVERIFIED]`).
- Redelivery: governed by `ack_wait`, `backoff`, `max_deliver`. Advisories exist for max-deliveries exceeded `[UNVERIFIED]` exact subject name.

### 2.4 JetStream API subjects (permissions-relevant)

| Subject pattern | Purpose | Source |
|---|---|---|
| `$JS.API.INFO` | Account info | SRV/jetstream_api.go |
| `$JS.API.STREAM.CREATE.<stream>` / `.UPDATE.<stream>` / `.DELETE.<stream>` / `.INFO.<stream>` / `.PURGE.<stream>` / `.SNAPSHOT.<stream>` / `.RESTORE.<stream>` / `.NAMES` / `.LIST` | Stream CRUD | SRV/jetstream_api.go |
| `$JS.API.STREAM.MSG.GET.<stream>` / `.MSG.DELETE.<stream>` | Message get/delete | SRV/jetstream_api.go |
| `$JS.API.DIRECT.GET.<stream>` and `$JS.API.DIRECT.GET.<stream>.>` | Direct (no-leader) get | SRV/jetstream_api.go |
| `$JS.API.CONSUMER.CREATE.<stream>[.<consumer>[.<filter>]]` | Consumer create | SRV/jetstream_api.go |
| `$JS.API.CONSUMER.DURABLE.CREATE.<stream>.<consumer>` | Durable create | SRV/jetstream_api.go |
| `$JS.API.CONSUMER.INFO.<stream>.<consumer>` / `.NAMES.<stream>` / `.LIST.<stream>` | Consumer info/list | SRV/jetstream_api.go |
| `$JS.API.CONSUMER.MSG.NEXT.<stream>.<consumer>` | Pull next | SRV/jetstream_api.go |
| `$JS.ACK.<...>` | Ack replies (subscribe-side for consumer) | SRV/consumer.go |

Note: `jsAllAPI = "$JS.API.>"`. Permissions that allow `$JS.API.>` grant the entire control plane (DOCS/running-a-nats-service/configuration/securing_nats/authorization: "the $JS.API.> control plane").

---

## 3. Core NATS

| Item | Finding | Source |
|---|---|---|
| Subjects | "case-sensitive and can contain any UTF-8 characters except whitespace, tabs and line breaks"; dot-separated tokens | DOCS/nats-concepts/subjects |
| Wildcards | `*` matches exactly one token; `>` matches one or more trailing tokens | DOCS/nats-concepts/subjects ; DOCS/running-a-nats-service/configuration/securing_nats/authorization |
| Reserved prefixes | `$JS.`, `$KV.`, `$O.`, `$SYS.` (system), `_INBOX.` (default inbox prefix) | SRV/jetstream_api.go, GO/kv.go, GO/object.go, DOCS/.../authorization ("_INBOX. is the default") |
| Request/reply | Publish + subscribe on a reply subject (`_INBOX.`) | DOCS/.../authorization |
| No responders | Server sends `NATS/1.0 503` with `Nats-Subject` header to requester when no subscriber and the client set `no_responders` (requires headers). Client error text: `no responders available for request`. | SRV/client.go (lines ~2801–2807 `HMSG ... NATS/1.0 503`), SRV/client.go (`ErrNoRespondersRequiresHeaders`); DOCS/learn (error string) |
| Headers | Supported via `HMSG`; `no_header_support` config key exists | DOCS/reference/config (no_header_support), SRV/client.go |
| Max payload | 1MB default; violation returns `-ERR 'Maximum Payload Violation'` | SRV/const.go, SRV/client.go |
| Subscription limits | `max_subscriptions`, `max_subscription_tokens` keys exist | DOCS/reference/config |
| Slow consumers | `max_pending` 64MB per connection | SRV/const.go |

Design implication: the NATS binding should use `_INBOX.<...>` reply subjects (default prefix) and rely on `503` for fast failure of request/reply when no agent subscribes.

---

## 4. KV and Object Store

### 4.1 KV (`GO/kv.go`)

| Item | Finding | Source |
|---|---|---|
| Backing stream | `KV_<bucket>` (stream name prefix); subjects `$KV.<bucket>.>`; format string `$KV.%s.` | GO/kv.go ; DOCS/nats-concepts/jetstream/key-value-store |
| Key → subject | key = last subject token; `get` = last message on subject | DOCS/nats-concepts/jetstream/key-value-store |
| History | default `KeyValueMaxHistory = 64` | GO/kv.go |
| Create | Conflict returns `ErrKeyExists` | GO/kv.go |
| Per-key TTL | Requires `LimitMarkerTTL` in bucket config (needed for watch to see expirations) | GO/kv.go |
| Config fields | `TTL`, `MaxBytes`, `MaxValueSize`, `LimitMarkerTTL`, `AllowDirect` (direct get) | GO/kv.go |
| Watch | Implemented as consumer on the stream | DOCS/nats-concepts/jetstream/key-value-store |

### 4.2 Object Store (`GO/object.go`)

| Item | Finding | Source |
|---|---|---|
| Chunk subject | `$O.<bucket>.C.<nuid>` (chunks) | GO/object.go |
| Metadata subject | `$O.<bucket>.M.<name>` | GO/object.go |
| Subject filter | `$O.%s.C.>` and `$O.%s.M.>` | GO/object.go |
| Chunk size | `objDefaultChunkSize = 128*1024` | GO/object.go |
| Stream name | `OBJ_<bucket>` | GO/object.go |

---

## 5. Auth, accounts, permissions

### 5.1 Authentication modes

| Mode | Config key(s) | Source |
|---|---|---|
| Global user/password | `username`, `password` (exclusive with `token`) | DOCS/reference/config/authorization |
| Token | `token` | DOCS/reference/config/authorization |
| Users list | `users: [{user, password, permissions}]` | DOCS/reference/config/authorization |
| Default permissions | `default_permissions` (applies only to users without their own block; never merged) | DOCS/running-a-nats-service/configuration/securing_nats/authorization |
| Auth callout | `auth_callout` | DOCS/reference/config/authorization |
| NKeys (per-user public key) | `nkey` inside users / accounts; account public key starts `A` | DOCS/reference/config/accounts ; DOCS/.../jwt |
| Operator mode (JWT) | `operator`, `trusted_keys`, `resolver`, `resolver_preload`, `resolver_pinned_accounts`, `system_account`, `no_system_account` | DOCS/reference/config/accounts ; DOCS/reference/config |
| TLS | `tls { cert_file, key_file, ca_file, verify, verify_and_map, timeout, cipher_suites, curve_preferences, insecure, pinned_certs, cert_store, certs, handshake_first }` | DOCS/reference/config/tls |
| `no_auth_user` | Fallback user for unauthenticated clients | DOCS/reference/config/authorization |

NKey prefixes (from the JWT page): operator public `O...`, account `A...`, user `U...`; seeds prepend `S` (example: `SUAH...` = user seed). Signing is with the seed; server only sees public keys and signatures; the nonce signature proves possession. Source: DOCS/running-a-nats-service/configuration/securing_nats/jwt.

JWT trust chain (from the page): user JWT signed by account (or account signing key); account JWT signed by operator; server verifies against keys in operator JWT; revocation and expiring creds exist. Source: same page.

### 5.2 Permissions (per user)

Shape:
```
users: [{ user: order-svc, password: "...",
  permissions: {
    publish:   { allow: ["orders.>"] },
    subscribe: { allow: ["_INBOX.>"] } } }]
```
Source: DOCS/running-a-nats-service/configuration/securing_nats/authorization.

Rules (verbatim-ish from the page):
- Permission = list of subjects for publish and separate list for subscribe. Independent.
- Once an `allow` list exists, all unlisted subjects are denied (including `$JS.API.>`).
- No `permissions` block and no `allow`/`deny` = unrestricted.
- Empty allow list (`publish: []`) is NOT a lock-down; use `publish: { deny: [">"] }`.
- `deny` beats `allow` when both match.
- `allow_responses`: lets a service reply to requests without a broad publish allow; server tracks reply subjects it handed out and permits exactly that reply.
- A subscribe deny on `_INBOX.>` silently breaks request-reply (timeout, no error).

Source: same authorization page; allow_responses at lines of same page.

### 5.3 Accounts

| Item | Finding | Source |
|---|---|---|
| `accounts` block | `users`, `exports`, `imports`, `nkey`, `jetstream`, `default_permissions`, `msg_trace` | DOCS/reference/config/accounts |
| Account nkey | "public NKey that identifies this account (an A-prefixed public account key)" | DOCS/reference/config/accounts |
| System account | `system_account` (default `$SYS`) | DOCS/reference/config (summary) `[UNVERIFIED]` exact default string recheck |

### 5.4 Least-privilege agent (proposal input, not verified fact)

Derived design, not a source claim: an agent identity should publish only under its own subject prefix, subscribe only to its inbox prefix and its inbound-task subject, and use `allow_responses` for replies. JetStream access should be limited to specific `$JS.API.CONSUMER.MSG.NEXT.<stream>.<consumer>`, `$JS.ACK.>` (reply-side), and `$JS.API.STREAM.MSG.GET.<stream>` as needed; `$JS.API.>` must be denied. The `$KV`/`$O` subjects must be allowed explicitly if KV/Object used.

### 5.5 TLS on loopback

- `tls` block with `cert_file` and `key_file` (DOCS/reference/config/tls).
- `verify: true` requires client certs; `verify_and_map` maps cert identity to user.
- Windows: `cert_store` (Windows only).
- Self-signed loopback cert setup steps: `[UNVERIFIED]` (not fetched).

---

## 6. Federation

| Item | Finding | Source |
|---|---|---|
| Leaf node listener | `leafnodes { port: 7422, host: 0.0.0.0, tls, authorization, advertise, isolate_leafnode_interest }` | DOCS/reference/config/leafnodes/ |
| Leaf remotes | `leafnodes { remotes: [ { urls: [...], account, credentials, tls, dial_timeout (default 1s) } ] }`; `reconnect` interval | DOCS/reference/config/leafnodes/ ; DOCS/learn/topologies/leaf-nodes |
| Leaf behavior | Leaf opens one outbound connection to hub; subject interest forwarded both ways; leaves compose into trees | DOCS/learn/topologies/leaf-nodes |
| Leaf JetStream domain | `domain` on leaf isolates JetStream systems across leaf link; streams not automatically shared | DOCS/learn/topologies/leaf-nodes ; DOCS/reference/config/jetstream/domain |
| Leaf handshake fields | `domain`, `cluster`, `xkey`, `auth_required`, `tls_required`, `jetstream` | DOCS/reference/protocols/leafnode |
| Clustering | Routes form mesh; RAFT groups agree; quorum commit; placement; `R3` replicas | DOCS/running-a-nats-service/configuration/clustering |
| Cluster port | `[UNVERIFIED]` default route port (6222 is commonly cited, not confirmed here) | — |
| Super-clusters / gateways | Covered on leaf/cluster pages `[UNVERIFIED]` details | DOCS/learn/topologies |
| Accounts across servers | Accounts `exports`/`imports` (cross-account streams/services) | DOCS/reference/config/accounts ; DOCS/.../securing_nats/... (accounts and cross-account) |
| JetStream partitions | JetStream domains (`domain`) are the partition mechanism for JetStream in leafs; `[UNVERIFIED]` other partition terminology | DOCS/reference/config/jetstream/domain |

---

## 7. Client libraries

### 7.1 async-nats (Rust)

| Item | Finding | Source |
|---|---|---|
| Crate version | 0.50.0 | RS/async-nats/Cargo.toml ; https://docs.rs/async-nats/0.50.0 |
| rust-version | 1.89.0 | RS/async-nats/Cargo.toml |
| License | Apache-2.0 | RS/async-nats/Cargo.toml |
| Default features | `server_2_10`, `server_2_11`, `server_2_12`, `server_2_14`, `service`, `ring`, `jetstream`, `nkeys`, `crypto`, `object-store`, `kv`, `websockets`, `nuid` | RS/async-nats/Cargo.toml |
| Optional features | `aws-lc-rs`, `fips`, `chrono`, `nkeys`, `nuid`, `experimental`, `slow_tests`, `websockets` | RS/async-nats/Cargo.toml |
| `kv` | depends on `jetstream` | RS/async-nats/Cargo.toml |
| `object-store` | depends on `jetstream`, `crypto` | RS/async-nats/Cargo.toml |
| Embedded server | No. The `nats-server` crate in nats.rs is a test helper that spawns the external `nats-server` binary via `Command::new("nats-server")` and parses `Store Directory:` from logs; not published as an embedded server. | RS/nats-server/src/lib.rs |
| Windows support | Not confirmed. nats.rs CI (`ci.yml`) had no Windows matrix entry in the fetched copy. `[UNVERIFIED]` whether async-nats builds/tests on Windows. | RS/.github/workflows/ci.yml (fetched; grep no `windows`) |
| Maturity | Version 0.50.0 (pre-1.0 semver); release count and production adoption `[UNVERIFIED]` | RS/async-nats/Cargo.toml |

### 7.2 Go (nats.go) and others

- Go client: `GO/` jetstream package (KV, Object, OrderedConsumer). Windows support `[UNVERIFIED]` (not checked).
- Ack methods across clients: `DoubleAck`, `ackAck`, `ack_sync`, `ackSync`, `AckAsync`, `double_ack`. Source: DOCS/learn/jetstream/delivery-and-acknowledgment.

---

## 8. Zenoh comparison (low confidence)

| Aspect | NATS | Zenoh | Source |
|---|---|---|---|
| Model | Subject-based pub/sub, request/reply, JetStream persistence | Key-expression pub/sub, queries/queryables, storage plugins | https://zenoh.io/docs/overview/zenoh-in-action ; https://zenoh.io/docs/manual/abstractions |
| Persistence | JetStream streams/KV/Object | Storage plugin ("storages") | https://zenoh.io/docs/manual/abstractions |
| Routing | Server/cluster/leaf/super-cluster | Router network, peer-to-peer | https://zenoh.io/ (overview) `[UNVERIFIED]` peer details |
| Perf (single benchmark) | NATS ~34K msg/s at 5000 pairs | Zenoh ~50.8K msg/s at 5000 pairs | https://arxiv.org/html/2603.21600v1 (benchmark; low confidence, workload-specific) |
| Auth model | NKeys/JWT/accounts | Not verified | — |

Confidence: low. Only the model description and one benchmark were checked; no evaluation for this use case.

---

## 9. Facts the RFC can rely on (summary)

| Topic | Stable fact | Source |
|---|---|---|
| Subject taxonomy | dot tokens, `*` / `>` wildcards, case-sensitive, no whitespace | DOCS/nats-concepts/subjects |
| JS API prefix | `$JS.API.` | SRV/jetstream_api.go |
| Ack reply | `$JS.ACK.<domain>.<accHash>.<stream>.<consumer>.<dc>.<sseq>.<dseq>.<ts>.<pending>` | SRV/consumer.go |
| Dedup | `Nats-Msg-Id`, window 2m default | SRV/stream.go |
| Redelivery | `ack_wait` 30s, `max_ack_pending` 1000, `max_deliver` -1 | SRV/consumer.go |
| Max payload | 1MB; violation `Maximum Payload Violation` | SRV/const.go |
| KV | `KV_<b>` stream, `$KV.<b>.>`, history 64 | GO/kv.go |
| Object | `OBJ_<b>`, `$O.<b>.C.>`, `$O.<b>.M.>`, chunk 128KB | GO/object.go |
| Auth | NKeys (O/A/U prefixes), JWT chain, `allow` / `deny`, `allow_responses` | DOCS/.../securing_nats |
| Leaf | port 7422, `remotes.urls`, domain | DOCS/reference/config/leafnodes/ |
| Windows | `service_windows.go` supports SCM run mode; `cert_store` Windows-only | SRV/service_windows.go ; DOCS/reference/config/tls |

## 10. Open items (`[UNVERIFIED]`)

1. Exact `sc.exe` / service-install steps on Windows; Windows `store_dir` default.
2. Memory footprint per stream/consumer/server.
3. Cluster route port default and gateway default port.
4. async-nats Windows build/test status.
5. Ack wire tokens (`+ACK`, `-NAK`, `+TERM`, `+WPI`) exact spelling in server code.
6. Advisory subject names for max-deliveries and stream/consumer events.
7. Self-signed loopback TLS procedure.
8. Zenoh security and routing details beyond the cited pages.
