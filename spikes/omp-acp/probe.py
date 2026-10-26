"""omp `--mode acp` probe: stdlib-only ACP v1 client over stdio (newline-delimited JSON-RPC 2.0).

Spike code (throwaway per RFC-0014 sec 4.4). Scenarios: init, prompt, approval, nofs, cancel,
persist, exit. Each writes .spikes/omp-acp/results/<name>.json and transcripts/<name>.jsonl.
See spikes/omp-acp/README.md for the exact commands.
"""
from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import threading
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
SPIKE = ROOT / ".spikes" / "omp-acp"
MODEL = "stub/stub-model"  # scripted OpenAI-compatible stub (spikes/omp-acp/stub); isolated agent dir
MCP_SERVER = HERE / "mcp_probe_server.py"
PY = str(Path(sys.executable).resolve())

# RFC-0009 [0009-018]: no fs, no terminal, no auth.terminal, and NO elicitation capability.
INIT_PARAMS = {
    "protocolVersion": 1,
    "clientCapabilities": {"fs": {"readTextFile": False, "writeTextFile": False},
                           "terminal": False, "auth": {"terminal": False}},
    "clientInfo": {"name": "agent-proto-spike", "title": "agent-proto omp spike", "version": "0.0.0"},
}

# RFC-0009 [0009-068]: fixed environment allowlist; provider keys are never forwarded.
ENV_ALLOW = ["PATH", "SYSTEMROOT", "USERPROFILE", "HOME", "TEMP", "TMP", "LANG",
             "USERNAME", "COMSPEC", "PATHEXT", "WINDIR", "APPDATA", "LOCALAPPDATA", "PROGRAMDATA", "HOMEDRIVE", "HOMEPATH"]


def omp_path() -> str:
    p = shutil.which("omp")
    if not p:
        raise SystemExit("omp not on PATH")
    return str(Path(p).resolve())


AGENT_DIR = [SPIKE / "agent"]  # sc_resume may switch this for the restarted process


def child_env() -> dict:
    env = {k: os.environ[k] for k in ENV_ALLOW if k in os.environ}
    env["PI_NO_TITLE"] = "1"
    env["PI_CODING_AGENT_DIR"] = str(AGENT_DIR[0])  # isolated agent dir; ~/.omp is never read
    return env


# ---- process tree helpers (Windows: PowerShell CIM). Only descendants of the spawned PID are touched.
def process_table() -> list:
    ps = subprocess.run(
        ["powershell", "-NoProfile", "-Command",
         "Get-CimInstance Win32_Process | Select-Object ProcessId,ParentProcessId,Name | ConvertTo-Json -Compress"],
        capture_output=True, text=True, timeout=90)
    data = json.loads(ps.stdout or "[]")
    if isinstance(data, dict):
        data = [data]
    return [(d["ProcessId"], d["ParentProcessId"], d["Name"]) for d in data]


def descendants(pid: int, table: list) -> list:
    kids: dict = {}
    for p, pp, n in table:
        kids.setdefault(pp, []).append((p, n))
    out, stack = [], [pid]
    while stack:
        cur = stack.pop()
        for p, n in kids.get(cur, []):
            out.append({"pid": p, "name": n})
            stack.append(p)
    return out


def alive_of(pids: list) -> list:
    live = {p for p, _, _ in process_table()}
    return [p for p in pids if p in live]


