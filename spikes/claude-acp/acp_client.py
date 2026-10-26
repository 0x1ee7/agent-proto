#!/usr/bin/env python3
"""Stdlib ACP client for the claude-agent-acp spike.

Newline-delimited JSON-RPC 2.0 over the child's stdio. Every frame in and out
is written to .spikes/claude-acp/out/<scenario>.jsonl (emails and org fields
redacted). See spikes/claude-acp/README.md for the exact commands.

--stub runs the adapter (and claude -p) against stub_anthropic.py, a local
Messages API stub, with a fake key and a throwaway CLAUDE_CONFIG_DIR. Results
from --stub runs are observed-with-stub, not real-model results.
"""
import argparse
import json
import os
import pathlib
import shutil
import signal
import subprocess
import sys
import threading
import time

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import stub_anthropic  # noqa: E402

ROOT = pathlib.Path(__file__).resolve().parents[2]
SPIKE = ROOT / ".spikes" / "claude-acp"
NODE = ROOT / ".spikes" / "node" / "node-v24.22.1-win-x64" / "node.exe"
ENTRY = SPIKE / "node_modules" / "@agentclientprotocol" / "claude-agent-acp" / "dist" / "index.js"
OUT = SPIKE / "out"
WORK = SPIKE / "work"
CFG = SPIKE / "cfg"
REDACT_KEYS = {"email", "orgId", "orgName", "accountId", "organization"}
# RFC-0009 [0009-069]: never hand the child a provider key; strip before spawning.
STRIPPED_ENV = ("ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN")
DEFAULT_SETTINGS_META = {"claudeCode": {"options": {"settings": {"permissions": {"defaultMode": "default"}}}}}


def child_env():
    return {k: v for k, v in os.environ.items() if k not in STRIPPED_ENV}


def stub_env(url, cfg_dir):
    """Child env for stub runs: fake key, local base URL, throwaway config dir."""
    env = child_env()
    env["ANTHROPIC_API_KEY"] = stub_anthropic.FAKE_KEY
    env["ANTHROPIC_BASE_URL"] = url
    env["CLAUDE_CONFIG_DIR"] = str(cfg_dir)
    return env


def stub_setup(name):
    """Fresh workdir, config dir and stub server for one scenario run."""
    wd = new_workdir(name)
    cfg = CFG / name
    shutil.rmtree(cfg, ignore_errors=True)
    cfg.mkdir(parents=True)
    stub = stub_anthropic.start_stub(wd, OUT / f"stub-{name}.jsonl")
    return wd, stub, stub_env(stub.url, cfg)


def redact(obj):
    if isinstance(obj, dict):
        return {k: ("<redacted>" if k in REDACT_KEYS and isinstance(v, str) else redact(v)) for k, v in obj.items()}
    if isinstance(obj, list):
        return [redact(x) for x in obj]
    return obj


def new_workdir(name):
    d = WORK / f"{name}-{time.strftime('%Y%m%d-%H%M%S')}"
    d.mkdir(parents=True, exist_ok=True)
    return d


class RpcError(Exception):
    pass


