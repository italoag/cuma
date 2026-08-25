# Protocols

Three protocols, three layers. They are not alternatives to each other.

| | ACP | A2A | MCP |
|---|---|---|---|
| Talks to | Local coding agents | Peer agents, often remote | Tools and resources |
| Transport | stdio, JSON-RPC | HTTPS, JSON-RPC | stdio, JSON-RPC |
| Discovery | Configured command | Agent Card | Configured command |
| Authentication | **The agent's own** | Bearer token by handle | Environment by handle |
| Implementation | Official Rust SDK | Native (see ADR-003) | Official `rmcp` SDK |

## ACP

The preferred path for local coding agents. Its decisive advantage is
authentication: an ACP agent manages its own credentials, so driving an
already-logged-in Claude Code or Codex reuses the user's subscription and the
harness never holds an API key.

```toml
[agents.codex]
protocol = "acp"
# Well-known agents need no command; codex and claude-code resolve automatically.

[agents.my-agent]
protocol = "acp"
command = "my-agent --acp"
```

**Lifecycle.** A process per execution: spawn → `initialize` → `session/new` →
`session/prompt` → shut down. See ADR-009 for why it is not reused.

**Capabilities.** ACP negotiates *protocol* features, not what an agent is good
at. The mapping is therefore partly read and partly assumed: `prompt.image`
becomes `Vision`, MCP-over-HTTP becomes `Research`, and a conservative coding
baseline is assumed because every ACP agent is a coding agent by construction.
Anything more specific belongs in configuration.

**Permissions.** Answered from the task's `Risk`, not from how the agent phrased
the request. An unattended run must not block on a prompt nobody will see.

| Policy | Permits |
|---|---|
| `AlwaysAllow` | Everything. Only safe inside a sandbox. |
| `AllowLowRisk` *(default)* | `ReadOnly` and `Low` risk |
| `AlwaysDeny` | Nothing |

**Stop reasons** map onto classification, and one mapping matters: `MaxTokens`
becomes `ContextOverflow`, which triggers a *replan*. Treating it as a generic
failure would retry the same oversized prompt somewhere else and fail
identically. `StopReason` is `#[non_exhaustive]`, so an unrecognized reason is
treated as a failure — marking unfinished work as done would be worse.

**Not reported by ACP:** changed files per turn, and tokens (behind an unstable
feature). Both surface as unknown rather than fabricated.

## A2A

For agents that are not local coding CLIs. Discovery is by Agent Card at
`/.well-known/agent-card.json`; delegation is `message/send`.

```toml
[agents.architect]
protocol = "a2a"
endpoint = "https://architect.example/a2a"
auth_secret_ref = "CUMA_ARCHITECT_TOKEN"   # a handle, never a token
```

A remote agent is the least trusted thing the harness talks to, so: cleartext is
refused unless the host is unambiguously local, bodies are capped at 8MB, and
card tags are sanitized before becoming capability names. See ADR-003.

## MCP

The tool layer, shared across every agent rather than configured per agent.

```toml
[mcp.git]
command = "git-mcp-server"
allowed_tools = ["git_status", "git_diff", "git_log"]   # not git_push

[mcp.github]
command = "github-mcp-server"
env = { GITHUB_TOKEN = "$GH_TOKEN" }                    # a reference, resolved at spawn
```

An allowlist is the difference between "the agent can read the repository" and
"the agent can do whatever this server implements". It is enforced at discovery
*and* again at call time — a cached descriptor is not an authorization decision.

Tool results are untrusted data, truncated at 32,000 characters, and never
interpreted as instructions.

## Adding a protocol

1. New crate, `cuma-protocol-<name>`.
2. Implement `AgentAdapter` — and `AgentDiscovery` if agents can be found rather
   than only configured.
3. Translate the wire vocabulary into `ExecutionUpdate` and `ExecutionOutcome`
   at the crate boundary. Nothing protocol-shaped leaves.
4. Classify failures into `ErrorClass` from structured data where possible;
   `classify_message` is the fallback, not the primary path.

No change to the orchestrator, the router or the core.
