# ADR-005 — `ai-memory` as an external process, not a linked crate

**Status:** Accepted

## Context

Long-term memory is what makes multi-session, multi-agent work coherent:

```
Session 1   Codex   implements a feature
Session 2   Claude  continues it
Session 3   Gemini  investigates a bug in it
```

All three need the same architectural decisions, conventions and findings.

`akitaonrails/ai-memory` is a Rust workspace built for exactly this. The
obvious move is to add it as a dependency.

## Decision

Integrate it as an **external process**, over its CLI or MCP interface, behind
the `MemoryStore` port.

## Rationale

*Correction.* An earlier version of this record gave a second reason: that
`ai-memory` 0.10.0 on crates.io needs Rust 1.96 and a `candle` ML stack. That
crate is a different project of the same name (AlphaOne LLC's
`ai-memory-mcp`), not the one the brief names. It is not a reason about
Akita's `ai-memory` at all. See `DEPENDENCY_ANALYSIS.md`.

**The architectural reason is decisive on its own.** Memory is only useful if it is *shared*. The point is that a Codex
session, a Claude session and a CUMA session all see the same knowledge. That
cannot work if the memory lives inside one of them. `ai-memory` exposes an MCP
server and a CLI precisely so different agent tools can share one store — using
it as a private library would defeat its purpose.

Linking it in would have been the technically inferior choice regardless.

## Ownership of data

Stated explicitly, because two stores that both hold "state" will drift:

| Data | Owner |
|---|---|
| Project knowledge, architectural decisions, conventions, findings | **ai-memory** |
| Sessions, tasks, attempts | CUMA runtime database |
| Usage, cost, latency | CUMA runtime database |
| Routing decisions and history | CUMA runtime database |
| Agent health | CUMA runtime database |

Rule of thumb: if another agent would want it, it belongs in memory. If it is
about how *this harness* behaved, it belongs in the runtime database.

## Memory is always optional

Every operation degrades rather than fails. A missing binary, a crashed backend
or malformed output costs recall, never the session. `NullMemory` is the
default, and running without recall is a supported configuration.

`remember` on an unavailable backend returns `"not-stored:backend-unavailable"`
rather than claiming success or raising an error — the caller deserves to know
which happened.

## Interfaces used

Read from the project's source, not assumed:

| CUMA | CLI backend (`ai-memory-cli`) | MCP backend (`ai-memory-mcp`) |
|---|---|---|
| recall | `ai-memory search <q> -n N --json` → `[{path, title, snippet, rank}]` | `memory_query {query, limit}` |
| remember | `ai-memory write-page --path P --body - --kind K -t cuma` (body on stdin) | `memory_write_page {path, body, tags}` |
| record a handoff | written as a page — the CLI cannot begin one | `memory_handoff_begin {summary, open_questions, next_steps, files_touched, cwd}` |

The MCP backend is reached through the `ToolProvider` port, so `cuma-memory`
itself depends on no MCP SDK. Its handoffs are ai-memory's own: typed, owned,
and claimed exactly once — the same contract as `AgentHandoff`.

## Consequences

**Good.** Memory is genuinely shared. CUMA's build stays light. Both projects
release independently.

**Costs.** A process spawn per operation, capped at a 10-second timeout because
recall sits on the planning critical path. The CLI's output format is not a
stable contract, so parsing is deliberately permissive — JSON array, wrapped
object, newline-delimited JSON, or plain lines — which is the difference between
"recall works across versions" and "recall silently returns nothing after an
upgrade".
