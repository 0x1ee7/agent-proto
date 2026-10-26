#!/usr/bin/env python3
"""Card-tolerance server for the hermes-a2a spike (Q7). Stdlib only, localhost only.

Serves an Agent Card that carries unknown top-level, interface, capability, skill and provider
fields. Answers SendMessage with a Task that carries unknown fields. Logs every request body it
receives (Authorization is omitted from the log) so the wire shape Hermes sends can be checked.

Usage: python spikes/hermes-a2a/card_server.py <port> <log-path>
Example: python spikes/hermes-a2a/card_server.py 9913 .spikes/hermes/logs/card-server.jsonl
"""
import json
import sys
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 9913
LOG = sys.argv[2] if len(sys.argv) > 2 else ".spikes/hermes/logs/card-server.jsonl"
BASE = f"http://127.0.0.1:{PORT}/"

CARD = {
    "name": "card-probe",
    "description": "Unknown-field tolerance probe",
    "url": BASE,
    "version": "1.0.0",
    "provider": {"organization": "spike", "url": BASE, "futureProviderField": 1},
    "supportedInterfaces": [
        {"url": BASE, "protocolBinding": "JSONRPC", "protocolVersion": "1.0", "futureInterfaceField": "x"}
    ],
    "capabilities": {"streaming": False, "pushNotifications": False, "futureCapability": True},
    "defaultInputModes": ["text/plain"],
    "defaultOutputModes": ["text/plain"],
    "skills": [
        {"id": "echo", "name": "echo", "description": "echo back", "tags": ["spike"], "futureSkillField": {"a": 1}}
    ],
    "futureTopLevelField": {"nested": [1, 2]},
}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def _send(self, code, obj):
        data = json.dumps(obj).encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path.split("?")[0] in ("/.well-known/agent-card.json", "/.well-known/agent.json"):
            self._send(200, CARD)
        else:
            self._send(404, {"error": "not found"})

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        try:
            req = json.loads(raw)
        except ValueError:
            req = {"unparsed": raw[:200].decode("utf-8", "replace")}
        headers = {k: v for k, v in self.headers.items() if k.lower() != "authorization"}
        with open(LOG, "a", encoding="utf-8") as fh:
            fh.write(json.dumps({"ts": time.time(), "path": self.path, "headers": headers, "body": req}) + "\n")
        method = req.get("method") if isinstance(req, dict) else None
        rid = req.get("id") if isinstance(req, dict) else None
        if method in ("SendMessage", "message/send"):
            task = {
                "id": "task-" + uuid.uuid4().hex[:16],
                "contextId": "ctx-" + uuid.uuid4().hex[:16],
                "futureTaskField": True,
                "status": {
                    "state": "TASK_STATE_COMPLETED",
                    "futureStatusField": 1,
                    "message": {"role": "ROLE_AGENT", "parts": [{"text": "CARD_PROBE_OK"}],
                                "messageId": uuid.uuid4().hex},
                },
                "artifacts": [{"artifactId": uuid.uuid4().hex, "parts": [{"text": "CARD_PROBE_OK"}]}],
            }
            self._send(200, {"jsonrpc": "2.0", "id": rid, "result": {"task": task}, "futureEnvelope": 1})
        else:
            self._send(200, {"jsonrpc": "2.0", "id": rid, "error": {"code": -32601, "message": "method not found"}})


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
