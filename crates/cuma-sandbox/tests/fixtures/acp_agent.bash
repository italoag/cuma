#!/bin/bash
# The ACP fixture agent in bash builtins only — for sandboxes whose shell has
# no `sed`, such as Wasmer's `wasmer/bash`. Same behaviour as acp_agent.sh.
while IFS= read -r line; do
  id=
  if [[ $line =~ \"id\":([0-9]+) ]]; then
    id=${BASH_REMATCH[1]}
  elif [[ $line =~ \"id\":(\"[^\"]*\") ]]; then
    id=${BASH_REMATCH[1]}
  fi
  case $line in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{},"authMethods":[],"agentInfo":{"name":"bash-agent","version":"1.0"}}}\n' "$id"
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
