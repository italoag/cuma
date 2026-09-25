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

## Spans

Every invocation opens a root span, and the work it does nests beneath it:

```
cuma{command=run}
└── session{session=…}
    └── task{task=…}
        └── attempt{task=… agent=codex model=gpt-…}
```

Log lines carry the fields of the spans they were emitted in, so a line from an
adapter says which session, task and agent it belongs to.

## OpenTelemetry

Traces can be exported over OTLP/HTTP to any collector (Jaeger, Tempo,
Honeycomb, an OpenTelemetry Collector). The exporter is a build-time option,
because most installs never need it:

```bash
cargo install --path crates/cuma-cli --features otel
```

```toml
[telemetry]
otlp_endpoint = "http://localhost:4318/v1/traces"
```

The standard `OTEL_EXPORTER_OTLP_*` variables apply too. Spans are batched and
flushed when the process exits. A configured endpoint in a build without the
feature logs a warning instead of silently exporting nothing.

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

**Tokens** are marked reported only when the agent reported every figure;
anything partly estimated — ACP context size with estimated output, A2A's text
estimates — is marked estimated. **Cost** reported by the agent itself (an ACP
`UsageUpdate` in USD) is preferred over a price-table estimate, and recorded
as reported.

**RTK's own measurements** — what it actually filtered, including commands
agents ran through their own RTK hooks — appear beside CUMA's figures:

```
RTK (measured): 214 commands filtered, 1.2M tokens saved (81% on average)
```

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
routing history, and agent health (consecutive failures, last error, last
success).

Everything is written as it happens, through the orchestrator's
`SessionRecorder`, so sessions started from an editor over ACP, a peer over
A2A, a host over MCP or the TUI are recorded exactly like `cuma run`. Routing
history reloads at startup, so a fresh process routes with everything earlier
sessions learned — including those.

Installed skills are recorded beside their files, in `.cuma/skills/installed.json`.

## Benchmarks

```bash
cargo bench -p cuma-orchestrator --bench harness    # routing, event bus, DAG, context, write prediction
cargo bench -p cuma-skills --bench integrity        # digest and signature verification
cargo bench -p cuma-persistence --bench store       # per-attempt database writes
```

Current figures are in the [roadmap](ROADMAP.md#measured-overhead).

## Not yet built

Metrics and logs over OTLP; only traces are exported.
