#!/usr/bin/env python3
"""Local stub of the Anthropic Messages API for the claude-acp spike (stdlib only).

It is NOT a model. It answers POST /v1/messages (streaming SSE or JSON), POST
/v1/messages/count_tokens and GET /v1/models. The reply is chosen by a STUB:
directive in the latest user prompt (see README.md). Usage numbers are fixed so
they can be checked against adapter output. Runs as a daemon thread inside the
client process; no separate process is started.
STUB:A2A:<agent>:<text> emits one a2a_send tool call (the a2abus MCP tool). Claude Code defers MCP tools: the
model is offered only ToolSearch and the full mcp__<server>__<tool> names are announced in a system reminder. The
stub therefore calls ToolSearch select:<full name> first, then the full name; if the tool is already in `tools` it
is called directly (matched by suffix). The stub log records `deferred` (announced mcp__ names) next to
`tools_offered`, because the `tools` array alone does not show which MCP tools a session has.
"""
import json
import pathlib
import re
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

FAKE_KEY = "sk-stub-not-real"
USAGE_IN = 123
USAGE_OUT = 17
LONG_CHUNKS = 200
LONG_DELAY_S = 0.05


def _text_of(content):
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "\n".join(b.get("text", "") for b in content if isinstance(b, dict) and b.get("type") == "text")
    return ""


def _tool_results(content):
    if isinstance(content, list):
        return [b for b in content if isinstance(b, dict) and b.get("type") == "tool_result"]
    return []


def _all_text(body):
    """Text of the system prompt and of every message."""
    parts = []
    system = body.get("system")
    if isinstance(system, str):
        parts.append(system)
    elif isinstance(system, list):
        parts.append(_text_of(system))
    for m in body.get("messages", []):
        parts.append(_text_of(m.get("content")))
    return "\n".join(parts)


def _deferred_mcp(body):
    """Full mcp__<server>__<tool> names announced as deferred (one per line in a system reminder)."""
    found = []
    for line in _all_text(body).splitlines():
        line = line.strip()
        if re.fullmatch(r"mcp__[A-Za-z0-9_-]+__[A-Za-z0-9_-]+", line) and line not in found:
            found.append(line)
    return found


def _answers_tool_search(msgs):
    """True when the latest user message carries only results of ToolSearch calls."""
    if len(msgs) < 2:
        return False
    results = _tool_results(msgs[-1].get("content"))
    prev = msgs[-2].get("content")
    if not results or not isinstance(prev, list):
        return False
    uses = {b.get("id"): b.get("name") for b in prev if isinstance(b, dict) and b.get("type") == "tool_use"}
    return all(uses.get(r.get("tool_use_id")) == "ToolSearch" for r in results)


