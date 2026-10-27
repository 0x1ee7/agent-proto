"""Behavior tests for stub_anthropic.py: STUB:A2A under Claude Code's deferred MCP tool loading.

Run: python -m unittest spikes/claude-acp/test_stub_anthropic.py  (or python spikes/claude-acp/test_stub_anthropic.py)

Request bodies mirror what the claude-agent-acp adapter sent in the D9 run (docs/spikes/e2e-m1.md): the a2abus
MCP tools are announced by full name in a reminder and only ToolSearch is offered; ToolSearch loads a schema into
`tools` for the next request.
"""
import pathlib
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import stub_anthropic as sa  # noqa: E402

PROMPT = "STUB:A2A:hermes:S3 research question"
SEND = "mcp__a2abus__a2a_send"
REMINDER = (
    "The following deferred tools are now available via ToolSearch. Their schemas are NOT loaded.\n"
    "CronCreate\nWebFetch\n"
    "mcp__a2abus__a2a_cancel\nmcp__a2abus__a2a_discover\nmcp__a2abus__a2a_get\n" + SEND + "\n"
)
BASE_TOOLS = ["Agent", "Bash", "Read", "ToolSearch", "Write"]


def text(value):
    return {"type": "text", "text": value}


def user_prompt():
    return {"role": "user", "content": [text("<system-reminder>\n" + REMINDER + "</system-reminder>"), text(PROMPT)]}


def body(messages, tool_names):
    return {"model": "m", "stream": True, "messages": messages,
            "tools": [{"name": n, "input_schema": {"type": "object"}} for n in tool_names]}


def call(tool_id, name):
    return {"role": "assistant", "content": [{"type": "tool_use", "id": tool_id, "name": name, "input": {}}]}


def result(tool_id):
    return {"role": "user", "content": [{"type": "tool_result", "tool_use_id": tool_id, "content": "ok"}]}


class StubA2aTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.stub = sa.Stub(self.tmp.name, pathlib.Path(self.tmp.name) / "stub.jsonl")

    def tearDown(self):
        self.stub.httpd.server_close()
        self.stub.log.close()
        self.tmp.cleanup()

    def plan_resolved(self, request):
        kind, payload = self.stub.plan(request)
        if kind == "tool":
            payload = (self.stub.resolve_tool(payload[0], request), payload[1])
        return kind, payload

    def test_deferred_tool_is_loaded_with_tool_search_first(self):
        kind, (name, args) = self.plan_resolved(body([user_prompt()], BASE_TOOLS))
        self.assertEqual(kind, "tool")
        self.assertEqual(name, "ToolSearch")
        self.assertEqual(args["query"], "select:" + SEND)

    def test_loaded_tool_is_called_by_full_name_after_tool_search(self):
        msgs = [user_prompt(), call("t1", "ToolSearch"), result("t1")]
        kind, (name, args) = self.plan_resolved(body(msgs, BASE_TOOLS + [SEND]))
        self.assertEqual((kind, name), ("tool", SEND))
        self.assertEqual(args, {"agent": "hermes", "text": "S3 research question"})

    def test_deferred_name_is_used_when_tool_search_result_does_not_load_a_schema(self):
        msgs = [user_prompt(), call("t1", "ToolSearch"), result("t1")]
        kind, (name, args) = self.plan_resolved(body(msgs, BASE_TOOLS))
        self.assertEqual((kind, name), ("tool", SEND))
        self.assertEqual(args["agent"], "hermes")

    def test_tool_already_in_tools_is_called_without_a_search(self):
        kind, (name, _) = self.plan_resolved(body([user_prompt()], BASE_TOOLS + [SEND]))
        self.assertEqual((kind, name), ("tool", SEND))

    def test_turn_ends_after_the_peer_call_result(self):
        msgs = [user_prompt(), call("t1", "ToolSearch"), result("t1"), call("t2", SEND), result("t2")]
        self.assertEqual(self.stub.plan(body(msgs, BASE_TOOLS + [SEND])), ("text", "DONE"))

    def test_log_names_the_deferred_tools(self):
        self.assertEqual(sa._deferred_mcp(body([user_prompt()], BASE_TOOLS)),
                         ["mcp__a2abus__a2a_cancel", "mcp__a2abus__a2a_discover", "mcp__a2abus__a2a_get", SEND])


if __name__ == "__main__":
    unittest.main()
