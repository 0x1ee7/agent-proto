#!/usr/bin/env python3
"""Architecture pages for the A2A-Bus repository.

The pages are drawn from reading the code under crates/ (module layout, public items, `use`
lines, Cargo.toml path dependencies, the approval and worker paths). Counts are computed at
build time from the sources; nothing is typed in from a document except the improvement
register, whose evidence is cited on each entry.

    python tools/architecture_diagrams.py              # all pages into docs/architecture/
    python tools/architecture_diagrams.py --only 02    # one page while iterating
    python tools/architecture_diagrams.py --list

Requires Python 3.11+ (tomllib) and Edge or Chrome for PDF output (CHROME overrides the path).
"""
import re
import sys
import tomllib
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
sys.path.insert(0, str(HERE))

import diagram_kit as kit  # noqa: E402
from diagram_kit import LayoutError  # noqa: E402

M = kit.MARGIN

# ---------------------------------------------------------------- improvement register
kit.register(
    kit.Improvement(
        "E-01", "high", "driver-acp (approval expiry)",
        "Live S4c passes (APPROVAL_TIMEOUT with detail), but after the expiry the omp backend stays closed: "
        "the next task fails BACKEND_CRASHED (BACKEND_START_FAILED: backend is closed) until the supervisor "
        "restarts (docs/spikes/e2e-m1.md, re-run 2026-10-27).",
        "Find where the backend is closed on approval expiry and why it is not respawned; fix it with a "
        "regression test; rerun S4c and the next task live and record it.",
        "backend not respawned after expiry"),
    kit.Improvement(
        "G-01", "high", "gateway-http (tls.rs)",
        "The TLS key must be owner-only on Unix, but Windows ACLs are not inspected "
        "(crates/gateway-http/src/tls.rs lines 1-4; RFC-0008 [0008-066], [0008-068]).",
        "Check the key ACL on Windows (icacls, as crates/a2ad/src/platform.rs does) and add a "
        "test for a key readable by other users.",
        "TLS key ACL"),
    kit.Improvement(
        "D-01", "medium", "a2ad (supervisor) and a2a-worker (crash path)",
        "Restart scenarios 0014-028 (supervisor killed) and 0014-029 (worker crash) are NOT RUN "
        "live (docs/spikes/e2e-m1.md, rows 0014-028 and 0014-029).",
        "Run both live on Windows with the spike harness and record the verdicts in e2e-m1.md.",
        "restarts not run live"),
    kit.Improvement(
        "P-01", "medium", "a2ad config and a2a-worker delegation",
        "delegation.auto_allow_peer_tools (RFC-0015 [0015-087], RFC-0006 [0006-061]) does not "
        "appear anywhere in crates/ (grep). Peer-tool calls always wait for an operator.",
        "Decide for M1: implement the key or move it out of M1 in the RFCs; record the decision.",
        "auto-allow absent"),
    kit.Improvement(
        "A-01", "low", "a2a-audit (gaplog.rs)",
        "audit-gaps.log has no rotation (crates/a2a-audit/AGENTS.md line 37; RFC-0013 [0013-039]).",
        "Add size or day rotation with the same retention rule as the JSONL sink.",
        "no gaps-log rotation"),
    kit.Improvement(
        "R-01", "low", "RFC-0006 line 36 (a2a-core budget.rs)",
        "RFC-0006 line 36 says the unlimited sentinel is valid at the root for wall time. "
        "[0006-032], the RFC-0006 table row 0006-032, and Allowance::validate (budget.rs line 129) "
        "refuse it for wall time.",
        "Correct line 36 to tokens and cost only. The code and [0006-032] agree.",
        "sentinel on wall time"),
)

