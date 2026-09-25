# Roadmap

## Where things stand

746 tests passing, clippy clean with warnings denied, checked on the MSRV
(Rust 1.88) with and without the `otel` feature.

| Milestone | State |
|---|---|
| 1 — Foundation | **Done** |
| 2 — ACP | **Done** — client, server with a workspace per session, `session/load`, `session/cancel`, registry discovery |
| 3 — Router | **Done** |
| 4 — Orchestration | **Done** — including worktree isolation |
| 5 — Resilience | **Done** |
| 6 — Memory | **Done** — ai-memory over its CLI or its MCP server, handoffs included |
| 7 — MCP | **Done** — client, server, allowlist-enforcing proxy shared with agents |
| 8 — A2A | **Done** — 1.0 dialect with 0.3 fallback, task lifecycle persisted across restarts, SSE |
| 9 — Skills | **Done** — content digests, Ed25519 signatures, git and HTTPS registries |
| 10 — TUI | **Done** |
| 11 — Optimization | **Done** — RTK verified and measured, benchmarks |

## Acceptance criteria

| Criterion | State | Where |
|---|---|---|
| A single meta-agent as the interface | **Done** | CLI, TUI, ACP, A2A, MCP servers |
| ACP agent support | **Done** | `cuma-protocol-acp` |
| A2A agent support | **Done** | `cuma-protocol-a2a` |
| MCP tool support | **Done** | `cuma-protocol-mcp` |
| Agent, model and capability registries | **Done** | `cuma-registry` |
| Planning and decomposition | **Done** | `cuma-planner` |
| Explainable multi-dimensional routing | **Done** | `cuma-router` |
| Retry, fallback, circuit breaker, replan | **Done** | `cuma-resilience`, orchestrator |
| Health monitoring, persisted | **Done** | `agent_health` table |
| Long-term memory | **Done** | `cuma-memory` |
| Agent handoff | **Done** | `AgentHandoff`, `HandoffPerformed`, ai-memory handoffs |
| Skill discovery, installation, security | **Done** | `cuma-skills` |
| Skill creation | **Done** | generated skills install disabled and `Untrusted` |
| Usage tracking, reported vs estimated | **Done** | `Known<T>`, `TokenUsage.reported`, `cost_reported` |
| Structured logging and tracing | **Done** | spans per command, session, task, attempt |
| OpenTelemetry export | **Done** | `otel` feature |
| Safe parallel execution | **Done** | ownership ledger, worktree isolation |
| Sandboxing | **Done** | agents confined by ai-jail, bubblewrap, `sandbox-exec` or firejail |
| RTK integration | **Done** | verified with `rtk gain`; measured savings reported |
| CUMA as an ACP / A2A / MCP agent | **Done** | `cuma serve --protocol acp \| a2a \| mcp` |
| Provider adapters | **Done** | `cuma-providers` |
| Tests, mock agents, benchmarks | **Done** | `cuma-testkit`, `benches/` |

## Measured overhead

From `cargo bench` on a development machine; what CUMA costs on top of the
agents it runs.

| Step | Time |
|---|---|
| Route one task across 5 / 50 / 200 agents | 9 µs / 108 µs / 521 µs |
| Publish 1 000 events to 4 subscribers | 121 µs |
| Ready set of a 200-task graph | 12 µs |
| Validate a 200-task graph | 1.3 ms |
| Assemble a task's context | 0.3 µs |
| Predict a task's writes against a 50 000-file index | 3.8 ms |
| Digest a 100-file skill / verify its signature | 606 µs / 45 µs |
| Record an attempt in SQLite | 33 µs |
| An MCP tool call through `cuma mcp proxy`, connection reused (debug build) | ~4 ms, against ~28 ms relaunching the server per call |

Nothing here is within three orders of magnitude of an agent's own latency.
Graph validation and write prediction are linear scans that could be indexed
if a plan or a repository ever grew large enough to matter.

## What is not built

