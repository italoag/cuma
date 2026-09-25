# Target architecture

CUMA is a **control plane for coding agents**. The user talks to one agent;
the system coordinates many.

It is not a wrapper around LLM APIs. It is a harness that receives an intent,
understands it, decomposes it, discovers what agents and models are available,
and delegates each piece of work to whichever one is best suited — then
observes, retries, reroutes and accounts for all of it.

## The shape of the system

```
                        ┌────────────────────┐
                        │        USER        │
                        └─────────┬──────────┘
                                  │  intent
                                  ▼
              ┌───────────────────────────────────────┐
              │          META AGENT HARNESS           │
              │                                       │
              │  Planner        decompose the goal    │
              │  Router         choose agent + model  │
              │  Orchestrator   walk the task DAG     │
              │  Context Mgr    assemble minimal ctx  │
              │  Memory Mgr     recall and record     │
              │  Skill Mgr      close capability gaps │
              │  Resilience     retry, fallback, trip │
              │  Usage          tokens, cost, health  │
              └───────────────┬───────────────────────┘
                              │
              ┌───────────────┼───────────────┐
             ACP             A2A             MCP
              │               │               │
              ▼               ▼               ▼
       Coding agents    Remote agents    Tools/resources
       (local, spawned) (peer systems)   (git, fs, docs)
```

## The one rule

**Nothing protocol-shaped may enter `cuma-core`.**

`cuma-core` defines tasks, agents, models, capabilities and errors. It defines
*ports* — traits the outer layers implement. It has no dependency on any
protocol SDK or provider SDK, and it never will.

Everything else follows from that. A new protocol is a new adapter crate. A new
front end subscribes to the event bus. Neither touches the orchestrator.

## Crate layout

```
crates/
├── cuma-core              domain + ports + errors + events         (no protocol deps)
├── cuma-config            layered declarative configuration
│
├── cuma-planner           goal → task DAG
├── cuma-router            filter → score → explain
├── cuma-orchestrator      DAG execution, retry, fallback, handoff
├── cuma-resilience        backoff, circuit breakers, classification
├── cuma-registry          agent / model / capability registries
│
├── cuma-protocol-acp      ← agent-client-protocol (official SDK)
├── cuma-protocol-a2a      ← Agent Cards + JSON-RPC
├── cuma-protocol-mcp      ← rmcp (official SDK)
│
├── cuma-memory            ← ai-memory, as an external process
├── cuma-skills            discovery, validation, installation
├── cuma-persistence       ← SQLite runtime state
├── cuma-usage             tokens, cost, outcomes
│
├── cuma-testkit           mock agents for every failure mode
├── cuma-tui               event-bus subscriber, no runtime state
└── cuma-cli               headless interface
```

Dependencies point inward. `cuma-core` depends on nothing in this list.

## The ports

Every trait in `cuma_core::ports` is the complete surface between the core and
the world outside it.

| Port | Implemented by | Purpose |
|---|---|---|
| `AgentAdapter` | ACP, A2A, mocks | Execute one task on one agent |
| `AgentDiscovery` | config, ACP, A2A | Find agents |
| `Planner` | heuristic, LLM-assisted | Decompose a goal |
| `ContextManager` | `MinimalContextManager` | Assemble the smallest useful prompt |
| `MemoryStore` | `AiMemoryCli`, `NullMemory` | Long-term shared memory |
| `ToolProvider` | MCP | Tools |
| `SkillRegistry` | local, built-in | Find and install skills |
| `LlmProvider` | provider SDKs | Direct model access, for the harness's own reasoning |
| `SecretStore` | keychain, environment | Resolve a handle into a secret at point of use |

`AgentAdapter` is the important one: it is what makes an ACP process, an A2A
endpoint and an in-process mock interchangeable to the orchestrator.

## The three protocols

They are not alternatives to each other. They occupy different layers.

### ACP — coding agents

