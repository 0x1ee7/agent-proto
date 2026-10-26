"""Scripted OpenAI-compatible chat-completions stub for the omp-acp spike (stdlib only).

Serves POST /v1/chat/completions with SSE streaming (and non-streaming JSON if stream is false).
Behaviour is chosen from the last user message (keywords) and whether the last message is a tool result.
Every request body is appended to <log-dir>/requests.jsonl so the probe can inspect what omp sent.

Usage: python stub_openai.py <port> <log-dir>
Scripted scenarios:
  "WRITE_FILE"   -> tool call write(path=written.txt, content=hello), then text DONE
  "READ_FILE"    -> tool call read(path=read-me.txt), then text = tool result verbatim
  "RUN_PING"     -> tool call bash(command=ping ... ), then text DONE
  "RUN_ECHO"     -> tool call bash(command=echo hi), then text DONE
  "LONG_STREAM"  -> 80 text chunks, 0.25 s apart (cancel testing)
  "CODE_WORD"    -> PINEAPPLE if an earlier message in this request mentions PINEAPPLE, else UNKNOWN
  anything else  -> text OK
"""
from __future__ import annotations

import json
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 18765
LOG_DIR = Path(sys.argv[2]) if len(sys.argv) > 2 else Path(".")
LOG_DIR.mkdir(parents=True, exist_ok=True)
REQ_LOG = LOG_DIR / "requests.jsonl"
USAGE = {"prompt_tokens": 123, "completion_tokens": 7, "total_tokens": 130}


def text_of(content) -> str:
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "".join(p.get("text", "") for p in content if isinstance(p, dict))
    return ""


def tool_names(body: dict) -> set:
    return {t["function"]["name"] for t in body.get("tools", []) if t.get("type") == "function"}


def tool_props(body: dict, name: str) -> set:
    for t in body.get("tools", []):
        f = t.get("function", {})
        if f.get("name") == name:
            return set(f.get("parameters", {}).get("properties", {}).keys())
    return set()


def decide(body: dict) -> dict:
    msgs = body.get("messages", [])
    last = msgs[-1] if msgs else {}
    if last.get("role") == "tool":
        result = text_of(last.get("content"))
        return {"text": result if result.strip() else "DONE"}
    last_user = ""
    for m in reversed(msgs):
        if m.get("role") == "user":
            last_user = text_of(m.get("content"))
            break
    up = last_user.upper()
    if "WRITE_FILE" in up:
        return {"tool": ("write", {"path": "written.txt", "content": "hello"})}
    if "READ_FILE" in up:
        return {"tool": ("read", {"path": "read-me.txt"})}
    if "RUN_PING" in up:
        return {"tool": ("bash", {"command": "ping -n 60 127.0.0.1 > nul"})}
    if "RUN_ECHO" in up:
        return {"tool": ("bash", {"command": "echo hi"})}
    if "LONG_STREAM" in up:
        return {"long": True}
    if "CODE_WORD" in up:
        seen = any("PINEAPPLE" in text_of(m.get("content")) for m in msgs[:-1])
        return {"text": "PINEAPPLE" if seen else "UNKNOWN"}
    return {"text": "OK"}


def chunk(delta: dict, finish=None, usage=None, model="stub-model") -> bytes:
    obj = {"id": "stub-1", "object": "chat.completion.chunk", "created": int(time.time()),
           "model": model,
           "choices": [{"index": 0, "delta": delta, "finish_reason": finish}] if delta is not None else []}
    if usage is not None:
        obj["usage"] = usage
    return b"data: " + json.dumps(obj).encode() + b"\n\n"


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):  # keep stdout clean
        pass

    def do_GET(self):
        if self.path.rstrip("/").endswith("/models"):
            data = json.dumps({"object": "list", "data": [{"id": "stub-model", "object": "model"}]}).encode()
            self._send(200, "application/json", data)
        else:
            self._send(404, "application/json", b'{"error":"not found"}')

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(length)
        body = json.loads(raw or b"{}")
        with open(REQ_LOG, "a", encoding="utf-8") as f:
            f.write(json.dumps({"t": time.time(), "path": self.path, "body": body}) + "\n")
        if not self.path.rstrip("/").endswith("/chat/completions"):
            self._send(404, "application/json", b'{"error":"not found"}')
            return
        plan = decide(body)
        model = body.get("model", "stub-model")
        stream = bool(body.get("stream"))
        if not stream:
            text = plan.get("text", "")
            msg = {"role": "assistant", "content": text}
            finish = "stop"
            if "tool" in plan:
                name, args = plan["tool"]
                msg = {"role": "assistant", "content": None, "tool_calls": [
                    {"id": "call_stub_1", "type": "function",
                     "function": {"name": name, "arguments": json.dumps(args)}}]}
                finish = "tool_calls"
            out = {"id": "stub-1", "object": "chat.completion", "created": int(time.time()), "model": model,
                   "choices": [{"index": 0, "message": msg, "finish_reason": finish}], "usage": USAGE}
            self._send(200, "application/json", json.dumps(out).encode())
            return
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()
        try:
            self.wfile.write(chunk({"role": "assistant", "content": ""}, model=model))
            if "tool" in plan:
                name, args = plan["tool"]
                self.wfile.write(chunk({"tool_calls": [{"index": 0, "id": "call_stub_1", "type": "function",
                                                        "function": {"name": name,
                                                                     "arguments": json.dumps(args)}}]},
                                       model=model))
                self.wfile.write(chunk({}, finish="tool_calls", model=model))
            elif plan.get("long"):
                for i in range(80):
                    self.wfile.write(chunk({"content": f"word{i} "}, model=model))
                    self.wfile.flush()
                    time.sleep(0.25)
                self.wfile.write(chunk({}, finish="stop", model=model))
            else:
                self.wfile.write(chunk({"content": plan.get("text", "")}, model=model))
                self.wfile.write(chunk({}, finish="stop", model=model))
            self.wfile.write(chunk(None, usage=USAGE, model=model))
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass  # client cancelled / went away

    def _send(self, code: int, ctype: str, data: bytes) -> None:
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


if __name__ == "__main__":
    srv = ThreadingHTTPServer(("127.0.0.1", PORT), Handler)
    print(f"stub listening on 127.0.0.1:{PORT}", flush=True)
    srv.serve_forever()
