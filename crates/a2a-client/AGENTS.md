# a2a-client

Owns: the requester runtime (layer L3, RFC-0001 4.2): sending requests and decoding replies,
following task event streams with ordered delivery, discovering agents from the card and presence
stores, budget and approval messages, and the client-side hooks for signing and verification
(RFC-0007).

Provides:
- `Client<B: Bus>`: `new`, `send`, `send_retrying`, `send_and_wait`, `stream`, `subscribe`,
  `get_task`, `list_tasks`, `cancel_task`, `extended_card`, `push_*`, `decide` (config signing hook),
  `decide_signed` (explicit operator `SigningHook`, RFC-0007 [0007-038]).
- `Outbound`, `text`: request message builder (`message_id`, task and context ids, parts,
  budget, return-immediately). `transport_id` names the bus message id; `to_message` and
  `from_message` convert to and from the wire message so a journaled send can be repeated.
- `CallOptions`, `Config`, `SigningHook`, `VerifyHook`.
- `signing::envelope_hook(signer, clock)`: signs outgoing requests (RFC-0007 0007-032..040). No verify constructor: `VerifyHook` lacks the subject and replay/trust state that `verify_request` needs.
- `EventStream`, `Event`, `StreamStart`, `Outcome`, `SendReply`, `Unary`: task event delivery.
- `Directory`, `Query`, `Listing`, `CardRecord`, `VIEW_MAX_AGE_MS`: discovery view; `query` filters, `all` lists every card (both use the same offline and order rules).
- `PendingApproval`, `allow_once`: approval request reading.
- `Presence`, `PresenceState`: presence record decoding.
- `budget`: budget allowance and loop-chain helpers over `a2a_core::budget`.
- `ClientError`, `ApprovalError`: client error taxonomy.

Uses: `a2a-core` (wire envelopes, subjects, headers, budget, approval constants), `a2a-security`
(card verification, approval digest, trust store), `a2a-types` (ids, `proto::a2a`,
`proto::a2abus`, extension activation), `bus-api` (`Bus`, `Watch`, names), `bytes`,
`pbjson-types`, `serde`, `serde_json`, `thiserror`. Dev-dependencies: `bus-mem`, `tokio`.

The crate speaks only through `bus_api::bus::Bus`. It never opens sockets or NATS connections.

## Rules

- Layer rule [0001-001]: depends on `a2a-core`, `a2a-security`, `a2a-types`, and `bus-api`, never on
  a binding crate or a worker crate. Tests run on `bus-mem` with a virtual clock.
- The principal of every request is the transport principal the client was built with. It is never
  synthesized from a subject ([0002-011]).
- Gateway origin (`A2AB-Origin`) is sent only when the client's sender is the gateway identity
  ([0002-112]).
- Replies must echo the request message id as causation id; a mismatch is `InvalidReply`
  ([0002-051]).
- A stream ends at the first terminal or interrupted event ([0002-044], [0005-018]).
- Events are delivered in per-task sequence order. Duplicates (`seq <= last`) are dropped. A gap is
  buffered and repaired by replay; a gap that replay cannot repair is `RetentionExceeded`
  ([0005-017], [0010-077]).
- Retries reuse the outbound message id, so the worker deduplicates ([0005-034]). Only local
  deadline expiry is retried.
- Discovery fails closed: an unsynced view, a view older than 60 s by the local clock, or a card whose
  signature does not verify for its pinned agent id is never routable ([0004-020], [0004-039],
  [0004-044]). Card text is untrusted input; invalid records are dropped.
- Approval decisions name the request digest (`request_sha256`, [0006-051]). The client checks that
  the option is offered and the request has not expired before sending ([0006-013]).
- No `unwrap`/`expect`/`panic` outside tests. No `unsafe`. Payloads are `Bytes` and are not copied
  on the receive path.
- Gap handling replays: a gap re-attaches a consumer from the start of the retained stream and drops duplicates. A gap that replay cannot repair is `RetentionExceeded`, and the caller re-subscribes with `get` ([0002-043], [0005-017]). The client does not call `get` itself (RFC feedback).

## Tests and the requirements they guard

Test names carry the requirement ID (`req_0002_NNN_*`, `req_0004_NNN_*`, `req_0005_NNN_*`,
`req_0006_NNN_*`, `req_0010_NNN_*`).

- 0002-020, 0002-050, 0002-051, 0002-053: call shape, blocking default, causation, stream on send.
- 0002-060 to 0002-065: subscribe, `after_seq`, retention.
- 0002-112, 0002-113, 0002-114: origin, inline limits, blob references.
- 0004-001 to 0004-024: discovery view, presence, query ordering, staleness.
- 0005-034: retry idempotence.
- 0006-012 to 0006-015: decision shape, ordered checks, single use.
- 0010-077: first-attach gap handling.

## Deviations from RFC text

- Blocking send is the default ([0002-050]); the A2A `return_immediately` flag is the opt-out.
- Stream end is the first terminal or interrupted state (resolves A2A 3.1.2 against L3; see RFC
  feedback).
- Gap handling replays from the retained stream and does not call `get` itself. [0002-043] and
  [0005-017] differ on the repair path; the choice is recorded as RFC feedback.
- Discovery uses snapshot refresh. There is no live watch in M1, so the view is refreshed by
  `Directory::sync`, and it goes stale after 60 s.
- Presence has no protobuf schema (RFC-0004 0004-008). Presence and card records are read as
  proto-JSON camelCase.
