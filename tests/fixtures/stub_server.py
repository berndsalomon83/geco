#!/usr/bin/env python3
"""Minimal stdio MCP server for tests: tools = echo, add.

Set STUB_EXTRA_TOOLS=1 in the server's env to also expose `pid` and `crash`.
They're opt-in so the default tool list stays at two and the existing
snapshot/count assertions keep meaning what they say:
  pid   — returns this process's PID, so a test can check it was reaped
  crash — exits immediately WITHOUT replying, simulating a backend that dies
          mid-call (the path a graceful shutdown() can't reproduce)
"""
import json, os, sys, time

# Record that this process started, so a test can count how many children geco
# actually SPAWNED. Counting which child answered is not the same question:
# geco hands every concurrent caller the winning connection, so N racing
# spawns and one spawn look identical from the reply side. This file is the
# only place the difference is visible.
_spawn_log = os.environ.get("STUB_SPAWN_LOG")
if _spawn_log:
    with open(_spawn_log, "a") as _f:
        _f.write(f"{os.getpid()}\n")

# Start, record the start, then die before answering initialize — a backend
# that fails its handshake for a reason geco can only discover by spawning it.
# Counting the log lines is then the only way to tell "one attempt, seven
# callers reusing its error" from "eight callers each failing on their own".
if os.environ.get("STUB_DIE_AFTER_LOG"):
    sys.exit(1)

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
        # Serial-blocking on purpose: while inside this handler the loop is not
        # reading stdin, so any other request (a health-probe list_tools) waits
        # behind it — which is how a busy backend is simulated.
        {"name": "sleep", "description": "Block for `ms` milliseconds, then reply",
         "inputSchema": {"type": "object", "properties": {"ms": {"type": "number"}},
                         "required": ["ms"]}},
        # A backend that answers with something other than text — the shape
        # every screenshot tool uses, and the one geco used to flatten into a
        # JSON string.
        {"name": "screenshot", "description": "Reply with a text block and an image block",
         "inputSchema": {"type": "object", "properties": {}}},
        # Structured output ONLY, no content array. Mirroring
        # structuredContent into a text block is a SHOULD in the spec, so a
        # compliant backend may reply exactly like this; geco used to flatten
        # it into {ok: true, result: ""} and the payload vanished.
        {"name": "structured", "description": "Reply with structuredContent and no content blocks",
         "inputSchema": {"type": "object", "properties": {}}},
    ]

# A 1x1 transparent PNG. The pixels are irrelevant; what matters is that the
# reply carries a block whose type is not "text".
PNG_1PX = ("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk"
           "+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==")

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
        elif name == "sleep":
            # Blocks the read loop for the duration — see the tool comment.
            time.sleep(float(args.get("ms", 0)) / 1000.0)
            reply(id_, {"content": [{"type": "text", "text": "slept"}], "isError": False})
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
        elif name == "screenshot":
            reply(id_, {"content": [
                {"type": "text", "text": "captured"},
                {"type": "image", "data": PNG_1PX, "mimeType": "image/png"},
            ], "isError": False})
        elif name == "structured":
            reply(id_, {"structuredContent": {"answer": 42, "unit": "tools"},
                        "isError": False})
        elif name == "echo":
            reply(id_, {"content": [{"type": "text", "text": args.get("text", "")}], "isError": False})
        elif name == "add":
            reply(id_, {"content": [{"type": "text", "text": str(args.get("a", 0) + args.get("b", 0))}], "isError": False})
        else:
            reply(id_, {"content": [{"type": "text", "text": f"unknown tool {name}"}], "isError": True})
    elif id_ is not None:
        reply(id_, {})