def _usage(output):
    return {"input_tokens": USAGE_IN, "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0, "output_tokens": output}


class Stub:
    def __init__(self, workdir, log_path):
        self.workdir = pathlib.Path(workdir)
        self.log = open(log_path, "w", encoding="utf-8")
        self.lock = threading.Lock()
        self.count = 0
        self.httpd = ThreadingHTTPServer(("127.0.0.1", 0), _Handler)
        self.httpd.stub = self
        self.url = f"http://127.0.0.1:{self.httpd.server_address[1]}"
        self.thread = threading.Thread(target=self.httpd.serve_forever, daemon=True)

    def start(self):
        self.thread.start()
        return self

    def close(self):
        self.httpd.shutdown()
        self.httpd.server_close()
        self.log.close()

    def note(self, **kw):
        with self.lock:
            self.log.write(json.dumps(kw, ensure_ascii=False) + "\n")
            self.log.flush()

    def plan(self, body):
        """Return (kind, payload). kind: text | long | tool."""
        msgs = body.get("messages", [])
        last = msgs[-1] if msgs else {}
        if last.get("role") == "user" and _tool_results(last.get("content")):
            if not _answers_tool_search(msgs):
                return "text", "DONE"
        if not body.get("tools"):
            return "text", "OK"
        prompt = ""
        for m in reversed(msgs):
            if m.get("role") == "user" and "STUB:" in _text_of(m.get("content")):
                prompt = _text_of(m.get("content"))
                break
        m = re.search(r"STUB:WRITE:(\S+)", prompt)
        if m:
            return "tool", ("Write", {"file_path": str(self.workdir / m.group(1)), "content": "hello"})
        m = re.search(r"STUB:READ:(\S+)", prompt)
        if m:
            return "tool", ("Read", {"file_path": str(self.workdir / m.group(1))})
        m = re.search(r"STUB:BASH:([^\n]+)", prompt)
        if m:
            return "tool", ("Bash", {"command": m.group(1).strip()})
        m = re.search(r"STUB:A2A:([a-z0-9][a-z0-9-]{0,47}):([^\n]+)", prompt)
        if m:
            return "tool", self.peer_send(m.group(1), m.group(2).strip(), body, _answers_tool_search(msgs))
        if "STUB:LONG" in prompt:
            return "long", None
        if "STUB:RECALL" in prompt:
            word = "UNKNOWN"
            for mm in msgs:
                if mm.get("role") == "user":
                    found = re.search(r"STUB:REMEMBER:(\w+)", _text_of(mm.get("content")))
                    if found:
                        word = found.group(1)
                        break
            return "text", word
        return "text", "OK"

    def peer_send(self, agent, text, body, searched):
        """The a2a_send call as a model that obeys Claude Code's deferred-tool rule would make it."""
        args = {"agent": agent, "text": text}
        names = [t.get("name", "") for t in body.get("tools", []) if isinstance(t, dict)]
        if any(n.lower().endswith("a2a_send") for n in names):
            return "a2a_send", args
        deferred = [n for n in _deferred_mcp(body) if n.endswith("__a2a_send")]
        if not deferred:
            return "a2a_send", args
        if not searched and "ToolSearch" in names:
            return "ToolSearch", {"query": f"select:{deferred[0]}", "max_results": 5}
        return deferred[0], args

    def resolve_tool(self, name, body):
        names = [t.get("name", "") for t in body.get("tools", []) if isinstance(t, dict)]
        if name in names:
            return name
        for n in names:
            if n.lower().endswith(name.lower()):
                return n
        return name


class _Handler(BaseHTTPRequestHandler):
    server_version = "stub-anthropic/0"

    def log_message(self, *a):
        pass

    def _json(self, code, obj):
        data = json.dumps(obj).encode("utf-8")
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path.startswith("/v1/models"):
            self._json(200, {"data": [], "has_more": False, "first_id": None, "last_id": None})
        else:
            self._json(404, {"type": "error", "error": {"type": "not_found_error", "message": "stub: no route"}})

    def do_POST(self):
        stub = self.server.stub
        raw = self.rfile.read(int(self.headers.get("content-length", "0") or 0))
        try:
            body = json.loads(raw or b"{}")
        except ValueError:
            self._json(400, {"type": "error", "error": {"type": "invalid_request_error", "message": "bad json"}})
            return
        if self.path.startswith("/v1/messages/count_tokens"):
            self._json(200, {"input_tokens": USAGE_IN})
            return
        if not self.path.startswith("/v1/messages"):
            self._json(404, {"type": "error", "error": {"type": "not_found_error", "message": "stub: no route"}})
            return
        kind, payload = stub.plan(body)
        if kind == "tool":
            payload = (stub.resolve_tool(payload[0], body), payload[1])
        with stub.lock:
            stub.count += 1
            n = stub.count
        stub.note(n=n, path=self.path, stream=bool(body.get("stream")), model=body.get("model"),
                  kind=kind, tool=payload[0] if kind == "tool" else None,
                  tools_offered=[t.get("name") for t in body.get("tools", []) if isinstance(t, dict)],
                  deferred=_deferred_mcp(body))
        try:
            self._respond(stub, n, body, kind, payload)
        except (BrokenPipeError, ConnectionResetError):
            stub.note(n=n, client_disconnected=True)

    def _respond(self, stub, n, body, kind, payload):
        model = body.get("model") or "stub"
        msg_id = f"msg_stub_{n}"
        stop = "tool_use" if kind == "tool" else "end_turn"
        if not body.get("stream"):
            if kind == "tool":
                content = [{"type": "tool_use", "id": f"toolu_stub_{n}", "name": payload[0], "input": payload[1]}]
            else:
                content = [{"type": "text", "text": payload if kind == "text" else "x" * 10}]
            self._json(200, {"id": msg_id, "type": "message", "role": "assistant", "model": model,
                             "content": content, "stop_reason": stop, "stop_sequence": None,
                             "usage": _usage(USAGE_OUT)})
            return
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        self.end_headers()

        def sse(event, data):
            self.wfile.write(f"event: {event}\ndata: {json.dumps(data)}\n\n".encode("utf-8"))
            self.wfile.flush()

        start_usage = _usage(1)
        sse("message_start", {"type": "message_start", "message": {
            "id": msg_id, "type": "message", "role": "assistant", "model": model, "content": [],
            "stop_reason": None, "stop_sequence": None, "usage": start_usage}})
        if kind == "tool":
            name, tool_input = payload
            sse("content_block_start", {"type": "content_block_start", "index": 0,
                                        "content_block": {"type": "tool_use", "id": f"toolu_stub_{n}",
                                                          "name": name, "input": {}}})
            sse("content_block_delta", {"type": "content_block_delta", "index": 0,
                                        "delta": {"type": "input_json_delta", "partial_json": json.dumps(tool_input)}})
            sse("content_block_stop", {"type": "content_block_stop", "index": 0})
        elif kind == "long":
            sse("content_block_start", {"type": "content_block_start", "index": 0,
                                        "content_block": {"type": "text", "text": ""}})
            for i in range(LONG_CHUNKS):
                sse("content_block_delta", {"type": "content_block_delta", "index": 0,
                                            "delta": {"type": "text_delta", "text": f"line {i}\n"}})
                time.sleep(LONG_DELAY_S)
            sse("content_block_stop", {"type": "content_block_stop", "index": 0})
        else:
            sse("content_block_start", {"type": "content_block_start", "index": 0,
                                        "content_block": {"type": "text", "text": ""}})
            sse("content_block_delta", {"type": "content_block_delta", "index": 0,
                                        "delta": {"type": "text_delta", "text": payload}})
            sse("content_block_stop", {"type": "content_block_stop", "index": 0})
        sse("message_delta", {"type": "message_delta", "delta": {"stop_reason": stop, "stop_sequence": None},
                              "usage": {"output_tokens": USAGE_OUT}})
        sse("message_stop", {"type": "message_stop"})


def start_stub(workdir, log_path):
    return Stub(workdir, log_path).start()
