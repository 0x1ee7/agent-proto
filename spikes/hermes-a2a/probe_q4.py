#!/usr/bin/env python3
"""Q4 follow-up probe: continue an INPUT_REQUIRED task (reuses helpers from probe.py).

Steps: (a) inbound SendMessage that the stub answers with [INPUT_REQUIRED]; (b) follow-up on the
same contextId with message.taskId set to that task; (c) GetTask on the task; (d) follow-up on the
same contextId without taskId. Writes .spikes/hermes/logs/probe-q4.json (tokens scrubbed).

Usage: python spikes/hermes-a2a/probe_q4.py   (gateway on 127.0.0.1:9911 and stub on 9912 must run)
"""
import json
import uuid

import probe


def main():
    ctx = "ctx-q4-" + uuid.uuid4().hex[:12]
    st, _, txt = probe.rpc("SendMessage", {"message": probe.umsg("STUB_INPUT_REQUIRED which city", ctx=ctx)})
    first = probe.task_of(txt)
    probe.record("Q4a", "INPUT_REQUIRED first turn", st, probe.task_brief(first), txt[:800])
    tid = first.get("id", "")

    msg = probe.umsg("Paris", ctx=ctx)
    msg["taskId"] = tid
    st, _, txt = probe.rpc("SendMessage", {"message": msg})
    probe.record("Q4b", "follow-up with message.taskId", st,
                 probe.task_brief(probe.task_of(txt)) or probe.brief(txt), txt[:800])

    st, _, txt = probe.rpc("GetTask", {"id": tid})
    probe.record("Q4c", "GetTask after follow-up", st, probe.task_brief(probe.task_of(txt)) or probe.brief(txt), txt[:600])

    st, _, txt = probe.rpc("SendMessage", {"message": probe.umsg("Paris again", ctx=ctx)})
    probe.record("Q4d", "follow-up same contextId, no taskId", st,
                 probe.task_brief(probe.task_of(txt)) or probe.brief(txt), txt[:400])

    out = probe.LOGS / "probe-q4.json"
    out.write_text(probe.scrub(json.dumps({"results": probe.RESULTS, "task_id": tid, "ctx": ctx},
                                          indent=2, ensure_ascii=False)), encoding="utf-8")
    print("wrote", out.relative_to(probe.ROOT), flush=True)


if __name__ == "__main__":
    main()
