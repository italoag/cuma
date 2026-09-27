# Architecture

The definitive statement is [`../TARGET_ARCHITECTURE.md`](../TARGET_ARCHITECTURE.md).
This page is the working reference: how the pieces fit and where to look.

## The rule everything else follows from

**Nothing protocol-shaped may enter `cuma-core`.**

`cuma-core` holds the domain and the ports. It has no dependency on any protocol
SDK or provider SDK. If a type there needs to know whether an agent speaks ACP
or A2A for anything beyond bookkeeping, the abstraction has leaked.

## Request flow

```
cuma run "implement OAuth and fix the tests"
   │
   ├─ Config::load            defaults → global → project → env → CLI flags
   ├─ build_orchestrator      discover agents, attach memory, restore history
   │
   └─ Orchestrator::run
        ├─ plan               MemoryStore::recall → Planner::plan → TaskGraph
        │
        └─ for each ready wave:
             ├─ route         filter → score → explain    (cuma-router)
             ├─ assemble      minimal context             (ContextManager)
             ├─ execute       AgentAdapter::execute       (ACP / A2A / mock)
             ├─ record        usage, history, breaker     (cuma-usage)
             └─ on failure    classify → retry | reroute | replan | give up
```

## Crate responsibilities

| Crate | Owns | Depends on |
|---|---|---|
| `cuma-core` | Domain, ports, errors, events | nothing |
| `cuma-config` | Layered configuration | core |
| `cuma-registry` | Agent, model, capability registries | core, config |
| `cuma-router` | Scoring, strategies, explanations, adaptive history | core, config, registry, resilience |
| `cuma-resilience` | Backoff, breakers, classification | core |
| `cuma-planner` | Goal → DAG | core |
| `cuma-orchestrator` | Execution loop, context assembly | core, router, resilience, planner, usage, registry |
| `cuma-usage` | Tokens, cost, outcomes | core |
| `cuma-persistence` | SQLite runtime state | core, router, usage |
| `cuma-memory` | `MemoryStore` implementations | core, config |
| `cuma-skills` | Discovery, validation, installation | core, config |
| `cuma-protocol-acp` | ACP adapter | core, config, ACP SDK |
| `cuma-protocol-a2a` | A2A adapter | core, config |
| `cuma-protocol-mcp` | MCP tools | core, config, rmcp |
| `cuma-server-acp` | CUMA *as* an ACP agent | core, orchestrator, ACP SDK |
| `cuma-workspace` | Ownership, checkpoints, worktrees, sandbox, RTK | core, config |
| `cuma-providers` | `LlmProvider` implementations, secret stores | core, config |
| `cuma-testkit` | Mock agents | core |
| `cuma-tui` | View model and rendering | core, orchestrator |
| `cuma-cli` | Headless interface | everything |

## Key types

| Type | Where | Why it matters |
|---|---|---|
| `AgentDescriptor` | `core::agent` | The router's entire view of an agent |
| `Known<T>` | `core::agent` | `Reported` / `Estimated` / `Unknown` — keeps guesses from becoming facts |
| `Capability` | `core::capability` | The shared vocabulary of tasks and agents |
| `TaskGraph` | `core::task` | Owns dependency semantics; the orchestrator never reasons about edges |
| `ErrorClass` | `core::error` | What resilience policy branches on |
| `EventBus` | `core::event` | The only channel between the runtime and any interface |
| `AgentHandoff` | `core::handoff` | Why a fallback costs one prompt, not a transcript replay |
| `RoutingDecision` | `router::explain` | Every decision, explainable after the fact |

## Where to look

| Question | File |
|---|---|
| How is an agent chosen? | `crates/cuma-router/src/router.rs` |
| How is a dimension scored? | `crates/cuma-router/src/score.rs` |
| What happens when something fails? | `crates/cuma-resilience/src/retry.rs` |
| How does a plan get built? | `crates/cuma-planner/src/heuristic.rs` |
| How does a task actually run? | `crates/cuma-orchestrator/src/executor.rs` |
| What does an agent receive? | `crates/cuma-orchestrator/src/context.rs` |
| How is ACP spoken? | `crates/cuma-protocol-acp/src/adapter.rs` |
| How is CUMA *served* as an agent? | `crates/cuma-server-acp/src/lib.rs` |
| What makes parallelism safe? | `crates/cuma-workspace/src/ownership.rs` |