# ---------------------------------------------------------------- repository scan
LAYERS = {
    "a2a-types": 0,
    "bus-api": 1, "bus-mem": 1, "bus-nats": 1,
    "a2a-core": 2, "a2a-security": 2, "a2a-workspace": 2, "a2a-audit": 2, "a2a-mcp": 2,
    "a2a-worker": 3, "a2a-client": 3,
    "driver-acp": 4, "driver-a2a-http": 4, "gateway-http": 4,
    "a2ad": 5, "a2a-tui": 5, "a2a-cli": 5,
    "a2a-conformance": 6,  # test crate, drawn outside the stack
}
ORDER = [  # rows top to bottom: highest layer first
    "a2ad", "a2a-tui", "a2a-cli",
    "gateway-http", "driver-acp", "driver-a2a-http",
    "a2a-worker", "a2a-client",
    "a2a-core", "a2a-security", "a2a-workspace", "a2a-audit", "a2a-mcp",
    "bus-nats", "bus-api", "bus-mem",
    "a2a-types", "a2a-conformance",
]
# RFC-0001 sec 4.2 lets the L1 bindings name bus-api; RFC-0015 [0015-001] lets the worker name
# a2a-client for outbound delegation. Any other same-layer edge stops the build.
SAME_LAYER_ALLOWED = {("a2a-worker", "a2a-client"), ("bus-mem", "bus-api"), ("bus-nats", "bus-api")}
TEST_ATTR = re.compile(r"#\[(?:[A-Za-z_][A-Za-z0-9_]*::)?test\b")
KIND_OF = {
    "a2ad": "app", "a2a-tui": "app", "a2a-cli": "app", "gateway-http": "app",
    "driver-acp": "app", "driver-a2a-http": "app",
    "bus-nats": "platform", "bus-mem": "tool", "a2a-conformance": "tool", "a2a-types": "data",
}
ROLE = {
    "a2ad": "supervisor, broker, child hosts",
    "a2a-tui": "operator terminal UI",
    "a2a-cli": "command line (binary a2a)",
    "gateway-http": "HTTP ingress, TLS, agent cards",
    "driver-acp": "ACP driver for omp and Claude Code",
    "driver-a2a-http": "driver for an A2A HTTP backend",
    "a2a-worker": "turn engine, approvals, delegation",
    "a2a-client": "peer client, signing, envelopes",
    "a2a-core": "tasks, approvals, budgets, envelopes",
    "a2a-security": "identity, trust, redaction, cards",
    "a2a-workspace": "workspace binding",
    "a2a-audit": "hash-chained audit, spool, sinks",
    "a2a-mcp": "MCP tool shim (a2abus-mcp-shim)",
    "bus-nats": "NATS JetStream binding",
    "bus-api": "Bus trait and message model",
    "bus-mem": "in-memory bus for tests",
    "a2a-types": "A2A v1.0 types and ids",
    "a2a-conformance": "conformance scenarios",
}
# Crates that carry a register item; the ID is drawn as a badge on their box.
BADGES = {"driver-acp": ["E-01"], "gateway-http": ["G-01"], "a2ad": ["D-01", "P-01"],
          "a2a-audit": ["A-01"], "a2a-core": ["R-01"]}

_SCAN = None


def scan():
    """Read crates/*/Cargo.toml (path dependencies) and count lines and test attributes."""
    global _SCAN
    if _SCAN is not None:
        return _SCAN
    crates = {}
    for toml in sorted((ROOT / "crates").glob("*/Cargo.toml")):
        data = tomllib.loads(toml.read_text(encoding="utf-8"))
        name = data["package"]["name"]
        deps = sorted(k for k, v in data.get("dependencies", {}).items()
                      if isinstance(v, dict) and "path" in v)
        loc = tests = 0
        for path in toml.parent.rglob("*.rs"):
            src = path.read_text(encoding="utf-8", errors="replace")
            loc += src.count("\n")
            tests += len(TEST_ATTR.findall(src))
        crates[name] = {"deps": deps, "loc": loc, "tests": tests}
    if set(LAYERS) != set(crates):
        raise LayoutError(f"crate list and LAYERS differ: {sorted(set(LAYERS) ^ set(crates))}")
    for name, c in crates.items():
        if LAYERS[name] == 6:
            continue
        for dep in c["deps"]:
            if LAYERS[dep] > LAYERS[name]:
                raise LayoutError(f"upward dependency {name} -> {dep}")
            if LAYERS[dep] == LAYERS[name] and (name, dep) not in SAME_LAYER_ALLOWED:
                raise LayoutError(f"same-layer dependency {name} -> {dep} is not allowed")
    _SCAN = crates
    return crates


def same_layer_edges():
    cr = scan()
    return sorted((r, d) for r in cr for d in cr[r]["deps"]
                  if LAYERS[r] == LAYERS[d] and LAYERS[r] != 6)


