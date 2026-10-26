"""Minimal stdio MCP server for the omp-acp spike (stdlib only).

Usage: python mcp_probe_server.py <log-file>
Logs every received method to the log file so the probe can see when omp connects.
Exposes one tool, probe_echo(text).
"""
import datetime
import json
import os
import sys

LOG = sys.argv[1] if len(sys.argv) > 1 else "mcp-probe.log"


def log(text):
    with open(LOG, "a", encoding="utf-8") as f:
        f.write(f"{datetime.datetime.now().isoformat()} pid={os.getpid()} {text}\n")


def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


log("start")
for raw in sys.stdin:
    line = raw.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    log(f"recv {method}")
    if "id" not in msg:
        continue
    if method == "initialize":
        result = {
            "protocolVersion": msg["params"].get("protocolVersion", "2024-11-05"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "agent-proto-probe", "version": "0.0.0"},
        }
    elif method == "tools/list":
        result = {"tools": [{
            "name": "probe_echo",
            "description": "Echo the given text back.",
            "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}},
                            "required": ["text"]},
        }]}
    elif method == "tools/call":
        text = msg["params"].get("arguments", {}).get("text", "")
        result = {"content": [{"type": "text", "text": "echo: " + text}], "isError": False}
    else:
        send({"jsonrpc": "2.0", "id": msg["id"],
              "error": {"code": -32601, "message": "Method not found"}})
        continue
    send({"jsonrpc": "2.0", "id": msg["id"], "result": result})
log("eof, exiting")
