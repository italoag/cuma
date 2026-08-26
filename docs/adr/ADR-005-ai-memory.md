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

`ai-memory` is a Rust workspace built for exactly this, and it is on crates.io.
The obvious move is to add it as a dependency.

## Decision

Integrate it as an **external process**, over its CLI or MCP interface, behind
the `MemoryStore` port.

## Rationale

The MSRV is the smaller reason: 0.10.0 requires Rust 1.96, above this
workspace's floor, and it pulls `candle-core`, `candle-nn`,
`candle-transformers` and `hf-hub` — a full machine-learning stack — into every
CUMA build.

**The architectural reason is decisive, and would hold even if both problems
vanished.** Memory is only useful if it is *shared*. The point is that a Codex
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

## Consequences

**Good.** Memory is genuinely shared. CUMA's build stays light. Both projects
release independently.

**Costs.** A process spawn per operation, capped at a 10-second timeout because
recall sits on the planning critical path. The CLI's output format is not a
stable contract, so parsing is deliberately permissive — JSON array, wrapped
object, newline-delimited JSON, or plain lines — which is the difference between
"recall works across versions" and "recall silently returns nothing after an
upgrade".
