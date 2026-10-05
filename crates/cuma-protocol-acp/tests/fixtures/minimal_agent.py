#!/usr/bin/env python3
"""A minimal ACP agent over stdio, for testing negotiation.

It answers `initialize` advertising no images and no HTTP or SSE MCP servers
— nothing beyond the ACP coding baseline — opens one session, and ends every
prompt turn at once with "done".
"""
import json
import sys


def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    message = json.loads(line)
    method, mid = message.get("method"), message.get("id")
    if method is None or mid is None:
        continue
    if method == "initialize":
        result = {
            "protocolVersion": 1,
            "agentCapabilities": {
                "promptCapabilities": {"image": False},
                "mcpCapabilities": {"http": False, "sse": False},
            },
            "authMethods": [],
            "agentInfo": {"name": "minimal", "version": "0.0.1"},
        }
    elif method == "session/new":
        result = {"sessionId": "session-1"}
    elif method == "session/prompt":
        send({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": message["params"]["sessionId"],
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": "done"},
                },
            },
        })
        result = {"stopReason": "end_turn"}
    else:
        send({"jsonrpc": "2.0", "id": mid,
              "error": {"code": -32601, "message": "method not found"}})
        continue
    send({"jsonrpc": "2.0", "id": mid, "result": result})
