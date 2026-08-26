# ADR-012 — CUMA serves the same protocols it consumes

**Status:** Accepted

## Context

CUMA drives coding agents over ACP and peers over A2A. The design always
pointed at the mirror image: CUMA *being* one of those agents, so an editor or
another agentic system sees a single agent and CUMA routes behind it.

```text
JetBrains / Zed          another agentic system
      │                            │
     ACP                          A2A
      ▼                            ▼
              CUMA ──┬── ACP ──> Codex
                     ├── ACP ──> Claude Code
                     └── A2A ──> a remote reviewer
```

## Decision

Serve both, as adapters pointing the other way. `cuma-server-acp` implements
the ACP agent role over stdio; `cuma-protocol-a2a::server` serves an Agent Card
and JSON-RPC over HTTP.

Neither changes the orchestrator. That is the payoff of the port boundary: a
server is a subscriber to the event bus and a caller of `Orchestrator::run`,
which is the same surface the CLI and the TUI use.

## What the client sees, and what it does not

The user in an editor should see **one coherent agent**, not the internals of a
routing layer. So orchestrator events are translated selectively:

| Forwarded | Withheld |
|---|---|
| The plan | Retry counters |
| Which agent each task went to | Circuit-breaker transitions |
| Streamed agent output | Scoring tables and rejection lists |
| Failures and fallbacks | Usage records |

Routing *is* surfaced — it is CUMA's distinguishing behaviour and a user should
know their work went to Codex — but as one line, not as the full breakdown.

## Advertise only what is implemented

Both servers under-claim deliberately:

- ACP advertises `load_session: false`, because there is no resume path.
  Claiming one would break editors that use it.
- A2A advertises `streaming: false` and `pushNotifications: false`, and
  `tasks/get` reports that CUMA runs goals synchronously rather than returning
  an empty task a caller would poll forever.
- The A2A Agent Card's skills are derived from the capabilities CUMA's
  *registered agents* actually have. Advertising more would make a peer's
  routing decisions wrong, not just CUMA's.

## A failed session still ends the turn

An ACP session that fails returns `EndTurn`, not `Refusal`. The turn genuinely
ended and the failure is in the transcript; `Refusal` would tell the editor
CUMA declined to work at all, which is a different thing.

## Consequences

**Good.** The primary architectural goal is met, and proven by an integration
test in which the real ACP client SDK drives CUMA over an in-process pipe,
including a mid-session fallback the client sees as one continuous
conversation. Adding a further front end costs an adapter, not a redesign.

**Costs.** Two more surfaces to keep in step with their specifications. The ACP
server holds session state (working directory, a bounded turn history) that must
not drift into duplicating the orchestrator's. A2A is synchronous, which suits
delegation and rules out long-poll patterns until the task lifecycle is
implemented.
