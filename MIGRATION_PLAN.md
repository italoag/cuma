# Migration plan

How the Go IoT scanner became a Rust agent harness, and what remains.

## Principle

**Non-destructive first.** The brief was explicit: do not rewrite destructively.
The Go tree was moved to `legacy/`, not deleted. It still builds from there. Any
claim in [`CURRENT_ARCHITECTURE.md`](CURRENT_ARCHITECTURE.md) can be checked
against the code it describes.

Only after the analysis was written, the classification made and the
dependencies verified did any Rust get written.

## Sequence followed

| # | Step | Outcome |
|---|---|---|
| 1 | Inspect the repository in full | 29 Go files, ~3,100 lines, 3 test files, no CI |
| 2 | Document the existing architecture | [`CURRENT_ARCHITECTURE.md`](CURRENT_ARCHITECTURE.md) |
| 3 | Classify every component | Nothing KEEP; five patterns worth carrying |
| 4 | Identify risks | Recorded, with mitigations, in the same document |
| 5 | Verify the toolchain and every dependency | Rust 1.94.1; [`DEPENDENCY_ANALYSIS.md`](DEPENDENCY_ANALYSIS.md) |
| 6 | Propose the target architecture | [`TARGET_ARCHITECTURE.md`](TARGET_ARCHITECTURE.md) |
| 7 | Move Go to `legacy/`, establish the workspace | Reversible; history preserved by `git mv` |
| 8 | Implement in vertical slices | Each milestone compiling and tested before the next |

## What moved where

```
cmd/, internal/, configs/, data/, scripts/, deploy/, go.mod, Makefile
    ──> legacy/
```

The module path is unchanged, so `cd legacy && go build ./...` still works.

## Patterns carried across

Not code — judgement. Five ideas from the Go service that were worth
re-expressing:

| From | To | The idea |
|---|---|---|
| `internal/store` | `cuma_core::ports` | An interface with a real and an in-memory implementation, so tests never touch the real thing |
| `internal/hub` | `cuma_core::EventBus` | Fan-out that a slow subscriber cannot stall |
| `internal/config` | `cuma-config` | Layered file + prefixed environment configuration |
| `internal/scanner/scanner.go` | `cuma-orchestrator` | A pipeline orchestrator with explicit lifecycle — generalized from a fixed pipeline to a task DAG |
| `cmd/cuma/main.go` | `cuma-cli` | Explicit dependency injection, graceful shutdown |

## Vertical slices

Each slice was a working system, not a layer. The first ran end to end against
mock agents before any protocol adapter existed.

| Milestone | Delivered | State |
|---|---|---|
| 1 — Foundation | Workspace, domain, ports, config, errors, event bus | Done |
| 2 — ACP | Client adapter on the official SDK, discovery, negotiation | Done |
| 3 — Router | Registries, scoring, strategies, explainability | Done |
| 4 — Orchestration | Planner, task DAG, dependencies, execution loop | Done |
| 5 — Resilience | Retry, fallback, circuit breakers, health | Done |
| 6 — Memory | `MemoryStore` port, external `ai-memory`, handoff | Done |
| 7 — MCP | Tool provider on the official SDK, allowlists | Done |
| 8 — A2A | Agent Cards, JSON-RPC delegation, discovery | Done |
| 9 — Skills | Registry, search, validation, installation | Done |
| 10 — TUI | State machine and view model done; **event loop outstanding** | Partial |
| 11 — Optimization | Context trimming, usage, adaptive routing done; **RTK outstanding** | Partial |

## What is not built

Stated plainly rather than implied.

| Gap | Impact | Where it goes |
|---|---|---|
| TUI event loop | `cuma chat` prints CLI guidance instead of drawing. State machine and view functions are written and tested. | `cuma-tui` |
| RTK integration | No output filtering for shell-heavy tasks. Config surface exists; detection does not. | `cuma-orchestrator` |
| CUMA as an ACP server | Editors cannot yet select CUMA as their agent. The primary goal of the design; the client half is done. | new `cuma-server-acp` |
| CUMA as an A2A server | Other systems cannot delegate to CUMA. | `cuma-protocol-a2a` |
| Parallel task execution | Ready tasks run sequentially. The DAG computes the parallel frontier correctly; workspace isolation (git worktrees, file ownership) is the missing safety mechanism. | `cuma-orchestrator` |
| Skill creation | Generating a skill that does not exist. Highest-risk feature in the brief; deliberately last. | `cuma-skills` |
| Provider SDK adapters | `LlmProvider` has no concrete implementation, so `LlmPlanner` has nothing to call. | new `cuma-providers` |
| Sandbox execution | `security.sandbox` is configured but not enforced. | `cuma-orchestrator` |

## Rollback

The Go service is intact under `legacy/` and the transformation is three
commits. Reverting is a `git revert` and a `git mv` back.

## Deployment changes

The new binary has materially different requirements. The old manifests do not
transfer unchanged:

| | Go scanner | Rust harness |
|---|---|---|
| Privileges | `CAP_NET_RAW`, `CAP_NET_ADMIN` | None |
| Networking | `--network=host` (ARP needs it) | Standard |
| System libraries | `libpcap0.8` | None |
| CGO | Required (gopacket) | Not applicable |
| Data | SQLite device database | SQLite runtime state |
| Child processes | None | Spawns ACP agents and MCP servers |

That last row is the one that matters for containerization: the image needs
whatever runtimes the configured agents require (`npx`, for the published ACP
adapters), which the old image never did.
