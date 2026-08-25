# ADR-007 — SQLite for runtime state

**Status:** Accepted

## Context

Routing history is worth little if it evaporates on exit. `cuma usage` needs to
report across sessions. Explaining why an agent was chosen three days ago
requires having kept the explanation.

## Decision

SQLite via `rusqlite`, with the `bundled` feature so there is no system SQLite
requirement and the build stays hermetic.

One connection behind a mutex, not a pool: writes are one row per attempt and
SQLite serializes them anyway. A pool would add contention management for a
workload that has none.

WAL journaling, because the TUI reads while the orchestrator writes.

## What it stores

Sessions, tasks, attempts, routing decisions, aggregated routing history, agent
health, installed skills.

**Not project knowledge.** That is `ai-memory`'s, per ADR-005. Two stores both
holding "state" will drift; the boundary is drawn explicitly.

## Two decisions worth naming

**An attempt and its history bucket are written in one transaction.** History
that disagrees with the attempts it summarizes would make `cuma usage` and the
router contradict each other.

**A history bucket of an unrecognized task type is skipped, not guessed at.** A
newer version may write a task type this build has never heard of. Skipping
loses one bucket; guessing puts the wrong evidence behind a routing decision.

**Foreign keys cascade.** Orphaned attempts would skew every later report.

## Consequences

**Good.** No server, no daemon, one file. Aggregation happens in SQL, so a
long-lived database does not make `cuma usage` slow. Restart-safe by
construction.

**Costs.** Single-writer, so a future multi-process CUMA would need a different
answer. The schema is versioned via `user_version` and migrations are additive;
`SCHEMA_VERSION` is currently 1.
