# CUMA

**A universal control plane for coding agents.**

You talk to one agent. It coordinates many.

CUMA receives an intent, understands it, decomposes it into tasks, discovers
which agents and models are available, and delegates each piece of work to
whichever one is best suited — then observes, retries, reroutes, and accounts
for every token it spent doing so.

It is not a wrapper around LLM APIs. It is a harness for complete agents.

```
                        ┌────────────────────┐
                        │        USER        │
                        └─────────┬──────────┘
                                  │  "implement OAuth and fix the tests"
                                  ▼
              ┌───────────────────────────────────────┐
              │          META AGENT HARNESS           │
              │  plan · route · execute · recover     │
              └───────────────┬───────────────────────┘
                              │
              ┌───────────────┼───────────────┐
             ACP             A2A             MCP
              │               │               │
              ▼               ▼               ▼
       Coding agents    Remote agents    Tools/resources
```

## What it does

```console
$ cuma explain "implement OAuth authentication and fix the tests"

Plan for: implement OAuth authentication and fix the tests

#  Type            Risk      Depends on  Task
------------------------------------------------------------------------
1  Inspection      ReadOnly  -           Inspect the repository
2  Research        ReadOnly  1           Research the OAuth2 code flow
3  Implementation  Medium    1,2         Implement it
4  Testing         Low       3           Write or update tests
5  Validation      Low       3,4         Run the project's tests
6  Review          ReadOnly  5           Review the changes

Routing for task 1:

Selected:
  Agent: claude-code
  Model: sonnet
Strategy: Balanced
Reasons:
  capability & quality     0.94  x0.30  =  0.282
  cost                     0.72  x0.20  =  0.144
  latency                  0.88  x0.10  =  0.088
  reliability              0.91  x0.30  =  0.273
  context fit              1.00  x0.10  =  0.100
  total                                    0.887
Alternatives:
  codex/gpt-x       0.821
  gemini/flash      0.714
Rejected:
  broken: circuit breaker open
```

Every routing decision explains itself. Weights are tunable, and an operator
cannot tune what they cannot see.

## Design commitments

**Never "use the most powerful model".** Routing filters on hard constraints,
then scores five weighted dimensions — capability, cost, latency, reliability,
context fit. An agent that cannot do the work is *filtered*, not merely
outscored: it does not become able by being cheap.

**Reuse the agent's own authentication.** ACP agents manage their own
credentials, so driving an already-logged-in Claude Code or Codex reuses your
subscription and CUMA never holds an API key. The best way to avoid leaking a
secret is not to have one.

**Retries are bounded and failures are never silent.** A rate limit backs off. A
crash reroutes immediately — retrying a dead process wastes an attempt. A
context overflow asks for a smaller plan rather than the same oversized prompt
somewhere else. Every decision is a value that becomes an event.

**A fallback costs one prompt, not a transcript replay.** When agent B takes over
from A, it receives a structured summary: what was done, what remains, what was
decided, what to avoid.

**An estimate is never presented as a measurement.** Agents differ in what they
report. Cost is `unknown` rather than `$0.00` when pricing is not known, and a
partially-priced total renders as a lower bound.

**Defaults deny.** Destructive operations off. Skill creation off. Sandboxing on.
Auto-install admits only what ships in the binary.

## Quick start

```bash
cargo build --release

mkdir -p .cuma && cat > .cuma/config.toml <<'EOF'
[router]
strategy = "balanced"

[agents.codex]
protocol = "acp"

[agents.claude-code]
protocol = "acp"
EOF

cuma doctor                    # check the installation
cuma explain "add a health endpoint"
cuma run "add a health endpoint"
cuma usage
```

Well-known agents (`codex`, `claude-code`) resolve to their published ACP
adapters automatically. Any command that speaks ACP over stdio works.

## Commands

| | |
|---|---|
| `cuma run "<goal>"` | Plan, route and execute |
| `cuma explain "<goal>"` | Plan and route without executing |
| `cuma agents list \| show \| discover` | Agents and their health |
| `cuma models list` | Models and pricing |
| `cuma skills search \| inspect \| install` | Skills |
| `cuma memory status \| search` | Long-term memory |
| `cuma usage [--by-model]` | Tokens, cost, outcomes |
| `cuma serve --protocol acp` | Be an agent an editor can select |
| `cuma serve --protocol a2a` | Be an agent other systems can delegate to |
| `cuma chat` | Interactive TUI |
| `cuma doctor` | Check the installation |

Every command takes `--json`, for CI and for other agents.

## Documentation

| | |
|---|---|
| [Target architecture](TARGET_ARCHITECTURE.md) | What the system is |
| [Architecture](docs/ARCHITECTURE.md) | How the pieces fit |
| [Protocols](docs/PROTOCOLS.md) | ACP, A2A, MCP |
| [Routing](docs/ROUTING.md) | How an agent is chosen |
| [Orchestration](docs/ORCHESTRATION.md) | Planning, execution, recovery |
| [Memory](docs/MEMORY.md) | Shared long-term memory |
| [Skills](docs/SKILLS.md) | Discovery, validation, installation |
| [Security](docs/SECURITY.md) | Threat model and posture |
| [Observability](docs/OBSERVABILITY.md) | Events, logs, usage |
| [Configuration](docs/CONFIGURATION.md) | Every setting |
| [Development](docs/DEVELOPMENT.md) | Working on CUMA |
| [Roadmap](docs/ROADMAP.md) | What is built and what is not |
| [ADRs](docs/adr/) | Twelve decisions, with their costs |

On the transformation from the previous product:
[current architecture](CURRENT_ARCHITECTURE.md) ·
[migration plan](MIGRATION_PLAN.md) ·
[dependency analysis](DEPENDENCY_ANALYSIS.md) ·
[implementation plan](IMPLEMENTATION_PLAN.md)

## Both directions

CUMA consumes ACP and A2A agents. It is also one:

```
JetBrains / Zed ──ACP──> CUMA ──┬──ACP──> Codex
another system  ──A2A──>        ├──ACP──> Claude Code
                                └──A2A──> a remote reviewer
```

From the editor's side there is a single agent. Behind it is the whole routing
apparatus.

## Status

564 tests, zero warnings. Verified against a live ACP agent, and against the
real ACP client SDK driving CUMA as an agent.

Known gaps are listed in the [roadmap](docs/ROADMAP.md) — it distinguishes what
is done from what is not, rather than implying. The main ones: skill signatures
are checked for presence but not cryptographically verified, A2A is synchronous
(no streaming or task lifecycle), and write prediction over-serializes tasks
whose description names no paths.

## History

CUMA was a Go IoT network scanner. It is preserved under
[`legacy/`](legacy/) and still builds. The two products share a repository, a
name and an author, and nothing else — see
[`CURRENT_ARCHITECTURE.md`](CURRENT_ARCHITECTURE.md) for the component-by-component
account of what transferred (patterns, not code).

## Licence

Apache-2.0