| Gap | Consequence |
|---|---|
| **Client MCP servers in CUMA-as-ACP** | An editor's `mcp_servers` in `session/new` are accepted and **silently dropped**; only `[mcp.*]` servers marked `share_with_agents` reach the agents CUMA delegates to. ACP requires every agent to accept stdio MCP servers from its client, so on this point CUMA is out of spec, and the editor cannot tell. See [the plan below](#next-editor-mcp-servers-for-the-agents-behind-cuma-as-acp). |
| **A2A beyond the task lifecycle** | Push notifications and the extended Agent Card are refused with their own error codes and advertised `false`. CUMA never pauses for input, so multi-turn tasks are refused. A task interrupted by a restart is reported failed, not resumed. |
| **A2A authentication beyond bearer tokens** | Both directions use bearer tokens from secret handles; there are no OAuth, OpenID Connect or mTLS flows. |
| **Binary agents from the ACP registry** | `cuma agents add` configures npx and uvx agents; binary distributions are described (URL, SHA-256) for a person to install. |
| **Network filtering without ai-jail** | bubblewrap, `sandbox-exec` and firejail confine the filesystem and environment but cannot filter by host; a `network_allowlist` is reported as not enforced. With no runtime installed at all, agents run unconfined unless `require_agent_sandbox` is set. |
| **macOS confinement, exercised** | The `sandbox-exec` profile is unit-tested; the Linux runtimes were checked with a probe agent, macOS was not. |
| **Write prediction** | Grounded in a file index and dependency outputs, but still read from a task's description. A task naming no path claims the whole workspace, which is safe and serializing. |
| **Skill revocation and pinning** | A trusted key cannot be revoked short of removing it from configuration, and git registries are not pinned to a commit in `installed.json`. |
| **ACP per-turn token usage** | Read from `PromptResponse.usage`, which the ACP schema marks unstable; when absent, input comes from `UsageUpdate` and output is estimated, and the total is labelled estimated. |
| **ratatui-bubbletea components** | The TUI is plain Ratatui; the Bubble Tea–style component crates were evaluated (see `DEPENDENCY_ANALYSIS.md`) and not adopted. |

## Next: editor MCP servers for the agents behind CUMA-as-ACP

Not a protocol limitation — an unbuilt path, with security decisions that
should be made explicitly rather than by forwarding whatever arrives.

**Why it is not a simple pass-through**

1. *No path exists.* An ACP adapter's MCP servers are fixed when the
   orchestrator is built — one per workspace, shared by its sessions — and
   nothing carries per-session tools down to an execution. That needs a
   protocol-neutral field on the execution request in `cuma-core`.
2. *Secrets.* A stdio server from the editor carries `env` **values**, not
   handles. Forwarding them puts the editor's tokens in messages CUMA writes
   and in domain memory, against the rule that only handles are stored.
3. *Not every agent can receive them.* An A2A agent is remote: it cannot
   launch a local process, and sending it the editor's commands and env would
   be a leak. A task needing these tools should prefer local ACP agents —
   which makes this a routing concern, not only a transport one.
4. *Control.* CUMA's own servers go through `cuma mcp proxy`, which enforces
   `allowed_tools`; the editor's have no allowlist in CUMA.
5. *HTTP and SSE.* CUMA advertises `mcpCapabilities.http/sse = false`.
   Accepting them means knowing which downstream agents support them (from
   their `initialize`) and leaving them out elsewhere.

**Plan**

- Keep each ACP session's `mcp_servers` (validated, count-capped) in memory
  and pass them to every execution of that session. `env` values are never
  persisted — not in `.cuma/acp-sessions/`, not in logs, not in the runtime
  database; a reloaded session asks the editor again, which ACP already does
  by sending `mcp_servers` with `session/load`.
- Hand them only to local ACP agents, merged with CUMA's shared servers
  (editor names win on collision, or are prefixed — to decide), never to A2A
  agents.
- Routing: when a session carries editor tools, prefer agents that will
  receive them; say so in the explanation when one that will not is chosen.
- Sandbox: keep the servers' commands readable and their env available to
  the agent that launches them.
- Decide item 4. Proposed: forward the editor's servers as they came — the
  editor already chose to trust them — and keep the proxy for CUMA's own.
- Until this lands, stop dropping silently: tell the editor, once per
  session, that its MCP servers are not forwarded.

**Size.** Medium: `cuma-core` (execution request), the orchestrator (per-run
tool servers), `cuma-server-acp` (session state), `cuma-protocol-acp`
(merge into `session/new`), the router (preference), and tests through the
real ACP client SDK with a stdio MCP server fixture.

## Later

- A2A push notifications
- Commit-pinned skill installs and a key revocation list
- A web interface, as another event-bus subscriber