# ---------------------------------------------------------------- page 1: layers
def page_layers(p):
    cr = scan()
    bands = [(5, "L5  executables"), (4, "L4  drivers and gateway"), (3, "L3  worker and client"),
             (2, "L2  core libraries"), (1, "L1  bus abstraction and bindings"), (0, "L0  shared types")]
    band_h = 118
    y = p.top
    for layer, label in bands:
        names = [n for n in ORDER if LAYERS[n] == layer]
        p.group(M, y, p.W - 2 * M, band_h - 8, label)
        gap = 14.0
        bw = min(300.0, (p.W - 2 * M - 48 - gap * (len(names) - 1)) / len(names))
        x = M + 24
        for name in names:
            c = cr[name]
            by, bh = y + 28, 82
            p.box(x, by, bw, bh, KIND_OF.get(name, "core"), name,
                  [ROLE[name], f"{c['loc']:,} lines, {c['tests']} test attributes"], size=8.5)
            bx = x + 12
            for ident in BADGES.get(name, []):
                bx += p.badge(bx, by + bh - 10, ident) + 8
            x += bw + gap
        y += band_h
    p.cards(M, None, p.W - 2 * M, None, [
        ("Reading this page", "#22d3ee", [
            "Bands are the RFC-0001 layers (sec 4.1). A crate may depend on its own or a lower band.",
            "Drawn from reading the code; the layer table is RFC-0001 sec 4.2.",
        ]),
        ("Counts", "#34d399", [
            "Lines: every .rs file under the crate, tests included. Test attributes: #[test] and "
            "#[x::test], scanned at build time (not the runtime pass count).",
            f"{sum(len(c['deps']) for c in cr.values())} path dependencies in [dependencies] "
            "(dev-dependencies not drawn).",
        ]),
        ("Exceptions", "#fbbf24", [
            "Same-layer edges: " + "; ".join(f"{r} to {d}" for r, d in same_layer_edges()) + ".",
            "a2a-conformance is a test crate and sits outside the stack.",
        ]),
    ], bottom=p.bottom)


