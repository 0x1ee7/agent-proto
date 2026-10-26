#!/usr/bin/env python3
"""Raw-socket retest for an oversized (> 1 MiB) JSON-RPC body (T14 in probe.py).

http.client aborts while still sending the body, which hides the server's reply. This script
sends the headers, then streams the body and keeps whatever bytes the server returns.
Reads the peer token from .spikes/hermes/secrets/peer.token (never printed).
Writes .spikes/hermes/logs/probe-extra-413.txt (token scrubbed).

Usage: python spikes/hermes-a2a/probe_extra.py
"""
import pathlib
import socket

ROOT = pathlib.Path(__file__).resolve().parents[2]
LOGS = ROOT / ".spikes" / "hermes" / "logs"
TOKEN = (ROOT / ".spikes" / "hermes" / "secrets" / "peer.token").read_text(encoding="utf-8").strip().partition(":")[2]

body = b'{"jsonrpc":"2.0","id":1,"method":"SendMessage","params":{"x":"' + b"a" * 1048577 + b'"}}'
head = (
    "POST / HTTP/1.1\r\n"
    "Host: 127.0.0.1:9911\r\n"
    "Content-Type: application/json\r\n"
    f"Authorization: Bearer {TOKEN}\r\n"
    "A2A-Version: 1.0\r\n"
    f"Content-Length: {len(body)}\r\n"
    "Connection: close\r\n\r\n"
).encode("ascii")

sock = socket.create_connection(("127.0.0.1", 9911), timeout=30)
events = []
try:
    sock.sendall(head)
    sent = 0
    step = 65536
    while sent < len(body):
        try:
            sock.sendall(body[sent:sent + step])
            sent += step
        except OSError as exc:
            events.append(f"send stopped at {sent} bytes: {exc}")
            break
    else:
        events.append(f"sent all {len(body)} body bytes")
except OSError as exc:
    events.append(f"head send failed: {exc}")

received = b""
try:
    while True:
        chunk = sock.recv(65536)
        if not chunk:
            events.append("recv: EOF")
            break
        received += chunk
except OSError as exc:
    events.append(f"recv stopped: {exc}")
finally:
    sock.close()

text = received.decode("utf-8", "replace").replace(TOKEN, "<TOKEN>")
status_line = text.split("\r\n", 1)[0] if text else "(no response bytes)"
report = "\n".join(events + [f"status_line: {status_line}", "response_head_and_body:", text[:800]])
LOGS.mkdir(parents=True, exist_ok=True)
(LOGS / "probe-extra-413.txt").write_text(report, encoding="utf-8")
print(report)
