#!/bin/sh
# A minimal ACP agent in POSIX sh, for exercising sandboxes: any image with
# `sh` and `sed` can run it, busybox included.
#
# It answers initialize, session/new and session/prompt. On a prompt it writes
# hello.txt in its working directory — the workspace, if the sandbox mounted
# or copied it where ACP said — and reports the text "done".
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  [ -n "$id" ] || id=$(printf '%s' "$line" | sed -n 's/.*"id":\("[^"]*"\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{},"authMethods":[],"agentInfo":{"name":"sh-agent","version":"1.0"}}}\n' "$id"
      ;;
    *'"method":"session/new"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"s1"}}\n' "$id"
      ;;
    *'"method":"session/prompt"'*)
      echo "written by the sandboxed agent" > hello.txt
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"done"}}}}\n'
      printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
      ;;
  esac
done