# ---------------------------------------------------------------- page 2: dependency grid
def page_grid(p):
    cr = scan()
    rows = ORDER
    cols = list(reversed(ORDER))
    label_w = 170.0
    x0 = M + label_w
    head_h = 52.0
    top = p.top
    cards = p.cards(M, None, p.W - 2 * M, None, [
        ("Reading the grid", "#22d3ee", [
            "Row uses column. Green: a lower layer. Amber: same layer (see the card on the right).",
            "Layers come from RFC-0001 sec 4.2; the dependency set is read from Cargo.toml.",
        ]),
        ("Checks run at build time", "#34d399", [
            "No dependency points up the stack; the build stops if one does.",
            "Same-layer edges: " + "; ".join(f"{r} to {d}" for r, d in same_layer_edges()) + ".",
        ]),
    ], bottom=p.bottom)
    grid_bottom = cards.y - 20
    row_h = (grid_bottom - top - head_h) / len(rows)
    cell_w = (p.W - M - 70 - x0) / len(cols)
    for j, name in enumerate(cols):
        cx = x0 + j * cell_w + cell_w / 2
        parts = name.split("-")
        lines = ["-".join(parts[:len(parts) // 2]), "-".join(parts[len(parts) // 2:])] \
            if len(name) > 11 and len(parts) > 1 else [name]
        for k, line in enumerate(lines):
            p.text(cx, top + 18 + k * 13, line, 8.5, kit.SUB, None, "middle", fit=cell_w - 4)
    grid_top = top + head_h
    for i, row in enumerate(rows):
        y = grid_top + i * row_h
        deps = set(cr[row]["deps"])
        layer_tag = "test" if LAYERS[row] == 6 else f"L{LAYERS[row]}"
        p.text(M, y + row_h / 2 + 4, f"{row}  {layer_tag}", 9.5, kit.WHITE, 600, fit=label_w - 8)
        for j, col in enumerate(cols):
            cx0 = x0 + j * cell_w
            p.rect(cx0, y, cell_w, row_h, "none", stroke=kit.GRID, width=0.8, rx=0)
            if row == col:
                p.rect(cx0 + cell_w / 2 - 2, y + row_h / 2 - 2, 4, 4, kit.FAINT, rx=2)
            elif col in deps:
                color = "#fbbf24" if LAYERS[col] == LAYERS[row] else "#34d399"
                p.circle(cx0 + cell_w / 2, y + row_h / 2, 5.5, color)
        p.text(x0 + len(cols) * cell_w + 10, y + row_h / 2 + 4, str(len(deps)), 9, kit.SUB, 600)
    p.text(x0 + len(cols) * cell_w + 10, grid_top - 10, "deps", 8.5, kit.SUB)


# ---------------------------------------------------------------- page 3: public API panels
def page_api(p):
    w = (p.W - 2 * M - 24) / 2
    panels = [
        ("a2a-core", "Tasks, approvals, budgets, envelopes",
         "Depends on a2a-types and bus-api (lower layers only).", [
            ("approval", "", ["Tier: Read, Write, Exec, DeleteOrMove",
                              "RequestState, Verdict, ApprovalBook", "ALLOW_ONCE_ID, REJECT_ONCE_ID"], []),
            ("task", "", ["TaskStore, Effect, Event, EventBody, TaskView", "TaskError, StoreError"], []),
            ("budget", "", ["Allowance, Metering, Usage", "UNLIMITED sentinel; validate(root)"], ["R-01"]),
            ("envelope, errors", "", ["Clock, SystemClock (envelope)", "WireError, LocalError (errors)"], []),
        ]),
        ("a2a-worker", "Turn engine",
         "Serves bus requests, runs backends, enforces approvals and budgets.", [
            ("backend", "", ["trait Backend: capabilities, health, attach, start",
                             "resolve_permission, cancel, shutdown"], []),
            ("worker", "", ["Worker: open, serve_one, pump, set_draining",
                            "serve_request, handle (rpc.rs)"], []),
            ("store, presence", "", ["FileStore; Publication, CardSigning, Timing"], []),
            ("delegation", "private: peer calls", ["pump_delegation (delegation/mod.rs)",
                                                   "outbound through a2a-client"], ["P-01"]),
        ]),
        ("a2a-audit", "Audit",
         "Hash-chained events, spool, publish pipeline and sinks.", [
            ("emitter", "", ["Emitter::seal, EmitterIdentity"], []),
            ("pipeline", "", ["Pipeline::emit, checkpoint, flush", "Publisher trait"], []),
            ("spool, sink", "", ["Spool::append, Overflow (Discard or Refuse)",
                                 "AuditSink, JsonlSink"], []),
            ("gap log", "", ["gaplog.rs: gap records", "no rotation of audit-gaps.log"], ["A-01"]),
        ]),
        ("bus-api", "Bus contract",
         "The trait every binding implements. bus-mem and bus-nats implement it.", [
            ("bus", "", ["trait Bus: capabilities, publish, request, serve",
                         "append, consume, kv_*, blob_*"], []),
            ("control", "", ["BusControl, Fault, DelayDist, Probability (fault injection)"], []),
            ("message", "", ["Headers, Entry, PublishAck, Reply, ReplyHandle"], []),
            ("streams", "", ["RequestStream, Consumer, Watch"], []),
        ]),
    ]
    h1 = max(p.panel_height(s) for *_, s in panels[:2])
    h2 = max(p.panel_height(s) for *_, s in panels[2:])
    ys = [p.top, p.top + h1 + 16]
    for idx, (name, title, tag, sections) in enumerate(panels):
        row = idx // 2
        p.panel(M + (idx % 2) * (w + 24), ys[row], w, h1 if row == 0 else h2,
                "core", name, f"{title}. {tag}", sections)


# ---------------------------------------------------------------- page 4: interface matrix
def page_interfaces(p):
    rows = ["a2ad", "a2a-cli", "a2a-tui", "gateway-http", "driver-acp", "driver-a2a-http",
            "a2a-worker", "a2a-client", "a2a-core"]
    cols = ["a2a-worker", "a2a-client", "a2a-core", "a2a-security", "a2a-audit", "bus-api", "bus-nats"]
    cells = {
        ("a2ad", "a2a-worker"): ["Worker, WorkerBuilder, FileStore", "backend traits, DelegationConfig",
                                 "Publication, CardSigning, WorkspaceBinding"],
        ("a2ad", "a2a-core"): ["Allowance, Metering", "Clock, SystemClock"],
        ("a2ad", "a2a-security"): ["Kid, Signer, storage"],
        ("a2ad", "a2a-audit"): ["Pipeline, PipelineConfig, Publisher", "Redactor, EmitterIdentity, Metrics"],
        ("a2ad", "bus-api"): ["Bus, BusError, Headers"],
        ("a2ad", "bus-nats"): ["NatsBus, connect, provision", "layout, permissions"],
        ("a2a-cli", "a2a-client"): ["Client, Config, SendReply", "ApprovalError, ClientError, SigningHook"],
        ("a2a-cli", "a2a-core"): ["SystemClock", "WireError, LocalError"],
        ("a2a-cli", "a2a-security"): ["Signer, storage keys", "TrustStore"],
        ("a2a-cli", "a2a-audit"): ["verify_paths, CheckpointVerifier"],
        ("a2a-cli", "bus-api"): ["Bus, BusError"],
        ("a2a-cli", "bus-nats"): ["NatsConfig, Seed"],
        ("a2a-tui", "a2a-client"): ["Event, PendingApproval", "signing envelope_hook"],
        ("a2a-tui", "a2a-core"): ["ALLOW_ONCE_ID, REJECT_ONCE_ID", "SystemClock"],
        ("a2a-tui", "a2a-security"): ["Signer, TrustStore", "storage"],
        ("a2a-tui", "bus-api"): ["Bus, Consumer, StartPosition"],
        ("a2a-tui", "bus-nats"): ["NatsConfig, Seed, layout"],
        ("gateway-http", "a2a-client"): ["Client, Directory, EventStream", "CallOptions, Outbound, SendReply"],
        ("gateway-http", "a2a-core"): ["Allowance, Clock", "WireError, BUS_DOMAIN, LocalError"],
        ("gateway-http", "bus-api"): ["Bus, BusError"],
        ("driver-acp", "a2a-worker"): ["backend traits", "check_turn_stream"],
        ("driver-acp", "a2a-core"): ["DEFAULT_EXPIRY_MS"],
        ("driver-a2a-http", "a2a-worker"): ["backend traits"],
        ("a2a-worker", "a2a-client"): ["Client, CallOptions, Directory", "Outbound, SendReply, budget"],
        ("a2a-worker", "a2a-core"): ["approval: ALLOW_ONCE_ID, Tier, MAX_OPEN_PER_TASK",
                                     "budget, task::TaskStore, envelope"],
        ("a2a-worker", "a2a-security"): ["card sign and verify, frame", "redact, Signer, TrustStore"],
        ("a2a-worker", "a2a-audit"): ["EventBuilder, Pipeline, AuditError"],
        ("a2a-worker", "bus-api"): ["Bus, RequestStream"],
        ("a2a-client", "a2a-core"): ["envelope, errors, headers, subject", "approval, DelegationRefused"],
        ("a2a-client", "a2a-security"): ["Ed25519Signer, Signer", "verify_card, sign_envelope, TrustStore"],
        ("a2a-client", "bus-api"): ["Bus, Consumer, Watch", "Headers, Reply"],
        ("a2a-core", "bus-api"): ["Headers, Reply, ReplyHandle", "InvalidName, Principal, Subject"],
    }
    forbidden = {
        ("a2a-core", "a2a-worker"): ["never: upward edge"],
    }
    layer = {n: LAYERS[n] for n in rows + cols}
    kinds = {n: KIND_OF.get(n, "core") for n in rows + cols}
    cards = p.cards(M, None, p.W - 2 * M, None, [
        ("Rule", "#fbbf24", [
            "Each cell lists the items the row imports from the column (read from use lines).",
            "Red-outlined cells mark a dependency the layer rule forbids; none is present.",
        ]),
        ("Scope", "#22d3ee", [
            "Rows are the consumers this page covers; a2a-mcp and a2a-workspace are not drawn.",
        ]),
    ], bottom=p.bottom)
    p.matrix(M, p.top, cards.y - 24, rows, cols, cells, kinds=kinds, layer=layer, forbidden=forbidden)


# ---------------------------------------------------------------- page 5: runtime lifecycle
def page_lifecycle(p):
    lanes = [
        ("a2ad start (Supervisor::run, crates/a2ad/src/supervisor.rs)", [
            ("layout, operator", ["ensure_layout, ensure_operator", "trust file under the roots"]),
            ("identities", ["check_path_budget, reclaim_stale", "ensure_identity per agent"]),
            ("health port", ["HealthServer::bind", "before the broker starts"]),
            ("broker", ["Broker::start, render_config", "NATS server for the bus"]),
            ("supervise", ["children run as: a2ad host <id>", "restarts with backoff"]),
        ], "D-01"),
        ("worker turn (Worker::serve_one, pump)", [
            ("bus request", ["requests.next() (bus binding)", "one request per serve_one"]),
            ("serve_request", ["request decoded and", "routed to the worker"]),
            ("handle", ["Worker::handle(inbound, now)", "rpc.rs"]),
            ("Backend::start", ["StartTurn; the backend emits", "BackendEvent through the sink"]),
            ("pump", ["backend events, health, cancels,", "deadlines, approval expiry, outbox"]),
        ], None),
    ]
    y = p.top
    box_w, step = 262.0, 296.0
    for title, steps, badge in lanes:
        p.group(M, y, p.W - 2 * M, 200, title)
        if badge:
            p.badge(p.W - M - 14, y + 17, badge, anchor="end")
        x = M + 30
        for i, (name, lines) in enumerate(steps):
            p.box(x, y + 40, box_w, 110, "core", name, lines, size=9)
            if i < len(steps) - 1:
                p.arrow((x + box_w + 2, y + 95), (x + step - 6, y + 95))
            x += step
        y += 230
    p.cards(M, None, p.W - 2 * M, None, [
        ("Source order", "#22d3ee", [
            "Startup order is the code order of Supervisor::run (supervisor.rs).",
            "Each child host is the same binary: current_exe host <id> --config <path>.",
        ]),
        ("Worker", "#34d399", [
            "Outbound peer calls keep serving bus requests while they wait (RFC-0015).",
            "pump runs on each wakeup: events, health, cancels, deadlines, expiry, outbox.",
        ]),
    ], bottom=p.bottom)


# ---------------------------------------------------------------- page 6: approvals
def page_approvals(p):
    tiers = [("Read", "read-only tools"),
             ("Write", "writes; every MCP tool is at least here"),
             ("Exec", "unknown tools default here"),
             ("DeleteOrMove", "deletes and moves")]
    p.text(M, p.top + 12, "assign_tier(known, mcp)", 10, kit.WHITE, 700)
    y = p.top + 26
    for name, note in tiers:
        p.box(M, y, 280, 66, "data", name, [note], size=9)
        p.arrow((M + 282, y + 33), (M + 330, y + 33))
        y += 80
    book_y = p.top + 120
    p.box(M + 336, book_y, 230, 96, "core", "ApprovalBook",
          ["one RequestState per request", "MAX_OPEN_PER_TASK open per task"], size=9)
    p.badge(M + 336, book_y - 12, "E-01", fit=None)
    states_x = M + 700
    states = [("Open", "waiting for an answer"), ("Allowed", "ALLOW_ONCE_ID"),
              ("Denied", "REJECT_ONCE_ID"), ("Expired", "expiry has passed"),
              ("Closed", "the task has ended")]
    sy = p.top + 20
    for name, note in states:
        p.box(states_x, sy, 220, 56, "core", name, [note], size=9)
        sy += 74
    mid = book_y + 48
    for k, label in enumerate(["answer: allow", "answer: reject", "expiry", "task ends"]):
        target = p.top + 20 + 28 + k * 74
        p.arrow((M + 566, mid), (states_x - 2, target), label=label,
                label_at=(M + 620, target + 14 if k else mid - 30))
    p.cards(M, None, p.W - 2 * M, None, [
        ("Timing (code values)", "#fbbf24", [
            "Approval expiry: DEFAULT_EXPIRY_MS = 300 s (crates/a2a-core/src/approval.rs).",
            "Driver permission backstop: DEFAULT_PERMISSION_TIMEOUT = 330 s; must exceed the "
            "expiry (check_permission_timeout, driver-acp config.rs).",
            "Driver turn idle: DEFAULT_TURN_IDLE_TIMEOUT = 600 s (driver-acp config.rs).",
        ]),
        ("Rule", "#34d399", [
            "Worker approval expiry is authoritative (commit 9a4172a). Live S4c confirms APPROVAL_TIMEOUT; "
            "the backend respawn after expiry is open (E-01).",
        ]),
    ], bottom=p.bottom)


# ---------------------------------------------------------------- page 7: audit path
def page_audit(p):
    stages = [
        ("Pipeline::emit", ["takes an EventBuilder", "returns the sequence number"]),
        ("Emitter::seal", ["redact, sequence, hash chain", "checkpoints signed per emitter"]),
        ("Spool::append", ["local spool", "Overflow: Discard or Refuse"]),
        ("Pipeline::flush", ["returns the count flushed", "checkpoint(peers) is separate"]),
    ]
    y = p.top + 120
    bw, gap = 280.0, 56.0
    x = M + 30
    for i, (name, lines) in enumerate(stages):
        p.box(x, y, bw, 110, "core", name, lines, size=9)
        if i < len(stages) - 1:
            p.arrow((x + bw + 2, y + 55), (x + bw + gap - 6, y + 55))
        x += bw + gap
    out_x = x
    p.box(out_x, p.top + 40, 300, 100, "core", "Publisher (trait)",
          ["publish(&AuditEvent, &[u8])", "sends sealed events downstream"], size=9)
    p.box(out_x, p.top + 190, 300, 100, "core", "JsonlSink",
          ["local JSONL with size rotation", "and retention (sink.rs)"], size=9)
    p.box(out_x, p.top + 340, 300, 80, "core", "audit-gaps.log", ["gap records; no rotation"], size=9)
    p.badge(out_x + 12, p.top + 340 + 92, "A-01")
    p.arrow((x - gap + 6, y + 40), (out_x - 2, p.top + 90))
    p.arrow((x - gap + 6, y + 70), (out_x - 2, p.top + 240))
    p.cards(M, None, p.W - 2 * M, None, [
        ("Source", "#22d3ee", [
            "Drawn from crates/a2a-audit/src: pipeline.rs (emit, checkpoint, flush), emitter.rs "
            "(seal), spool.rs, sink.rs, gaplog.rs.",
        ]),
        ("Not drawn", "#fbbf24", [
            "Chain verification (verify.rs, a2a audit verify) and the NATS publisher binding are "
            "outside this page.",
        ]),
    ], bottom=p.bottom)


# ---------------------------------------------------------------- page 8: improvement register
def page_register(p):
    areas = [
        ("app", "driver-acp", ["E-01"]),
        ("app", "gateway-http", ["G-01"]),
        ("app", "a2ad", ["D-01", "P-01"]),
        ("core", "a2a-audit", ["A-01"]),
        ("core", "a2a-core", ["R-01"]),
    ]
    order = ["E-01", "G-01", "D-01", "P-01", "A-01", "R-01"]
    notes = [
        "E-01 first: after an approval expiry the omp backend stays closed (live, 2026-10-27).",
        "G-01 next: RFC-0008 [0008-066] requires the owner-only key check on every platform.",
    ]
    kit.improvement_register(p, areas, order, notes)


# ---------------------------------------------------------------- wiring
SPECS = [
    kit.Spec("01-crates-by-layer", "Crates by layer",
             "Crates in the RFC-0001 layers, with line and test-attribute counts", page_layers),
    kit.Spec("02-dependency-grid", "Path dependencies",
             "Which crate uses which, read from Cargo.toml; the build stops on an upward edge", page_grid),
    kit.Spec("03-public-api", "Public API by crate",
             "Main public items of the core crates, grouped by module", page_api),
    kit.Spec("04-interfaces", "Interfaces between crates",
             "Items each consumer imports from each provider", page_interfaces),
    kit.Spec("05-runtime-lifecycle", "Runtime lifecycle",
             "Supervisor startup and one worker turn, in code order", page_lifecycle),
    kit.Spec("06-approvals", "Approval path",
             "Tier assignment, request states and the timing constants", page_approvals),
    kit.Spec("07-audit-path", "Audit path",
             "From event to spool, publisher and the local JSONL sink", page_audit),
    kit.Spec("08-improvement-register", "Improvement register",
             "Points found while drawing the pages, each with its evidence", page_register),
]

if __name__ == "__main__":
    kit.main(SPECS, ROOT / "docs" / "architecture",
             project="a2a-bus", generator="tools/architecture_diagrams.py",
             footer_right="drawn from reading crates/; counts computed at build time",
             root=ROOT)
