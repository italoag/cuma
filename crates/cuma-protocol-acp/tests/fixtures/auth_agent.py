#!/usr/bin/env python3
"""An ACP agent that requires `authenticate` before `session/new`.

Advertises one agent-managed auth method and answers `session/new` with
AUTH_REQUIRED (-32000) until `authenticate` has been called — the handshake
an agent that manages its own login performs.
"""
import json
import sys

authenticated = False


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
            "agentCapabilities": {"promptCapabilities": {"image": False}},
            "authMethods": [{"id": "device-login", "name": "Log in with a device code"}],
            "agentInfo": {"name": "authy", "version": "0.0.1"},
        }
    elif method == "authenticate":
        authenticated = True
        result = {}
    elif method == "session/new":
        if not authenticated:
            send({"jsonrpc": "2.0", "id": mid,
                  "error": {"code": -32000, "message": "Authentication required"}})
            continue
        result = {"sessionId": "session-1"}
    elif method == "session/prompt":
        result = {"stopReason": "end_turn"}
    else:
        send({"jsonrpc": "2.0", "id": mid,
              "error": {"code": -32601, "message": "method not found"}})
        continue
    send({"jsonrpc": "2.0", "id": mid, "result": result})
