#!/usr/bin/env python3
"""Stub OpenAI-compatible chat endpoint for the hermes-a2a spike.

Localhost only, stdlib only, no real provider and no credentials. Hermes is pointed here via
model.provider=custom, model.base_url=http://127.0.0.1:<port>/v1 in the throwaway HERMES_HOME.

Behaviour (keyed on the latest user message text, so prompts are reproducible):
  STUB_INPUT_REQUIRED      -> "[INPUT_REQUIRED] Which city?"
  STUB_TOOL_DISCOVER <url> -> tool call a2a_discover {"url": <url>}
  STUB_TOOL_CALL <name> <url> <text> -> tool call a2a_call (args per tool schema, see README)
  last message role=tool   -> "STUB_AFTER_TOOL: <tool result, first 400 chars>"
  anything else            -> "STUB_REPLY: ok"

Every request is appended as one JSON line to the log file (argv[2]) so the spike can inspect
the exact message history Hermes built for each A2A turn.

Usage: python stub_openai.py <port> <log-path>
"""
import json
import sys
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LOG_PATH = None


def _text(content):
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "".join(p.get("text", "") for p in content if isinstance(p, dict))
    return ""


def _latest_user_text(messages):
    for m in reversed(messages):
        if m.get("role") == "user":
            return _text(m.get("content"))
    return ""


def decide(messages, tools):
    """Return (assistant_message_dict, finish_reason)."""
    last = messages[-1] if messages else {}
    if last.get("role") == "tool":
        return {"role": "assistant", "content": "STUB_AFTER_TOOL: " + _text(last.get("content"))[:400]}, "stop"
    user = _latest_user_text(messages)
    tool_names = {t.get("function", {}).get("name") for t in (tools or [])}
    if "STUB_TOOL_DISCOVER " in user and "a2a_discover" in tool_names:
        url = user.split("STUB_TOOL_DISCOVER ", 1)[1].split()[0]
        call = {"id": "call_" + uuid.uuid4().hex[:8], "type": "function",
                "function": {"name": "a2a_discover", "arguments": json.dumps({"url": url})}}
        return {"role": "assistant", "content": None, "tool_calls": [call]}, "tool_calls"
    if "STUB_TOOL_CALL " in user and "a2a_call" in tool_names:
        # STUB_TOOL_CALL <agent-or-url> <text...>
        rest = user.split("STUB_TOOL_CALL ", 1)[1].strip()
        target, _, text = rest.partition(" ")
        call = {"id": "call_" + uuid.uuid4().hex[:8], "type": "function",
                "function": {"name": "a2a_call", "arguments": json.dumps({"agent": target, "message": text.strip()})}}
        return {"role": "assistant", "content": None, "tool_calls": [call]}, "tool_calls"
    if "STUB_INPUT_REQUIRED" in user:
        return {"role": "assistant", "content": "[INPUT_REQUIRED] Which city?"}, "stop"
    return {"role": "assistant", "content": "STUB_REPLY: ok"}, "stop"


def _log(entry):
    if LOG_PATH:
        with open(LOG_PATH, "a", encoding="utf-8") as fh:
            fh.write(json.dumps(entry, ensure_ascii=False) + "\n")


class Handler(BaseHTTPRequestHandler):
    server_version = "stub-openai/0.1"

    def log_message(self, fmt, *args):  # keep stderr quiet
        pass

    def _send_json(self, code, payload):
        body = json.dumps(payload).encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path.rstrip("/") in ("/v1/models", "/models"):
            return self._send_json(200, {"object": "list", "data": [{"id": "stub-model", "object": "model", "owned_by": "stub"}]})
        self._send_json(404, {"error": "not found"})

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0) or 0)
        try:
            req = json.loads(self.rfile.read(length).decode("utf-8") or "{}")
        except (ValueError, UnicodeDecodeError):
            return self._send_json(400, {"error": "bad json"})
        if not self.path.rstrip("/").endswith("/chat/completions"):
            return self._send_json(404, {"error": "unsupported path " + self.path})
        messages = req.get("messages", [])
        tools = req.get("tools", [])
        msg, finish = decide(messages, tools)
        _log({
            "ts": time.time(), "path": self.path, "stream": bool(req.get("stream")),
            "model": req.get("model"), "n_messages": len(messages),
            "roles": [m.get("role") for m in messages],
            "last_user_head": _latest_user_text(messages)[:200],
            "tool_names": sorted(t.get("function", {}).get("name", "") for t in tools),
            "reply_content": msg.get("content"), "reply_tool_calls": msg.get("tool_calls"),
        })
        cid, created, model = "chatcmpl-" + uuid.uuid4().hex[:12], int(time.time()), req.get("model") or "stub-model"
        if not req.get("stream"):
            payload = {"id": cid, "object": "chat.completion", "created": created, "model": model,
                       "choices": [{"index": 0, "message": msg, "finish_reason": finish}],
                       "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}}
            return self._send_json(200, payload)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()

        def chunk(delta, fin=None):
            obj = {"id": cid, "object": "chat.completion.chunk", "created": created, "model": model,
                   "choices": [{"index": 0, "delta": delta, "finish_reason": fin}]}
            self.wfile.write(("data: " + json.dumps(obj) + "\n\n").encode("utf-8"))
            self.wfile.flush()

        chunk({"role": "assistant"})
        if msg.get("tool_calls"):
            for i, tc in enumerate(msg["tool_calls"]):
                chunk({"tool_calls": [{"index": i, "id": tc["id"], "type": "function",
                                       "function": {"name": tc["function"]["name"], "arguments": tc["function"]["arguments"]}}]})
        elif msg.get("content"):
            chunk({"content": msg["content"]})
        chunk({}, finish)
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()


def main():
    global LOG_PATH
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 9912
    LOG_PATH = sys.argv[2] if len(sys.argv) > 2 else None
    srv = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    print(f"stub-openai listening on 127.0.0.1:{port}", flush=True)
    srv.serve_forever()


if __name__ == "__main__":
    main()
