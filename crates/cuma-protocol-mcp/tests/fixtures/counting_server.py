#!/usr/bin/env python3
"""A minimal MCP server over stdio, for testing connection reuse.

Tools:
  whoami  -> "pid=<pid> calls=<n>": the same pid across calls means the
             connection was reused, and calls counts this process's calls.
  slow    -> sleeps 300 ms, then answers like whoami.
  die     -> exits at once without answering, as a crashing server would.
"""
import json
import os
import sys
import threading
import time

calls = 0
lock = threading.Lock()


def reply(mid, result=None, error=None):
    message = {"jsonrpc": "2.0", "id": mid}
    if error is not None:
        message["error"] = error
    else:
        message["result"] = result
    with lock:
        sys.stdout.write(json.dumps(message) + "\n")
        sys.stdout.flush()


def answer(mid, delay):
    # Answered on its own thread, so concurrent calls overlap here and any
    # serialisation measured is the client's.
    global calls
    time.sleep(delay)
    with lock:
        calls += 1
        count = calls
    reply(mid, text("pid=%d calls=%d" % (os.getpid(), count)))


def text(value):
    return {"content": [{"type": "text", "text": value}], "isError": False}


TOOLS = [
    {"name": name, "description": name, "inputSchema": {"type": "object"}}
    for name in ("whoami", "slow", "die")
]

for line in sys.stdin:
    try:
        message = json.loads(line)
    except ValueError:
        continue
    method, mid = message.get("method"), message.get("id")
    params = message.get("params") or {}
    if mid is None:
        continue  # notifications need no answer
    if method == "initialize":
        reply(mid, {
            "protocolVersion": params.get("protocolVersion", "2025-06-18"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "counting", "version": "1.0.0"},
        })
    elif method == "tools/list":
        reply(mid, {"tools": TOOLS})
    elif method == "tools/call":
        name = params.get("name")
        if name == "die":
            os._exit(3)
        delay = 0.3 if name == "slow" else 0.0
        threading.Thread(target=answer, args=(mid, delay)).start()
    elif method == "ping":
        reply(mid, {})
    else:
        reply(mid, error={"code": -32601, "message": "unsupported: %s" % method})