class Agent:
    """One adapter process plus a reader thread that answers agent->client requests."""

    def __init__(self, name, argv, cwd, perm="allow_once", fs_mode="decline", new_group=False, env=None):
        OUT.mkdir(parents=True, exist_ok=True)
        self.name = name
        self.perm = perm          # allow_once | reject_once | cancel | hold
        self.fs_mode = fs_mode    # decline | advertise (advertised fs, still answered with -32601)
        self.log = open(OUT / f"{name}.jsonl", "w", encoding="utf-8")
        self.errlog = open(OUT / f"{name}.stderr.log", "w", encoding="utf-8")
        self.lock = threading.Lock()
        self.pending = {}
        self.notes = []
        self.requests_in = []
        self.permissions = []
        self.held = []
        self.next_id = 0
        self.t0 = time.time()
        flags = subprocess.CREATE_NEW_PROCESS_GROUP if new_group else 0
        self.p = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=self.errlog, cwd=str(cwd),
                                  env=child_env() if env is None else env, creationflags=flags)
        self.reader = threading.Thread(target=self._read_loop, daemon=True)
        self.reader.start()

    def _rec(self, direction, msg):
        line = {"t": round(time.time() - self.t0, 3), "dir": direction, "msg": redact(msg)}
        self.log.write(json.dumps(line, ensure_ascii=False) + "\n")
        self.log.flush()

    def _write(self, msg):
        data = (json.dumps(msg) + "\n").encode("utf-8")
        with self.lock:
            self._rec("->", msg)
            self.p.stdin.write(data)
            self.p.stdin.flush()

    def _read_loop(self):
        for raw in self.p.stdout:
            text = raw.decode("utf-8", "replace").strip()
            if not text:
                continue
            try:
                msg = json.loads(text)
            except ValueError:
                self._rec("<-!", {"raw": text[:300]})
                continue
            self._rec("<-", msg)
            if "method" in msg and "id" in msg:
                threading.Thread(target=self._answer, args=(msg,), daemon=True).start()
            elif "method" in msg:
                self.notes.append(msg)
            else:
                slot = self.pending.get(msg.get("id"))
                if slot:
                    slot["msg"] = msg
                    slot["ev"].set()
        self._rec("<-EOF", {"t_eof": round(time.time() - self.t0, 3)})
        for slot in list(self.pending.values()):
            if slot["msg"] is None:
                slot["msg"] = {"error": {"code": -1, "message": "EOF"}}
                slot["ev"].set()

    def _answer(self, msg):
        method, rid, params = msg["method"], msg["id"], msg.get("params") or {}
        self.requests_in.append({"method": method, "params_keys": sorted(params.keys())})
        if method == "session/request_permission":
            self.permissions.append(params)
        if method == "session/request_permission":
            options = params.get("options", [])
            if self.perm == "hold":
                self.held.append({"id": rid, "options": options})
                return
            if self.perm == "cancel":
                outcome = {"outcome": "cancelled"}
            else:
                want = "allow_once" if self.perm == "allow_once" else "reject_once"
                opt = next((o for o in options if o.get("kind") == want), None)
                outcome = {"outcome": "selected", "optionId": opt["optionId"]} if opt else {"outcome": "cancelled"}
            self._write({"jsonrpc": "2.0", "id": rid, "result": {"outcome": outcome}})
            return
        self._write({"jsonrpc": "2.0", "id": rid,
                     "error": {"code": -32601, "message": f"spike client declines {method}"}})

    def request(self, method, params, timeout=180):
        with self.lock:
            self.next_id += 1
            rid = self.next_id
            slot = {"ev": threading.Event(), "msg": None}
            self.pending[rid] = slot
        self._write({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        if not slot["ev"].wait(timeout):
            raise TimeoutError(f"{method} timed out after {timeout}s")
        msg = slot["msg"]
        if "error" in msg:
            raise RpcError(json.dumps(msg["error"]))
        return msg["result"]

    def notify(self, method, params):
        self._write({"jsonrpc": "2.0", "method": method, "params": params})

    def answer_held(self, outcome):
        for h in self.held:
            self._write({"jsonrpc": "2.0", "id": h["id"], "result": {"outcome": outcome}})
        self.held = []

    def wait_for(self, pred, timeout=60):
        end = time.time() + timeout
        while time.time() < end:
            if any(pred(n) for n in self.notes) or self.held:
                return True
            time.sleep(0.1)
        return False

    def close_stdin(self):
        try:
            self.p.stdin.close()
        except OSError:
            pass

    def finish(self, timeout=20):
        """Close stdin, wait for natural exit, record exit code."""
        self.close_stdin()
        try:
            code = self.p.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            code = None
        self.log.close()
        self.errlog.close()
        return code


def init_params(auth_terminal=False, advertise_fs=False):
    return {
        "protocolVersion": 1,
        "clientCapabilities": {
            "fs": {"readTextFile": advertise_fs, "writeTextFile": advertise_fs},
            "terminal": False,
            "auth": {"terminal": auth_terminal},
        },
        "clientInfo": {"name": "agent-proto-spike", "title": "agent-proto spike", "version": "0.0.0"},
    }


def start(name, perm="allow_once", fs_mode="decline", new_group=False, env=None):
    return Agent(name, [str(NODE), str(ENTRY)], cwd=SPIKE, perm=perm, fs_mode=fs_mode,
                 new_group=new_group, env=env)


def summarize_notes(agent):
    kinds, usage = {}, []
    for n in agent.notes:
        p = n.get("params", {})
        if n["method"] == "session/update":
            k = p.get("update", {}).get("sessionUpdate", "?")
            kinds[k] = kinds.get(k, 0) + 1
            if k == "usage_update":
                usage.append({x: p["update"].get(x) for x in ("used", "size", "cost")})
        else:
            kinds[n["method"]] = kinds.get(n["method"], 0) + 1
    return {"notification_kinds": kinds, "usage_updates": usage}


def tool_summary(agent):
    out = []
    for n in agent.notes:
        if n["method"] == "session/update":
            u = n["params"].get("update", {})
            if u.get("sessionUpdate") in ("tool_call", "tool_call_update"):
                out.append({k: u.get(k) for k in ("sessionUpdate", "title", "kind", "status")})
    return out


def permission_summary(agent):
    return [{"toolCall": {k: p.get("toolCall", {}).get(k) for k in ("title", "kind")},
             "options": [{"optionId": o.get("optionId"), "kind": o.get("kind"), "name": o.get("name")}
                         for o in p.get("options", [])]}
            for p in agent.permissions]


def agent_text(agent):
    return "".join(n["params"]["update"].get("content", {}).get("text", "") for n in agent.notes
                   if n["method"] == "session/update" and n["params"]["update"].get("sessionUpdate") == "agent_message_chunk")


def auth_status(agent):
    return [n["params"] for n in agent.notes if n["method"] == "_auth/status_update"]


def scenario_init(args):
    stub = None
    env = None
    if args.stub:
        _wd, stub, env = stub_setup("init-stub")
    a = start("init", env=env)
    try:
        r = a.request("initialize", init_params(args.auth_terminal, args.advertise_fs))
        a.wait_for(lambda n: n["method"] == "_auth/status_update", timeout=8)
        out = {
            "protocolVersion": r.get("protocolVersion"),
            "agentCapabilities": r.get("agentCapabilities"),
            "agentInfo": r.get("agentInfo"),
            "authMethods": r.get("authMethods"),
            "auth_status_update": redact(auth_status(a)),
        }
        print(json.dumps(out, indent=1))
    finally:
        print("exit_code_after_stdin_close:", a.finish())
        if stub:
            stub.close()


def open_session(a, cwd, meta=None):
    a.request("initialize", init_params(False, a.fs_mode == "advertise"))
    return a.request("session/new", {"cwd": str(cwd), "mcpServers": [],
                                     "_meta": meta if meta is not None else DEFAULT_SETTINGS_META})


def meta_for_mode(mode):
    return {"claudeCode": {"options": {"settings": {"permissions": {"defaultMode": mode}}}}}


def scenario_write(args):
    stub = None
    env = None
    if args.stub:
        cwd, stub, env = stub_setup("write-stub")
        if args.disk_mode:
            (CFG / "write-stub" / "settings.json").write_text(
                json.dumps({"permissions": {"defaultMode": args.disk_mode}}), encoding="utf-8")
        prompt = "STUB:WRITE:probe.txt"
    else:
        cwd = new_workdir("write")
        prompt = ("Use the Write tool to create probe.txt in the current directory with the content hello. "
                  "Then reply DONE.")
    a = start("write", perm=args.perm, fs_mode=args.fs_mode, env=env)
    try:
        s = open_session(a, cwd, {} if args.disk_mode else meta_for_mode(args.default_mode))
        sid = s["sessionId"]
        print("session/new modes.currentModeId:", s.get("modes", {}).get("currentModeId"),
              "available:", [m["id"] for m in s.get("modes", {}).get("availableModes", [])])
        r = a.request("session/prompt", {"sessionId": sid, "prompt": [{"type": "text", "text": prompt}]})
        print("prompt result:", json.dumps(r))
        target = cwd / "probe.txt"
        print("probe.txt exists:", target.exists(), "content:", target.read_text() if target.exists() else None)
        print("requests_in from agent:", json.dumps(a.requests_in))
        print("permission requests:", json.dumps(permission_summary(a)))
        print("tool updates:", json.dumps(tool_summary(a)))
        print("agent text:", json.dumps(agent_text(a)))
        print(json.dumps(summarize_notes(a)))
        print("auth_status_update:", json.dumps(redact(auth_status(a))))
    finally:
        print("exit_code_after_stdin_close:", a.finish())
        if stub:
            stub.close()


def scenario_cancel(args):
    stub = None
    env = None
    if args.stub_long:
        cwd, stub, env = stub_setup("cancel-long")
        a = start("cancel-long", perm="allow_once", env=env)
        prompt = "STUB:LONG"
    else:
        if args.stub:
            cwd, stub, env = stub_setup("cancel-stub")
            prompt = "STUB:WRITE:c.txt"
        else:
            cwd = new_workdir("cancel")
            prompt = "Use the Write tool to create c.txt with the content x."
        a = start("cancel", perm="hold", env=env)
    try:
        sid = open_session(a, cwd)["sessionId"]
        result = {}

        def run():
            try:
                result["r"] = a.request("session/prompt", {"sessionId": sid, "prompt": [{"type": "text", "text": prompt}]}, timeout=180)
            except Exception as e:
                result["err"] = str(e)

        t = threading.Thread(target=run)
        t.start()
        if args.stub_long:
            chunk = lambda n: (n["method"] == "session/update"  # noqa: E731
                               and n["params"]["update"].get("sessionUpdate") == "agent_message_chunk")
            if not a.wait_for(chunk, timeout=60):
                print("no streamed chunk arrived; cancel not exercised")
            time.sleep(0.5)
            print("chunks before cancel:", sum(1 for n in a.notes if chunk(n)))
        else:
            if not a.wait_for(lambda n: False, timeout=90):
                print("no permission request arrived; cancel not exercised")
        t_cancel = time.time()
        a.notify("session/cancel", {"sessionId": sid})
        if not args.stub_long and not args.no_answer:
            a.answer_held({"outcome": "cancelled"})
        t.join(60)
        print("cancel sent; prompt returned after", round(time.time() - t_cancel, 3), "s")
        print("prompt result:", json.dumps(result.get("r", result.get("err"))))
        print("cancel_variant:", "stub-long" if args.stub_long else ("no-answer" if args.no_answer else "answer-cancelled"))
        print("permission requests:", json.dumps(permission_summary(a)))
        print(json.dumps(summarize_notes(a)))
        print("exists c.txt:", (cwd / "c.txt").exists())
    finally:
        print("exit_code_after_stdin_close:", a.finish())
        if stub:
            time.sleep(1)
            stub.close()


def scenario_resume_a(args):
    cwd = new_workdir("resume")
    a = start("resume-a")
    try:
        sid = open_session(a, cwd)["sessionId"]
        r = a.request("session/prompt", {"sessionId": sid, "prompt": [{"type": "text", "text":
                      "Remember the word banana. Reply only OK."}]})
        print("prompt result:", json.dumps(r))
        (OUT / "resume-state.json").write_text(json.dumps({"sessionId": sid, "cwd": str(cwd)}))
    finally:
        print("exit_code_after_stdin_close:", a.finish())


def scenario_resume_b(args):
    state = json.loads((OUT / "resume-state.json").read_text())
    a = start("resume-b")
    try:
        r = a.request("initialize", init_params())
        caps = r.get("agentCapabilities", {})
        print("loadSession:", caps.get("loadSession"),
              "sessionCapabilities.resume:", json.dumps(caps.get("sessionCapabilities", {}).get("resume")))
        before = len(a.notes)
        res = a.request(args.method, {"sessionId": state["sessionId"], "cwd": state["cwd"], "mcpServers": [],
                                      "_meta": DEFAULT_SETTINGS_META})
        print(f"{args.method} result keys:", sorted(res.keys()) if isinstance(res, dict) else res)
        print(f"notifications during {args.method}:", len(a.notes) - before)
        r2 = a.request("session/prompt", {"sessionId": state["sessionId"], "prompt": [{"type": "text", "text":
                       "What word did I ask you to remember? Reply with only the word."}]})
        print("prompt result after restart:", json.dumps(r2))
        print("agent text contains banana:", "banana" in agent_text(a).lower())
        print(json.dumps(summarize_notes(a)))
    except (RpcError, TimeoutError) as e:
        print("error:", e)
    finally:
        print("exit_code_after_stdin_close:", a.finish())


def scenario_resume_stub(args):
    """Stub-mode resume: adapter A completes a turn, exits; adapter B reloads and recalls."""
    wd, stub, env = stub_setup("resume-stub")
    try:
        a = start("resume-stub-a", env=env)
        try:
            sid = open_session(a, wd)["sessionId"]
            r = a.request("session/prompt", {"sessionId": sid, "prompt": [{"type": "text", "text":
                          "STUB:REMEMBER:banana Remember the word. Reply OK."}]})
            print("A prompt result:", json.dumps(r))
        finally:
            print("A exit_code_after_stdin_close:", a.finish())
        b = start("resume-stub-b", env=env)
        try:
            caps = b.request("initialize", init_params())["agentCapabilities"]
            print("B loadSession:", caps.get("loadSession"),
                  "sessionCapabilities.resume:", json.dumps(caps.get("sessionCapabilities", {}).get("resume")))
            before = len(b.notes)
            res = b.request(args.method, {"sessionId": sid, "cwd": str(wd), "mcpServers": [],
                                          "_meta": DEFAULT_SETTINGS_META})
            print(f"{args.method} result keys:", sorted(res.keys()) if isinstance(res, dict) else res)
            replay = [n["params"]["update"].get("sessionUpdate") for n in b.notes[before:]
                      if n["method"] == "session/update"]
            print(f"update kinds during {args.method}:", json.dumps(sorted(set(replay))))
            r2 = b.request("session/prompt", {"sessionId": sid, "prompt": [{"type": "text", "text":
                           "STUB:RECALL What word did I ask you to remember? Reply with only the word."}]})
            print("B prompt result:", json.dumps(r2))
            print("B agent text:", json.dumps(agent_text(b)))
            print("agent text contains banana:", "banana" in agent_text(b).lower())
        except (RpcError, TimeoutError) as e:
            print("B error:", e)
        finally:
            print("B exit_code_after_stdin_close:", b.finish())
    finally:
        time.sleep(1)
        stub.close()


def scenario_fs(args):
    """Stub-mode: Read tool then Bash tool; reveals whether the adapter asks the client for fs/terminal."""
    wd, stub, env = stub_setup("fs-stub")
    (wd / "probe.txt").write_text("hello from file", encoding="utf-8")
    a = start("fs", perm="allow_once", fs_mode=args.fs_mode, env=env)
    try:
        sid = open_session(a, wd)["sessionId"]
        r1 = a.request("session/prompt", {"sessionId": sid, "prompt": [{"type": "text", "text": "STUB:READ:probe.txt"}]})
        print("Read prompt result:", json.dumps(r1))
        r2 = a.request("session/prompt", {"sessionId": sid, "prompt": [{"type": "text", "text": "STUB:BASH:echo hi"}]})
        print("Bash prompt result:", json.dumps(r2))
        print("requests_in from agent:", json.dumps(a.requests_in))
        print("permission requests:", json.dumps(permission_summary(a)))
        print("tool updates:", json.dumps(tool_summary(a)))
        print("agent text:", json.dumps(agent_text(a)))
        print(json.dumps(summarize_notes(a)))
    finally:
        print("exit_code_after_stdin_close:", a.finish())
        time.sleep(1)
        stub.close()


def tree_rows():
    ps = "Get-CimInstance Win32_Process | Select-Object ProcessId,ParentProcessId,Name | ConvertTo-Json -Compress"
    raw = subprocess.run(["powershell.exe", "-NoProfile", "-Command", ps], capture_output=True, text=True).stdout
    rows = json.loads(raw) if raw.strip() else []
    return [rows] if isinstance(rows, dict) else rows


def descendants(root_pid, rows):
    kids, names = {}, {}
    for r in rows:
        kids.setdefault(r["ParentProcessId"], []).append(r["ProcessId"])
        names[r["ProcessId"]] = r["Name"]
    out, stack = [], [root_pid]
    while stack:
        for c in kids.get(stack.pop(), []):
            out.append({"pid": c, "name": names.get(c)})
            stack.append(c)
    return out


def pid_alive(pid):
    ps = f"if (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ 'alive' }} else {{ 'gone' }}"
    return subprocess.run(["powershell.exe", "-NoProfile", "-Command", ps],
                          capture_output=True, text=True).stdout.strip() == "alive"


def scenario_tree(args):
    cwd = new_workdir("tree")
    a = start("tree", new_group=True)
    try:
        sid = open_session(a, cwd)["sessionId"]
        if args.prompt:
            a.request("session/prompt", {"sessionId": sid, "prompt": [{"type": "text", "text": "Reply OK."}]})
        before = descendants(a.p.pid, tree_rows())
        print("children before action:", json.dumps(before))
        t0 = time.time()
        if args.how == "stdin":
            a.close_stdin()
        elif args.how == "terminate":
            a.p.terminate()
        elif args.how == "kill":
            a.p.kill()
        elif args.how == "taskkill":
            subprocess.run(["taskkill", "/PID", str(a.p.pid), "/T", "/F"], capture_output=True, text=True)
        elif args.how == "ctrlbreak":
            os.kill(a.p.pid, signal.CTRL_BREAK_EVENT)
        try:
            code = a.p.wait(timeout=30)
        except subprocess.TimeoutExpired:
            code = None
        print(f"action={args.how} node_exit_code={code} exit_after_s={round(time.time() - t0, 2)}")
        time.sleep(3)
        print("surviving children 3s after node exit:",
              json.dumps([c for c in before if pid_alive(c["pid"])]))
    finally:
        a.log.close()
        a.errlog.close()
        if a.p.poll() is None:
            a.p.kill()


def scenario_claude_p(args):
    stub = None
    env = child_env()
    if args.stub:
        cwd, stub, env = stub_setup("claude-p-stub")
        out_name = "claude-p-stub.jsonl"
    else:
        cwd = new_workdir("claude-p")
        out_name = "claude-p.jsonl"
    claude = shutil.which("claude") or r"C:\Users\0x1de\.local\bin\claude.exe"
    cmd = [claude, "-p", "STUB:PLAIN Reply with the single word OK", "--output-format", "stream-json",
           "--verbose", "--no-session-persistence"]
    try:
        r = subprocess.run(cmd, cwd=str(cwd), capture_output=True, text=True, timeout=180, env=env)
    finally:
        if stub:
            stub.close()
    (OUT / out_name).write_text(r.stdout, encoding="utf-8")
    print("exit:", r.returncode, "stderr_bytes:", len(r.stderr))
    for line in r.stdout.splitlines():
        try:
            ev = json.loads(line)
        except ValueError:
            continue
        if ev.get("type") == "result":
            print("result keys:", sorted(ev.keys()))
            print("is_error:", ev.get("is_error"), "subtype:", ev.get("subtype"), "stop_reason:", ev.get("stop_reason"))
            print("result text:", json.dumps(ev.get("result")))
            print("usage:", json.dumps(ev.get("usage")))
            print("modelUsage keys:", sorted((ev.get("modelUsage") or {}).keys()))
            print("total_cost_usd:", ev.get("total_cost_usd"))


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="scenario", required=True)
    p = sub.add_parser("init")
    p.add_argument("--auth-terminal", action="store_true")
    p.add_argument("--advertise-fs", action="store_true")
    p.add_argument("--stub", action="store_true")
    p.set_defaults(fn=scenario_init)
    p = sub.add_parser("write")
    p.add_argument("--perm", default="allow_once", choices=["allow_once", "reject_once"])
    p.add_argument("--fs-mode", default="decline", choices=["decline", "advertise"])
    p.add_argument("--default-mode", default="default",
                   choices=["default", "acceptEdits", "plan", "auto", "bypassPermissions"])
    p.add_argument("--disk-mode", default=None,
                   choices=["default", "acceptEdits", "plan", "auto", "bypassPermissions"])
    p.add_argument("--stub", action="store_true")
    p.set_defaults(fn=scenario_write)
    p = sub.add_parser("cancel")
    p.add_argument("--no-answer", action="store_true")
    p.add_argument("--stub", action="store_true")
    p.add_argument("--stub-long", action="store_true")
    p.set_defaults(fn=scenario_cancel)
    sub.add_parser("resume-a").set_defaults(fn=scenario_resume_a)
    p = sub.add_parser("resume-b")
    p.add_argument("--method", default="session/load", choices=["session/load", "session/resume"])
    p.set_defaults(fn=scenario_resume_b)
    p = sub.add_parser("resume-stub")
    p.add_argument("--method", default="session/load", choices=["session/load", "session/resume"])
    p.set_defaults(fn=scenario_resume_stub)
    p = sub.add_parser("fs")
    p.add_argument("--fs-mode", default="decline", choices=["decline", "advertise"])
    p.set_defaults(fn=scenario_fs)
    p = sub.add_parser("tree")
    p.add_argument("--how", required=True, choices=["stdin", "terminate", "kill", "taskkill", "ctrlbreak"])
    p.add_argument("--prompt", action="store_true")
    p.set_defaults(fn=scenario_tree)
    p = sub.add_parser("claude-p")
    p.add_argument("--stub", action="store_true")
    p.set_defaults(fn=scenario_claude_p)
    args = ap.parse_args()
    args.fn(args)


if __name__ == "__main__":
    sys.exit(main())
