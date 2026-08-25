# Observability

## Correlation

Every event carries the ids needed to join it back to its context, so a log line
is queryable without parsing prose:

```
session_id ──> task_id ──> attempt_id
                       └──> agent_id, model_id
```

```
session
└── task
    ├── planning
    ├── routing        ← the full scoring breakdown, kept
    ├── execution
    ├── tool calls
    └── validation
```

## Events

The complete vocabulary is `cuma_core::EventKind`:

| Group | Events |
|---|---|
| Session | `SessionStarted`, `SessionCompleted` |
| Task | `TaskPlanned`, `TaskCreated`, `TaskStatusChanged`, `TaskCompleted`, `TaskFailed`, `TaskSkipped` |
| Routing | `AgentSelected`, `RoutingFailed` |
| Execution | `AgentStarted`, `AgentOutputReceived`, `AgentFailed` |
| Resilience | `RetryScheduled`, `FallbackSelected`, `CircuitBreakerChanged`, `HandoffPerformed` |
| Skills | `SkillInstalled`, `SkillRejected` |
| Usage | `UsageRecorded` |

`AgentSelected` carries the *rendered explanation*, not just a score, so any
subscriber can display or persist the reasoning without asking the router to
recompute a decision already made.

The bus is deliberately lossy: a slow subscriber must never stall the
orchestrator. Subscribers that fall behind are *told* they lagged rather than
silently missing events.

## Logging

```bash
cuma run "..." -v          # debug
cuma run "..." -vv         # trace
cuma run "..." --json      # structured, for CI and other agents
RUST_LOG=cuma_router=trace cuma run "..."
```

**Logs go to stderr, structured output to stdout**, so `cuma usage --json | jq`
works with the log level turned up.

## Usage

```bash
cuma usage                # by agent
cuma usage --by-model     # by agent and model
cuma usage --json
```

```
Sessions: 12   Attempts: 47   Recorded spend: >=$3.4210

AGENT USAGE
Name         Tasks  Success  Tokens  Cost      Mean latency
------------------------------------------------------------
claude-code     31      94%    2.1M  ~$2.4100  8.2s
codex           16      88%    1.2M  ~$1.0110  6.1s
```

### Estimates are never rendered as measurements

| Rendering | Means |
|---|---|
| `~$2.4100` | Every attempt was priced. Still derived, hence `~`. |
| `≥$1.2000 (3 of 9 attempts unpriced)` | A lower bound. |
| `unknown` | Nothing was priced. |
| `-` | Nothing ran. |

The JSON form carries `cost_is_complete` and `attempts_without_pricing`, because
a consumer cannot otherwise tell a complete total from an incomplete one.

`Known<T>` distinguishes `Reported`, `Estimated` and `Unknown` throughout the
domain, and it propagates: a total built from one estimate is an estimate.

## Health

```bash
cuma doctor           # configuration layers, agents, database, memory, security
cuma agents list      # health per agent
cuma agents show <id> # capabilities, models, last error
```

## Persisted

Survives restarts, in `.cuma/runtime.db`:

sessions, tasks, attempts, routing decisions (with explanations), aggregated
routing history, agent health, installed skills.

Routing history reloads at startup, so a fresh process routes with everything
earlier sessions learned.

## Not yet built

OpenTelemetry export. `tracing` is the substrate, so adding an OTLP layer is a
subscriber change rather than an instrumentation change.
