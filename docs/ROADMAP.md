# Roadmap

## Where things stand

380 tests passing, zero build warnings, verified against a live ACP agent.

| Milestone | State |
|---|---|
| 1 — Foundation | **Done** |
| 2 — ACP | **Done** |
| 3 — Router | **Done** |
| 4 — Orchestration | **Done** |
| 5 — Resilience | **Done** |
| 6 — Memory | **Done** |
| 7 — MCP | **Done** |
| 8 — A2A | **Done** |
| 9 — Skills | **Done** |
| 10 — TUI | **Partial** — state machine and view functions done and tested; event loop outstanding |
| 11 — Optimization | **Partial** — context trimming, usage and adaptive routing done; RTK outstanding |

## Acceptance criteria

Against the product definition, stated honestly.

| Criterion | State |
|---|---|
| A single meta-agent as the interface | **Done** (CLI; TUI partial) |
| ACP agent support | **Done** |
| A2A agent support | **Done** |
| MCP tool support | **Done** |
| Agent registry | **Done** |
| Model registry | **Done** |
| Capability discovery | **Done** |
| Task planning and decomposition | **Done** |
| Intelligent routing | **Done** |
| Cost / quality / health routing | **Done** |
| Retry | **Done** |
| Fallback | **Done** |
| Circuit breaker | **Done** |
| Health monitoring | **Done** |
| Long-term memory | **Done** |
| Agent handoff | **Done** |
| Skill discovery | **Done** |
| Skill installation | **Done** |
| Skill security | **Done** |
| Usage tracking | **Done** |
| Cost estimation | **Done** |
| Token statistics | **Done** |
| Structured logging | **Done** |
| Security boundaries | **Done** |
| Unit tests | **Done** |
| Integration tests | **Done** |
| Mock agents | **Done** |
| Architecture documentation | **Done** |
| ADRs | **Done** |
| CLI headless | **Done** |
| **Safe parallel execution** | **Not done** — DAG computes the frontier; workspace isolation missing |
| **TUI** | **Partial** |
| **RTK integration** | **Not done** |
| **Skill creation** | **Not done** |

## Next, in order

Ordered by what unblocks the most.

### 1. TUI event loop

The state machine (`cuma_tui::AppState`) and the view functions are written and
tested without a terminal. What remains is the crossterm loop: subscribe, fold,
draw, handle keys.

*First because it is the only unfinished item whose design is already settled.*

### 2. CUMA as an ACP server

The primary architectural goal.

```
JetBrains / Zed ──ACP──> CUMA ──┬──ACP──> Codex
                                ├──ACP──> Claude Code
                                └──A2A──> remote architect
```

The client half works and the SDK supports the agent role. A new
`cuma-server-acp` crate implementing `Agent` and forwarding to the orchestrator.

### 3. Safe parallel execution

Not a scheduling problem — a safety one. Dependency independence is not
workspace independence. Needs git worktrees, file ownership tracking and merge
coordination before `max_parallel_tasks` can mean anything.

### 4. RTK integration

Detect on `PATH`, wrap shell-heavy tool calls, record tokens saved. The
configuration surface and the usage counter already exist.

### 5. Provider adapters

A concrete `LlmProvider` so `LlmPlanner` has something to call. In a
`cuma-providers` crate, behind the port — never scattered through the domain.

### 6. Sandbox enforcement

`security.sandbox` is configured and unenforced. Wire `sandbox_command` into
agent and skill execution.

### 7. Skill creation

Generate, test, validate, register. The highest-risk feature in the brief,
deliberately last, gated behind `allow_creation = false` by default.

## Later

- CUMA as an A2A server
- Remote skill registries and signature verification
- OpenTelemetry export
- Session reuse across tasks (ACP `session/load`)
- Benchmarks for routing, context selection and registry lookup
- A web interface, as another event-bus subscriber