class Client:
    def __init__(self, name: str, args: list, cwd: Path, policy=None):
        self.name = name
        self.t0 = time.time()
        self.events: list = []
        self.permissions: list = []
        self.unhandled: list = []
        self.elicitations: list = []
        self.elicit_policy = None  # callable(params, client) -> result dict; None leaves the request unanswered
        self.policy = policy or (lambda params: {"outcome": {"outcome": "cancelled"}})
        self.lock = threading.Lock()
        self.write_lock = threading.Lock()
        self.next_id = 0
        self.waiters: dict = {}
        (SPIKE / "logs").mkdir(parents=True, exist_ok=True)
        self.stderr_file = open(SPIKE / "logs" / f"{name}.stderr.log", "wb")
        self.args = args
        self.proc = subprocess.Popen(args, cwd=str(cwd), env=child_env(), stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, stderr=self.stderr_file, bufsize=0)
        self.reader = threading.Thread(target=self._read_loop, daemon=True)
        self.reader.start()

    def stamp(self) -> float:
        return round(time.time() - self.t0, 3)

    def _record(self, direction: str, msg) -> None:
        with self.lock:
            self.events.append({"t": self.stamp(), "dir": direction, "msg": msg})

    def _write(self, msg: dict) -> None:
        data = (json.dumps(msg, separators=(",", ":")) + "\n").encode("utf-8")
        self._record("->", msg)
        with self.write_lock:
            self.proc.stdin.write(data)
            self.proc.stdin.flush()

    def _read_loop(self) -> None:
        for raw in iter(self.proc.stdout.readline, b""):
            line = raw.strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                with self.lock:
                    self.events.append({"t": self.stamp(), "dir": "<-", "raw_non_json": line.decode("utf-8", "replace")[:300]})
                continue
            self._record("<-", msg)
            if "method" in msg and "id" in msg:
                self._on_request(msg)
            elif "method" not in msg:
                slot = self.waiters.get(msg.get("id"))
                if slot:
                    slot[1] = msg
                    slot[0].set()
        self._record("<-", {"eof": True})
        for slot in list(self.waiters.values()):
            slot[0].set()

    def _on_request(self, msg: dict) -> None:
        if msg["method"] == "elicitation/create" and self.elicit_policy is not None:
            answer = self.elicit_policy(msg["params"], self)
            self.elicitations.append({"t": self.stamp(), "request": msg["params"], "answer": answer})
            if answer is not None:
                self._write({"jsonrpc": "2.0", "id": msg["id"], "result": answer})
            return
        if msg["method"] == "session/request_permission":
            answer = self.policy(msg["params"])
            self.permissions.append({"t": self.stamp(), "request": msg["params"], "answer": answer})
            self._write({"jsonrpc": "2.0", "id": msg["id"], "result": answer})
        else:
            # Declined capability (RFC-0009 [0009-019]); -32601 is UNVERIFIED for omp.
            self.unhandled.append({"t": self.stamp(), "method": msg["method"], "params": msg.get("params")})
            self._write({"jsonrpc": "2.0", "id": msg["id"],
                         "error": {"code": -32601, "message": "Method not found: client declined capability"}})

    def send(self, method: str, params: dict) -> int:
        with self.lock:
            self.next_id += 1
            rid = self.next_id
            self.waiters[rid] = [threading.Event(), None]
        self._write({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        return rid

    def wait(self, rid: int, timeout: float):
        slot = self.waiters[rid]
        if not slot[0].wait(timeout):
            return {"probe_timeout_s": timeout}
        return slot[1] if slot[1] is not None else {"probe_eof": True}

    def call(self, method: str, params: dict, timeout: float = 120):
        return self.wait(self.send(method, params), timeout)

    def notify(self, method: str, params: dict) -> None:
        self._write({"jsonrpc": "2.0", "method": method, "params": params})

    def updates(self, start: int = 0, end: int | None = None) -> list:
        out = []
        for e in self.events[start:end]:
            m = e.get("msg", {})
            if e["dir"] == "<-" and m.get("method") == "session/update":
                u = m["params"]["update"]
                out.append({"t": e["t"], "kind": u.get("sessionUpdate"), "update": u})
        return out

    def wait_for_update(self, kind: str, timeout: float) -> bool:
        deadline = time.time() + timeout
        while time.time() < deadline:
            if any(u["kind"] == kind for u in self.updates()):
                return True
            time.sleep(0.2)
        return False

    def close(self, timeout: float = 20) -> dict:
        tree = descendants(self.proc.pid, process_table())
        t0 = time.time()
        self.proc.stdin.close()  # EOF on stdin is the documented shutdown signal
        try:
            rc = self.proc.wait(timeout)
            how = "exited after stdin EOF"
        except subprocess.TimeoutExpired:
            self.proc.kill()
            rc = self.proc.wait()
            how = f"no exit {timeout}s after stdin EOF; killed"
        dt = round(time.time() - t0, 2)
        self.reader.join(5)
        pids = [d["pid"] for d in tree]
        leftovers = alive_of(pids)
        killed = []
        for p in leftovers:  # clean up only our own descendants
            subprocess.run(["taskkill", "/PID", str(p), "/T", "/F"], capture_output=True)
            killed.append(p)
        self.stderr_file.close()
        return {"exit_code": rc, "how": how, "seconds": dt,
                "descendants_before_close": tree,
                "descendants_alive_after_exit": leftovers,
                "descendants_killed_by_probe": killed}

    def save(self) -> None:
        d = SPIKE / "transcripts"
        d.mkdir(parents=True, exist_ok=True)
        with open(d / f"{self.name}.jsonl", "w", encoding="utf-8") as f:
            for e in self.events:
                f.write(json.dumps(e, ensure_ascii=False) + "\n")


def policy_for(decision: str):
    want = {"allow": "allow_once", "reject": "reject_once", "cancel": None}[decision]

    def _p(params: dict) -> dict:
        if want:
            for o in params.get("options", []):
                if o.get("kind") == want:
                    return {"outcome": {"outcome": "selected", "optionId": o["optionId"]}}
        return {"outcome": {"outcome": "cancelled"}}
    return _p


def fresh_dir(*parts: str) -> Path:
    p = SPIKE.joinpath(*parts)
    shutil.rmtree(p, ignore_errors=True)
    p.mkdir(parents=True, exist_ok=True)
    return p


def overlay(mode: str) -> Path:
    d = SPIKE / "overlays"
    d.mkdir(parents=True, exist_ok=True)
    p = d / f"approval-{mode}.yml"
    p.write_text(f"tools:\n  approvalMode: {mode}\n", encoding="utf-8")
    return p


def omp_args(work: Path, session_dir: Path, mode: str, via: str = "flag", config: Path | None = None) -> list:
    args = [omp_path(), "--mode", "acp", "--cwd", str(work), "--session-dir", str(session_dir),
            "--model", MODEL, "--thinking", "off", "--no-title"]
    if via == "flag":
        args += ["--approval-mode", mode]
    else:
        args += ["--config", str(config or overlay(mode))]
    return args


def start(name: str, work: Path, session_dir: Path, mode: str, via: str = "flag", policy=None) -> Client:
    return Client(name, omp_args(work, session_dir, mode, via), work, policy)


def mcp_entry(name: str, command: str, args: list | None = None) -> dict:
    return {"name": name, "command": command, "args": args or [], "env": []}


def text_prompt(text: str) -> list:
    return [{"type": "text", "text": text}]


def new_session(c: Client, work: Path, mcp: list | None = None):
    r = c.call("session/new", {"cwd": str(work), "mcpServers": mcp or []})
    return r.get("result", {}).get("sessionId"), r


def summarize_updates(c: Client, start: int = 0, end: int | None = None) -> list:
    return [{"t": u["t"], "kind": u["kind"], "update": u["update"]} for u in c.updates(start, end)]


def finish(c: Client, out: dict) -> dict:
    out["close"] = c.close()
    c.save()
    (SPIKE / "results").mkdir(parents=True, exist_ok=True)
    (SPIKE / "results" / f"{c.name}.json").write_text(json.dumps(out, indent=1, default=str, ensure_ascii=False), encoding="utf-8")
    return out


# ---------------- scenarios ----------------

def sc_init(a):
    """Q1 initialize response; Q2 session/new with [], valid stdio mcpServer, bogus stdio mcpServer."""
    work = fresh_dir("work", "init")
    log = SPIKE / "logs" / "mcp-init.log"
    log.parent.mkdir(parents=True, exist_ok=True)
    log.unlink(missing_ok=True)
    c = start("init", work, SPIKE / "sessions" / "init", "always-ask", a.via)
    out = {"initialize": c.call("initialize", INIT_PARAMS)}
    out["session_new_empty_mcp"] = c.call("session/new", {"cwd": str(work), "mcpServers": []})
    out["session_new_stdio_mcp"] = c.call("session/new", {"cwd": str(work), "mcpServers": [
        mcp_entry("probe", PY, [str(MCP_SERVER), str(log)])]})
    out["session_new_bogus_mcp"] = c.call("session/new", {"cwd": str(work), "mcpServers": [
        mcp_entry("bogus", "C:/nonexistent/probe-missing.exe")]})
    time.sleep(3)
    out["mcp_server_log_after_new"] = log.read_text(encoding="utf-8").splitlines() if log.exists() else None
    return finish(c, out)


def sc_prompt(a):
    """Q3: trivial prompt under always-ask; update kinds, usage, stopReason."""
    work = fresh_dir("work", "prompt")
    c = start("prompt", work, SPIKE / "sessions" / "prompt", "always-ask", a.via)
    out = {"initialize_ok": "result" in c.call("initialize", INIT_PARAMS)}
    sid, new = new_session(c, work)
    out["session_new"] = new
    n0 = len(c.events)
    out["prompt_result"] = c.call("session/prompt", {"sessionId": sid, "prompt": text_prompt(
        "Reply with exactly the word OK and nothing else.")}, timeout=180)
    out["updates"] = summarize_updates(c, n0)
    out["update_kinds"] = sorted({u["kind"] for u in out["updates"]})
    out["permissions"] = c.permissions
    out["unhandled_client_calls"] = c.unhandled
    return finish(c, out)


def init_params_for(a) -> dict:
    """Default: no elicitation (RFC-0009 [0009-018]); --elicit advertises elicitation.form for Q4."""
    p = {**INIT_PARAMS, "clientCapabilities": dict(INIT_PARAMS["clientCapabilities"])}
    if getattr(a, "elicit", False):
        p["clientCapabilities"]["elicitation"] = {"form": {}}
    return p

def sc_approval(a):
    """Q4: write and read under always-ask / always-ask-cached with no elicitation capability."""
    work = fresh_dir("work", f"approval-{a.mode}-{a.decision}-{a.via}")
    (work / "read-me.txt").write_text("secret-42\n", encoding="utf-8")
    name = f"approval-{a.mode}-{a.decision}-{a.via}" + ("-elicit" if a.elicit else "")
    c = start(name, work, SPIKE / "sessions" / name, a.mode, a.via, policy_for(a.decision))
    out = {"mode": a.mode, "decision": a.decision, "via": a.via, "elicit": a.elicit,
           "initialize_ok": "result" in c.call("initialize", init_params_for(a))}
    sid, new = new_session(c, work)
    out["session_new_ok"] = bool(sid)
    n0 = len(c.events)
    out["prompt_write"] = c.call("session/prompt", {"sessionId": sid, "prompt": text_prompt(
        "WRITE_FILE: use your file write tool to create a file named written.txt in the current directory "
        "containing exactly: hello . Then reply DONE.")}, timeout=240)
    out["write_updates"] = summarize_updates(c, n0)
    n1 = len(c.events)
    out["prompt_read"] = c.call("session/prompt", {"sessionId": sid, "prompt": text_prompt(
        "READ_FILE: use your file read tool to read read-me.txt in the current directory, then reply with its "
        "content and nothing else.")}, timeout=240)
    out["read_updates"] = summarize_updates(c, n1)
    out["written_txt_exists"] = (work / "written.txt").exists()
    out["permissions"] = c.permissions
    out["unhandled_client_calls"] = c.unhandled
    return finish(c, out)


def sc_nofs(a):
    """Q5: fs/terminal declined; does omp still use its own write and bash tools?"""
    work = fresh_dir("work", "nofs")
    c = start("nofs", work, SPIKE / "sessions" / "nofs", "yolo", a.via)
    out = {"initialize_ok": "result" in c.call("initialize", INIT_PARAMS)}
    sid, _ = new_session(c, work)
    n0 = len(c.events)
    out["prompt"] = c.call("session/prompt", {"sessionId": sid, "prompt": text_prompt(
        "WRITE_FILE: use your file write tool to create fs-test.txt containing exactly: ok .")}, timeout=240)
    out["prompt_bash"] = c.call("session/prompt", {"sessionId": sid, "prompt": text_prompt(
        "RUN_ECHO: use your bash tool to run: echo shell-ok . Then reply DONE.")}, timeout=240)
    out["updates"] = summarize_updates(c, n0)
    out["written_txt_exists"] = (work / "written.txt").exists()
    out["permissions"] = c.permissions
    out["unhandled_client_calls"] = c.unhandled
    return finish(c, out)


def sc_cancel(a):
    """Q6: session/cancel mid-turn (bash ping). Q8: children of the cancelled tool."""
    work = fresh_dir("work", "cancel")
    c = start("cancel", work, SPIKE / "sessions" / "cancel", "yolo", a.via)
    out = {"initialize_ok": "result" in c.call("initialize", INIT_PARAMS)}
    sid, _ = new_session(c, work)
    rid = c.send("session/prompt", {"sessionId": sid, "prompt": text_prompt(
        "RUN_PING: use your bash tool to run exactly: ping -n 60 127.0.0.1 > nul . Then reply DONE.")})
    deadline = time.time() + 150
    while time.time() < deadline and c.waiters[rid][1] is None and not c.wait_for_update("tool_call", 1):
        pass
    out["tool_call_seen_before_cancel"] = any(u["kind"] == "tool_call" for u in c.updates())
    time.sleep(4)
    tree_before_cancel = descendants(c.proc.pid, process_table())
    out["tree_before_cancel"] = tree_before_cancel
    t_cancel = c.stamp()
    c.notify("session/cancel", {"sessionId": sid})
    out["prompt_result_after_cancel"] = c.wait(rid, 90)
    out["prompt_latency_s"] = round(c.stamp() - t_cancel, 2)
    time.sleep(3)
    out["updates_after_cancel"] = [u for u in summarize_updates(c) if u["t"] >= t_cancel]
    out["permissions"] = c.permissions
    out["unhandled_client_calls"] = c.unhandled
    pids = [d["pid"] for d in tree_before_cancel]
    out["tree_pids_alive_after_cancel_and_response"] = alive_of(pids)
    # Follow-up prompt after cancel: does the session still accept a turn?
    out["followup_prompt"] = c.call("session/prompt", {"sessionId": sid, "prompt": text_prompt(
        "Reply with exactly the word OK and nothing else.")}, timeout=180)
    return finish(c, out)


def sc_persist(a):
    """Q7: session/load and session/resume after process restart, mcpServers passed."""
    work = fresh_dir("work", "persist")
    sdir = fresh_dir("sessions", "persist")
    log = SPIKE / "logs" / "mcp-persist.log"
    log.unlink(missing_ok=True)
    mcp = [mcp_entry("probe", PY, [str(MCP_SERVER), str(log)])]
    c1 = start("persist-a", work, sdir, "always-ask", a.via)
    out1 = {"initialize_ok": "result" in c1.call("initialize", INIT_PARAMS)}
    sid, new = new_session(c1, work, mcp)
    out1["session_new_with_mcp"] = new
    out1["prompt"] = c1.call("session/prompt", {"sessionId": sid, "prompt": text_prompt(
        "Remember this code word for later: PINEAPPLE. Reply with OK.")}, timeout=180)
    out1["persisted_files"] = sorted(str(p.relative_to(sdir)) for p in sdir.rglob("*") if p.is_file())
    out1["close"] = c1.close()
    c1.save()

    c2 = start("persist-b", work, sdir, "always-ask", a.via)
    out = {"restart_run_a": {k: v for k, v in out1.items()}, "session_id_a": sid}
    init2 = c2.call("initialize", INIT_PARAMS)
    out["initialize_b"] = init2
    caps = init2.get("result", {}).get("agentCapabilities", {})
    out["loadSession_advertised"] = caps.get("loadSession")
    out["sessionCapabilities"] = caps.get("sessionCapabilities")
    i0 = len(c2.events)
    out["session_load_with_mcp"] = c2.call("session/load", {"sessionId": sid, "cwd": str(work),
                                                            "mcpServers": mcp}, timeout=180)
    i1 = len(c2.events)
    out["replay_updates_during_load"] = summarize_updates(c2, i0, i1)
    out["session_resume_with_mcp"] = c2.call("session/resume", {"sessionId": sid, "cwd": str(work),
                                                                "mcpServers": mcp}, timeout=120)
    out["prompt_after_load"] = c2.call("session/prompt", {"sessionId": sid, "prompt": text_prompt(
        "CODE_WORD: what code word did I ask you to remember? Reply with the word only.")}, timeout=180)
    out["mcp_server_log"] = log.read_text(encoding="utf-8").splitlines() if log.exists() else None
    out["unhandled_client_calls"] = c2.unhandled
    return finish(c2, out)


def sc_exit(a):
    """Q8: plain stdin-EOF shutdown with no session."""
    work = fresh_dir("work", "exit")
    c = start("exit", work, SPIKE / "sessions" / "exit", "always-ask", a.via)
    out = {"initialize_ok": "result" in c.call("initialize", INIT_PARAMS)}
    return finish(c, out)


def elicit_for(a):
    """Q1 to Q5: answer policy for elicitation/create. Client.elicitations records each request and answer."""
    def _e(params: dict, client: Client):
        if a.answer == "accept":
            return {"action": "accept", "content": {"value": a.value}}
        if a.answer == "decline":
            return {"action": "decline"}
        if a.answer == "cancel":
            return {"action": "cancel"}
        if a.answer == "cancel-notify":
            client.notify("session/cancel", {"sessionId": params["sessionId"]})
            return {"action": "cancel"}
        if a.answer == "notify-only":
            client.notify("session/cancel", {"sessionId": params["sessionId"]})
            return None  # cancel while the elicitation is pending; the elicitation is never answered
        return None  # cancel-notify-silent: leave the elicitation unanswered, no session/cancel
    return _e


def sc_elicit(a):
    """Q1 to Q3, Q5: elicitation/create answers for write, read and bash under a given approval mode."""
    name = f"elicit-{a.mode}-{a.answer}" + (f"-{a.value.replace(' ', '_')}" if a.answer == "accept" else "")
    if a.perm != "allow":
        name += f"-perm-{a.perm}"
    if a.config_yaml:
        name += "-cfg"
    work = fresh_dir("work", name)
    (work / "read-me.txt").write_text("secret-42\n", encoding="utf-8")
    cfg, via = None, "flag"
    if a.config_yaml:
        cfg = SPIKE / "overlays" / f"{name}.yml"
        cfg.parent.mkdir(parents=True, exist_ok=True)
        cfg.write_text(a.config_yaml.replace("\\n", "\n"), encoding="utf-8")
        via = "config"
    c = Client(name, omp_args(work, SPIKE / "sessions" / name, a.mode, via, cfg), work)
    c.elicit_policy = elicit_for(a)
    c.policy = policy_for(a.perm)
    out = {"mode": a.mode, "answer": a.answer, "value": a.value, "config_yaml": a.config_yaml}
    caps = {**INIT_PARAMS["clientCapabilities"], "elicitation": {"form": {}}}
    out["initialize_ok"] = "result" in c.call("initialize", {**INIT_PARAMS, "clientCapabilities": caps})
    sid, _ = new_session(c, work)
    write_text = ("WRITE_FILE: use your file write tool to create written.txt containing exactly: hello . "
                  "Then reply DONE.")
    read_text = "READ_FILE: use your file read tool to read read-me.txt, then reply with its content and nothing else."
    steps = [("write1", write_text), ("read1", read_text),
             ("bash1", "RUN_ECHO: use your bash tool to run: echo shell-ok . Then reply DONE."),
             ("write2", write_text), ("read2", read_text)]
    for key, text in steps:
        n0 = len(c.events)
        out[key] = c.call("session/prompt", {"sessionId": sid, "prompt": text_prompt(text)},
                          timeout=40 if a.answer == "cancel-notify-silent" else 120)
        out[key + "_updates"] = summarize_updates(c, n0)
        if a.answer == "cancel-notify-silent":
            break
    out["elicitations"] = c.elicitations
    out["permissions"] = c.permissions
    out["unhandled_client_calls"] = c.unhandled
    out["written_txt_exists"] = (work / "written.txt").exists()
    return finish(c, out)


def resume_client(name: str, work: Path, sdir: Path, a) -> Client:
    """Start omp; with --default-session-dir the --session-dir flag is omitted (omp's default sessions location)."""
    args = omp_args(work, sdir, "always-ask")
    if a.default_session_dir:
        i = args.index("--session-dir")
        del args[i:i + 2]
    return Client(name, args, work)


def sc_resume(a):
    """Q6: which conditions let session/load and session/resume succeed after a restart."""
    name = ("resume-" + ("same-process" if a.same_process else "restart") + "-"
            + ("close" if a.close_before else "noclose") + "-" + a.load_cwd + "cwd"
            + ("-list" if a.list_first else "") + ("-defaultdir" if a.default_session_dir else "")
            + ("-agentdirb" if a.agent_dir_b else ""))
    work = fresh_dir("work", name)
    work_b = fresh_dir("work", name + "-b") if a.load_cwd == "other" else work
    sdir = fresh_dir("sessions", name)
    c1 = resume_client(name + "-a", work, sdir, a)
    out = {"variant": {"same_process": a.same_process, "close_before": a.close_before,
                       "load_cwd": a.load_cwd, "list_first": a.list_first}}
    out["initialize_a"] = "result" in c1.call("initialize", INIT_PARAMS)
    sid, new = new_session(c1, work)
    out["session_new"] = new
    out["prompt_a"] = c1.call("session/prompt", {"sessionId": sid, "prompt": text_prompt(
        "Remember this code word for later: PINEAPPLE. Reply with OK.")}, timeout=180)
    out["persisted_files"] = sorted(str(p.relative_to(sdir)) for p in sdir.rglob("*") if p.is_file())
    if a.close_before:
        out["session_close_a"] = c1.call("session/close", {"sessionId": sid})
    if a.same_process:
        c2 = c1
    else:
        out["close_a"] = c1.close()
        c1.save()
        if a.agent_dir_b:
            agent_b = SPIKE / "agent-b"
            shutil.rmtree(agent_b, ignore_errors=True)
            agent_b.mkdir(parents=True)
            for f in ("models.yml", "config.yml"):
                shutil.copy(SPIKE / "agent" / f, agent_b / f)
            AGENT_DIR[0] = agent_b
        c2 = resume_client(name + "-b", work_b, sdir, a)
        out["initialize_b"] = "result" in c2.call("initialize", INIT_PARAMS)
    if a.list_first:
        out["session_list"] = c2.call("session/list", {"cwd": str(work_b)})
    out["session_load"] = c2.call("session/load", {"sessionId": sid, "cwd": str(work_b), "mcpServers": []},
                                  timeout=120)
    out["session_resume"] = c2.call("session/resume", {"sessionId": sid, "cwd": str(work_b), "mcpServers": []},
                                    timeout=120)
    out["prompt_b"] = c2.call("session/prompt", {"sessionId": sid, "prompt": text_prompt(
        "CODE_WORD: what code word did I ask you to remember? Reply with the word only.")}, timeout=120)
    return finish(c2, out)


SCENARIOS = {"init": sc_init, "prompt": sc_prompt, "approval": sc_approval, "nofs": sc_nofs,
             "cancel": sc_cancel, "persist": sc_persist, "exit": sc_exit, "elicit": sc_elicit,
             "resume": sc_resume}


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("scenario", choices=sorted(SCENARIOS))
    ap.add_argument("--mode", default="always-ask", choices=["always-ask", "always-ask-cached", "write", "yolo"])
    ap.add_argument("--decision", default="allow", choices=["allow", "reject", "cancel"])
    ap.add_argument("--via", default="flag", choices=["flag", "config"],
                    help="flag: --approval-mode; config: --config overlay file")
    ap.add_argument("--elicit", action="store_true",
                    help="approval only: advertise clientCapabilities.elicitation.form (probe declines elicitation/create)")
    ap.add_argument("--answer", default="accept",
                    choices=["accept", "decline", "cancel", "cancel-notify", "cancel-notify-silent", "notify-only"],
                    help="elicit scenario: answer to elicitation/create")
    ap.add_argument("--value", default="Approve", help="elicit scenario: enum value sent on accept")
    ap.add_argument("--config-yaml", default=None,
                    help="elicit scenario: overlay YAML (use literal \\n) passed with --config")
    ap.add_argument("--perm", default="allow", choices=["allow", "reject", "cancel"],
                    help="elicit scenario: answer to session/request_permission (bash)")
    ap.add_argument("--same-process", action="store_true", help="resume: load in the same process (no restart)")
    ap.add_argument("--close-before", action="store_true", help="resume: session/close before restart")
    ap.add_argument("--load-cwd", default="same", choices=["same", "other"], help="resume: cwd used after restart")
    ap.add_argument("--list-first", action="store_true", help="resume: session/list before load")
    ap.add_argument("--default-session-dir", action="store_true",
                    help="resume: omit --session-dir (omp default sessions location)")
    ap.add_argument("--agent-dir-b", action="store_true",
                    help="resume: restarted process uses a second agent dir (copies stub models.yml/config.yml)")
    a = ap.parse_args()
    res = SCENARIOS[a.scenario](a)
    print(json.dumps(res, indent=1, default=str, ensure_ascii=False))


if __name__ == "__main__":
    main()
