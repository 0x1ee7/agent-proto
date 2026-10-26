"""Print one line per scenario result file: elicitations, permissions, tool outcomes, resume results.

Usage: python spikes/omp-acp/summarize.py [glob]   (default glob: elicit-*.json)
Reads .spikes/omp-acp/results/*.json written by probe.py.
"""
import glob
import json
import sys
from pathlib import Path

RESULTS = Path(__file__).resolve().parents[2] / ".spikes" / "omp-acp" / "results"


def tool_outcome(r: dict, key: str) -> str:
    ups = [u["update"] for u in r.get(key + "_updates", []) if u.get("kind") == "tool_call_update"]
    done = [u for u in ups if u.get("status") in ("completed", "failed")]
    if not done:
        res = r.get(key)
        if isinstance(res, dict) and "stopReason" in res.get("result", {}):
            return res["result"]["stopReason"]
        return "no-tool-update"
    last = done[-1]
    text = " | ".join(c.get("content", {}).get("text", "") for c in last.get("content", []))
    return f"{last['status']}: {text[:70]}"


def main() -> None:
    pattern = sys.argv[1] if len(sys.argv) > 1 else "elicit-*.json"
    for f in sorted(RESULTS.glob(pattern)):
        r = json.loads(f.read_text(encoding="utf-8"))
        print(f"== {f.name}")
        for e in r.get("elicitations", []):
            first = e["request"]["message"].splitlines()[0]
            print(f"   elicit: {first} -> {json.dumps(e.get('answer'), ensure_ascii=False)}")
        for p in r.get("permissions", []):
            tc = p["request"]["toolCall"]
            print(f"   perm: {tc['title']} ({tc['kind']}) -> {json.dumps(p.get('answer'), ensure_ascii=False)}")
        for k in ("write1", "read1", "bash1", "write2", "read2"):
            if k in r:
                print(f"   {k}: {tool_outcome(r, k)}")
        for k in ("session_load", "session_resume", "prompt_b"):
            if k in r:
                res = r[k]
                msg = res.get("error", {}).get("data", {}).get("details") if "error" in res else "ok"
                print(f"   {k}: {msg or 'ok'}")


if __name__ == "__main__":
    main()
