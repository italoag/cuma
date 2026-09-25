# Protocols

Three protocols, three layers. They are not alternatives to each other.

| | ACP | A2A | MCP |
|---|---|---|---|
| Talks to | Local coding agents | Peer agents, often remote | Tools and resources |
| Transport | stdio, JSON-RPC | HTTPS, JSON-RPC | stdio, JSON-RPC |
| Discovery | Configured command, or the ACP registry | Agent Card | Configured command |
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

**Finding agents.** The ACP project publishes a registry of agents and how to
launch each. `cuma agents discover --registry` lists it; `cuma agents add <id>`
writes the `[agents.<id>]` entry for an npx or uvx agent. Binary-only agents are
described — archive and SHA-256 — for a person to install. The registry is
cached in `.cuma/cache/` and the cached copy used, marked stale, offline.

**Lifecycle.** A process per execution: spawn → `initialize` → `session/new`
(with any shared MCP servers) → `session/prompt` → shut down. See ADR-009 for
why it is not reused. Under ai-jail the agent is launched inside the sandbox;
see [SECURITY.md](SECURITY.md).

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

**Tokens and cost.** Taken, in order of preference, from the turn's own usage
(`PromptResponse.usage`, reported), from the last `UsageUpdate` (context size
as input, output estimated — the whole marked estimated), or estimated from the
text exchanged. A USD cost in `UsageUpdate` is recorded as the agent's reported
cost in preference to a price-table estimate; other currencies are not
converted. **Changed files** are not reported by ACP per turn and are not
invented.

## A2A

For agents that are not local coding CLIs. Discovery is by Agent Card at
`/.well-known/agent-card.json` (at the origin, then under the endpoint's path).
CUMA speaks A2A 1.0 and falls back to 0.3 for peers that do not know the 1.0
method names — see [ADR-015](adr/ADR-015-a2a-dialects.md).

| Peer does | CUMA does |
|---|---|
| Advertises streaming | `SendStreamingMessage`, following SSE events as they arrive |
| Returns a running task | Polls `GetTask` with backoff until it settles |
| Finishes `COMPLETED` | Success |
| Stops at `INPUT_REQUIRED` / `AUTH_REQUIRED` | Failure, with what the peer asked |
| Ends `FAILED` / `REJECTED` / `CANCELED` | Failure |
| Outlives the task's deadline | `CancelTask`, then a timeout |

```toml
[agents.architect]
protocol = "a2a"
endpoint = "https://architect.example/a2a"
auth_secret_ref = "CUMA_ARCHITECT_TOKEN"   # a handle, never a token
```

A remote agent is the least trusted thing the harness talks to, so: cleartext is
refused unless the host is unambiguously local — including an endpoint a card
redirects to — bodies and streams are capped at 8MB, and card tags are
sanitized before becoming capability names. A2A reports no tokens, so they are
estimated from the text and marked as such. See ADR-003.

## MCP

The tool layer, shared across every agent rather than configured per agent.

```toml
[mcp.git]
command = "uvx"
args = ["mcp-server-git"]
allowed_tools = ["git_status", "git_diff", "git_log"]   # not git_commit
share_with_agents = true                                # off by default

[mcp.github]
command = "github-mcp-server"
args = ["stdio"]
env = { GITHUB_PERSONAL_ACCESS_TOKEN = "$GH_TOKEN" }    # a reference, resolved at spawn
```

```bash
cuma mcp list                          # configured servers
cuma mcp tools [--server git]          # what they expose, allowlists applied
cuma mcp call git_status --args '{"repo_path": "."}'
```

**Sharing with agents.** A server marked `share_with_agents` is handed to every
ACP agent in `session/new` — as `cuma mcp proxy <name>`, not as the server's
own command. The proxy enforces the allowlist and resolves secrets inside CUMA,
so neither depends on the agent behaving and no token travels in an ACP
message. See [ADR-016](adr/ADR-016-mcp-sharing.md).

An allowlist is the difference between "the agent can read the repository" and
"the agent can do whatever this server implements". It is enforced at discovery
*and* again at call time — a cached descriptor is not an authorization decision.

Tool results are untrusted data, truncated at 32,000 characters, and never
interpreted as instructions.

## Serving, not just consuming

CUMA implements the agent role of both protocols it consumes.

```bash
cuma serve --protocol acp                      # stdio; an editor selects CUMA
cuma serve --protocol a2a --bind 127.0.0.1:8420  # HTTP; peers delegate to CUMA
cuma serve --protocol mcp                      # stdio; any MCP host calls CUMA as tools
```

| | Implemented | Advertised `false` / refused |
|---|---|---|
| ACP | `session/new`, `session/prompt` (streamed), `session/cancel`, `session/load` with a persisted history | image and audio prompts |
| A2A | `SendMessage` (blocking or `returnImmediately`), `SendStreamingMessage`, `SubscribeToTask`, `GetTask`, `ListTasks`, `CancelTask` — in 1.0 and 0.3 | push notifications (`-32003`), extended card (`-32007`), follow-up messages to a task (`-32004`) |
| MCP | `cuma_run`, `cuma_explain`, `cuma_agents` | — |

Concurrent ACP sessions and A2A tasks each run as their own orchestrator
session and see only their own events. ACP sessions are stored in
`.cuma/acp-sessions/`, so an editor can `session/load` one after CUMA restarts;
`load_session` is advertised only when that store exists. The A2A server does
not authenticate callers: keep the default loopback bind, or put it behind a
proxy that authenticates.

Both under-claim deliberately — an unimplemented capability is advertised
`false` rather than claimed, because an editor that relies on a capability CUMA
does not have fails worse than one that never asked. See
[ADR-012](adr/ADR-012-bidirectional-protocols.md).

The A2A Agent Card's advertised skills are derived from what CUMA's *registered
agents* can actually do, so a peer routing to CUMA is not misled.

## Adding a protocol

1. New crate, `cuma-protocol-<name>`.
2. Implement `AgentAdapter` — and `AgentDiscovery` if agents can be found rather
   than only configured.
3. Translate the wire vocabulary into `ExecutionUpdate` and `ExecutionOutcome`
   at the crate boundary. Nothing protocol-shaped leaves.
4. Classify failures into `ErrorClass` from structured data where possible;
   `classify_message` is the fallback, not the primary path.

No change to the orchestrator, the router or the core.
