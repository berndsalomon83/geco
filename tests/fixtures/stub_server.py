#!/usr/bin/env python3
"""Minimal stdio MCP server for tests: tools = echo, add.

Set STUB_EXTRA_TOOLS=1 in the server's env to also expose `pid` and `crash`.
They're opt-in so the default tool list stays at two and the existing
snapshot/count assertions keep meaning what they say:
  pid   — returns this process's PID, so a test can check it was reaped
  crash — exits immediately WITHOUT replying, simulating a backend that dies
          mid-call (the path a graceful shutdown() can't reproduce)
"""
import json, os, sys

TOOLS = [
    {"name": "echo", "description": "Echo the input text back",
     "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}},
    {"name": "add", "description": "Add two numbers a and b",
     "inputSchema": {"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}}, "required": ["a", "b"]}},
    {"name": "secret_tool", "description": "Should be hidden by denylist in tests",
     "inputSchema": {"type": "object", "properties": {}}},
]

if os.environ.get("STUB_EXTRA_TOOLS"):
    TOOLS += [
        {"name": "pid", "description": "Return this server process's PID",
         "inputSchema": {"type": "object", "properties": {}}},
        {"name": "crash", "description": "Exit immediately without replying",
         "inputSchema": {"type": "object", "properties": {}}},
        {"name": "getenv", "description": "Report an env var as seen by this child process",
         "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}},
                         "required": ["name"]}},
        {"name": "grow_toolset", "description": "Add a tool, then send notifications/tools/list_changed",
         "inputSchema": {"type": "object", "properties": {}}},
    ]

def reply(id_, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": id_, "result": result}) + "\n")
    sys.stdout.flush()


def notify(method, params=None):
    msg = {"jsonrpc": "2.0", "method": method}
    if params is not None:
        msg["params"] = params
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method, id_ = msg.get("method"), msg.get("id")
    if method == "initialize":
        reply(id_, {"protocolVersion": msg["params"].get("protocolVersion", "2024-11-05"),
                    "capabilities": {"tools": {"listChanged": True}},
                    "serverInfo": {"name": "stub", "version": "0.0.1"}})
    elif method == "notifications/initialized":
        continue
    elif method == "tools/list":
        reply(id_, {"tools": TOOLS})
    elif method == "tools/call":
        name = msg["params"]["name"]
        args = msg["params"].get("arguments") or {}
        if name == "crash":
            # No reply, no clean shutdown — the transport just dies.
            os._exit(1)
        elif name == "pid":
            reply(id_, {"content": [{"type": "text", "text": str(os.getpid())}], "isError": False})
        elif name == "getenv":
            val = os.environ.get(args.get("name", ""), "<unset>")
            reply(id_, {"content": [{"type": "text", "text": val}], "isError": False})
        elif name == "grow_toolset":
            # A backend whose tool list changes at runtime — the case geco has
            # to notice without anyone asking it to.
            TOOLS.append({
                "name": "brand_new_tool",
                "description": "Appeared at runtime after a list_changed notification",
                "inputSchema": {"type": "object", "properties": {}},
            })
            reply(id_, {"content": [{"type": "text", "text": "grown"}], "isError": False})
            notify("notifications/tools/list_changed")
        elif name == "echo":
            reply(id_, {"content": [{"type": "text", "text": args.get("text", "")}], "isError": False})
        elif name == "add":
            reply(id_, {"content": [{"type": "text", "text": str(args.get("a", 0) + args.get("b", 0))}], "isError": False})
        else:
            reply(id_, {"content": [{"type": "text", "text": f"unknown tool {name}"}], "isError": True})
    elif id_ is not None:
        reply(id_, {})
