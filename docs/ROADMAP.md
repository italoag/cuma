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
| 10 — TUI | **Done** |
| 11 — Optimization | **Done** |

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
| Safe parallel execution | **Done** |
| TUI | **Done** |
| RTK integration | **Done** |
| Skill creation | **Done** |
| CUMA as an ACP agent | **Done** |
| CUMA as an A2A agent | **Done** |
| Sandboxing | **Done** |
| Provider adapters | **Done** |

## What is not built

Stated plainly rather than implied.

| Gap | Consequence |
|---|---|
| **Skill signature verification** | Presence of a checksum and signature is checked; neither is cryptographically validated. `Verified` currently means "claims integrity metadata", not "integrity proven". |
| **A2A streaming and task lifecycle** | `message/send` runs synchronously; `tasks/get` and `tasks/cancel` report that there is nothing to address afterwards. The Agent Card does not claim streaming. |
| **ACP `session/load`** | No resume path, so a session cannot be restored mid-flight. Advertised as `false` rather than claimed. |
| **Worktree-per-task execution** | Worktrees are implemented and tested; the orchestrator isolates by file ownership instead. Ownership is sufficient for correctness; worktrees would raise the achievable parallelism. |
| **Write prediction quality** | Paths are guessed from a task's description, so an unpredictable task claims the whole workspace and over-serializes. Better prediction is the main lever on achievable parallelism. |
| **Remote skill registries** | The `SkillRegistry` trait supports multiple backends; only built-in and local-directory registries exist. |
| **OpenTelemetry export** | `tracing` is the substrate, so this is a subscriber change rather than an instrumentation change. |
| **Benchmarks** | No measurements for routing, context selection or registry lookup. |

## Later

- CUMA as an A2A server
- Remote skill registries and signature verification
- OpenTelemetry export
- Session reuse across tasks (ACP `session/load`)
- Benchmarks for routing, context selection and registry lookup
- A web interface, as another event-bus subscriber