The preferred transport for local coding agents. It is what editors already
speak, and — crucially — it reuses the agent's **own authenticated session**, so
the harness never needs an API key to drive Claude Code or Codex.

Built on the official `agent-client-protocol` Rust SDK. CUMA does not
reimplement JSON-RPC, framing or the schema.

### A2A — peer agents

For independent, often remote agents. Discovery is by Agent Card; delegation is
by JSON-RPC. Used for agents that are not coding CLIs — a remote architecture
reviewer, a specialist service.

### MCP — tools

The tool layer, orthogonal to both. CUMA is an MCP client so that git, the
filesystem, documentation search and anything else an operator configures can be
shared across every agent rather than configured once per agent.

## Routing

The single most important component, and the one most easily got wrong.

**Never "use the most powerful model".** Routing is a three-stage pipeline:

1. **Filter.** Hard constraints. Pins, exclusions, health, circuit breakers,
   missing capabilities, budget ceilings. A filtered candidate can never be
   rescued by a high score — an agent that cannot do the work does not become
   able to by being cheap.

2. **Score.** Five weighted dimensions, each normalized to `[0,1]` with higher
   always better:

   ```
   score = quality·w_q + cost·w_c + latency·w_l + reliability·w_r + context·w_x
   ```

   Unknown metrics score at a neutral midpoint, never at the best value.
   Rewarding an agent for disclosing nothing would make silence the winning
   strategy.

3. **Explain.** Every decision carries its full breakdown, its runners-up, and
   the reason each rejected candidate was rejected. Weights are
   operator-tunable, and an operator cannot tune what they cannot see.

See [`docs/ROUTING.md`](docs/ROUTING.md).

## Resilience

Three invariants, each backed by tests:

- **Retries are bounded.** No configuration produces an infinite loop.
- **Failures are never silent.** Every decision is a value that becomes an event.
- **Reaction follows classification.** Policy branches on `ErrorClass`, never on
  an error's text.

A rate limit backs off against the same agent. A crash reroutes immediately —
retrying a dead process wastes a whole attempt. An auth failure gives up at once.
A context overflow asks the planner for a smaller plan rather than trying the
same oversized prompt somewhere else.

Circuit breakers are keyed per agent *and* per agent+model, so one overloaded
model does not disqualify its siblings while a crashed process disqualifies them
all.

See [`docs/ORCHESTRATION.md`](docs/ORCHESTRATION.md).

## Handoff

When agent A fails and agent B takes over, B does not replay A's transcript. It
receives a structured summary: what was done, what remains, what was decided,
what to avoid. That is the difference between a fallback costing one prompt and
one costing a whole conversation.

## Accounting

An estimate is never presented as a measurement.

Agents differ in what they report. `Known<T>` distinguishes `Reported`,
`Estimated` and `Unknown` throughout, and it propagates: a total built from one
estimate is an estimate. Cost is `None` rather than `$0.00` when pricing is
unknown, and a partially-priced total renders as a lower bound.

## Security posture

Everything from outside is data, never instructions: agent output, MCP results,
A2A artifacts, Agent Cards, skill manifests, repository contents.

Defaults deny. Destructive operations are off. Skill creation is off. Sandboxing
is on. Auto-install admits only `Trusted`. Cleartext remote endpoints are
refused. Secrets are stored as *handles* and resolved at point of use.

See [`docs/SECURITY.md`](docs/SECURITY.md).

## Two directions, one core

CUMA consumes ACP and A2A agents. It is also designed to be *exposed* as one:

```
JetBrains / Zed / VS Code
        │
       ACP
        ▼
    CUMA  ──┬── ACP ──> Codex
            ├── ACP ──> Claude Code
            ├── A2A ──> remote architect
            └── MCP ──> git, docs, browser
```

From the editor's perspective there is a single agent. Behind it is the whole
routing apparatus. This is a primary architectural goal, not an afterthought —
it is why the core is independent of every interface.

## Status

See [`docs/ROADMAP.md`](docs/ROADMAP.md) for what is built and what is not.
