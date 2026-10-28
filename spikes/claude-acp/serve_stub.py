"""Serve stub_anthropic.py on a fixed loopback port until killed.

Usage: python serve_stub.py <port> <workdir> <log-path>

Used by the Docker demo stack (deploy/compose/a2ad.demo.toml), which runs it as an
external agent of the supervisor. No real model is called and no credential is read.
"""
import pathlib
import sys
import threading

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

import stub_anthropic  # noqa: E402


def main() -> int:
    if len(sys.argv) != 4:
        print(__doc__, file=sys.stderr)
        return 2
    port, workdir, log_path = int(sys.argv[1]), sys.argv[2], sys.argv[3]
    pathlib.Path(workdir).mkdir(parents=True, exist_ok=True)
    stub = stub_anthropic.Stub(workdir, log_path, port).start()
    print(f"claude stub listening on {stub.url}", flush=True)
    threading.Event().wait()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
