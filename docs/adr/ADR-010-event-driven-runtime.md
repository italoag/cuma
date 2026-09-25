# ADR-010 — An event bus between the runtime and every interface

**Status:** Accepted

## Context

A TUI, a CLI, structured logs, persistence, usage tracking and — eventually — an
ACP server, an A2A server and a web front end all need to know what the
orchestrator is doing.

The obvious approach is for the orchestrator to call each of them. That is how a
runtime ends up unable to change without breaking its UI.

## Decision

The orchestrator publishes to a `tokio::sync::broadcast` bus. Everything else
subscribes.

`EventKind` covers the full vocabulary: session lifecycle, task lifecycle, agent
selection, streamed output, failures, retries, fallbacks, breaker transitions,
handoffs, skill decisions and usage. Every event carries correlatable ids —
session, task, attempt — so a log line joins back without parsing prose.

The orchestrator calls no interface directly.

## Two decisions worth naming

**The bus is deliberately lossy.** A slow subscriber — a TUI mid-redraw — must
never stall the orchestrator. Subscribers that fall behind receive
`RecvError::Lagged` and are *told* they lagged, rather than silently missing
events.

**`AgentSelected` carries the rendered explanation, not just a score.** The
reasoning travels with the event, so any subscriber can display or persist it
without asking the router to recompute a decision that has already been made.

## What this buys

The TUI holds no orchestration state. It subscribes, folds each event into a
view model, and draws. Which means the whole interface is testable by feeding it
an event sequence — no terminal, no orchestrator, no timing. `cuma-tui`'s tests
do exactly that.

It is also what makes exposing CUMA as an ACP or A2A server a matter of adding a
subscriber rather than restructuring the runtime.

## Consequences

**Good.** Interfaces are decoupled from the runtime and independently testable.
New consumers cost nothing. Publishing with no subscribers is not an error, so
the harness runs headless.

**Costs.** Every event is cloned per subscriber, so payloads are kept small —
streamed output is chunked rather than accumulated in the event. Buffer capacity
(1024) is a tuning parameter: too small and slow subscribers lag, too large and
a stalled subscriber holds memory.
