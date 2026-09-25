# ADR-016 — Agents reach shared MCP servers through CUMA's proxy

**Status:** Accepted

*Extends [ADR-004](ADR-004-mcp-tools.md).*

## Context

MCP servers configured once in CUMA should be usable by the agents it
delegates to — that is much of the point of a control plane. ACP lets a client
hand an agent MCP servers in `session/new`, as a command the agent launches
itself.

Handing over the server's own command would bypass both of CUMA's controls:
the agent would see every tool, whatever `allowed_tools` says, and the server's
environment — tokens included — would have to be written into the session
request in plain text.

## Decision

A server marked `share_with_agents = true` (off by default) is handed to agents
as `cuma --workspace <dir> mcp proxy <name>`. The proxy is CUMA's own MCP server
(`ToolServer`) in front of the real one: it lists only allowed tools, refuses
calls to others, and resolves `$VAR` secrets from its own environment.

The same `ToolServer`, over a different provider, is `cuma serve --protocol mcp`,
which exposes `cuma_run`, `cuma_explain` and `cuma_agents` to any MCP host.

## Consequences

- An allowlist holds whoever the client is.
- No secret appears in an ACP message.
- Each tool call through the proxy launches the real server; connection reuse
  is future work.
- Exposing `cuma_run` to an agent CUMA itself routes to would let the two
  delegate to each other until the budget stopped them, so CUMA's own MCP
  server is never shared automatically.
