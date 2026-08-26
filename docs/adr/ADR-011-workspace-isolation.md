# ADR-011 — File ownership, not dependency order, gates parallelism

**Status:** Accepted

*Supersedes the parallelism half of [ADR-009](ADR-009-agent-isolation.md), which
deferred concurrency pending this mechanism.*

## Context

`TaskGraph::ready_tasks` returns every task whose dependencies have completed.
The obvious reading is that those tasks can run concurrently.

They cannot. **Dependency independence is not workspace independence.** Two
tasks with no edge between them can both edit `src/auth.rs`, and whichever
writes second silently discards the other's work — with no error, no conflict
marker, and nothing in the transcript to explain why half the change vanished.

## Decision

An `OwnershipLedger` gates the ready set. Before a writing task runs, it claims
the paths it will write; a task whose claim conflicts is deferred to a later
wave rather than failed.

Three properties make this work:

**Claims are on path prefixes.** A task owning `src/auth/` conflicts with one
owning `src/auth/token.rs`, in both directions. Exact-path matching would miss
exactly the case that motivates the mechanism.

**Prediction is pessimistic.** Write paths are guessed from a task's
description. When nothing path-shaped can be found, the task claims the whole
workspace and serializes against everything. Guessing narrowly would let two
tasks run concurrently and corrupt each other — the failure this exists to
prevent. A false serialization costs latency; a false parallelization costs the
user's work.

**Read-only tasks never contend.** Inspection, research and review cannot
corrupt anything, so they run together regardless of what they mention.

## Why not a worktree per task

Git worktrees are implemented (`cuma_workspace::git`) and give stronger
isolation: two tasks writing concurrently write to different directories
entirely.

They are not the default because they change what the agent sees. A task in a
worktree cannot observe a sibling's completed work without a merge, and merges
introduce conflicts that need a human. Ownership is sufficient for correctness
and keeps every task looking at one coherent tree.

Worktrees remain available for the case ownership handles worst: several
long-running tasks that all claim the workspace root.

## Consequences

**Good.** Independent tasks genuinely overlap — tested by asserting on an
observed concurrency high-water mark, which is what distinguishes real overlap
from a fast sequence. Conflicting tasks are serialized rather than corrupted,
and still all complete. `max_parallel_tasks` is a real bound.

**Costs.** Write prediction is a heuristic over prose, so it over-serializes: a
task described as "implement OAuth" claims everything. Better prediction —
asking the planner to name paths, or reading a dry run — is the main lever on
achievable parallelism.

Each task in a wave executes against a clone of the graph, and only its own row
is folded back. That is cheap relative to an agent call and avoids a lock the
whole wave contends on, but it does mean re-planning is honoured only when its
task ran alone.
