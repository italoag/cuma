# ADR-009 — A process per execution; parallelism gated on isolation

**Status:** Accepted

## Context

Two questions that look separate and are not:

1. Should an ACP agent process be reused across tasks?
2. Should independent tasks run concurrently?

Both are about what happens when agents share state they should not.

## Decision on process lifetime

**One process per execution.** An ACP agent is spawned, initialized, given a
session, prompted, and shut down.

That costs a spawn per task and buys three things worth more:

- A crashed agent cannot poison later tasks.
- `Send`-ness stays simple; no long-lived non-`Send` connection to thread
  through the orchestrator.
- There is no child left running when a session is abandoned.

Session reuse is a real optimization, and ACP supports `session/load`. It is
deferred rather than rejected: the cost is a subprocess spawn, which is small
next to an agent turn.

## Decision on parallelism

`TaskGraph::ready_tasks` computes the parallel frontier correctly — every task
it returns is dependency-independent and could run concurrently.

**The orchestrator runs them sequentially anyway.**

Not because the scheduling is hard. Because concurrent writers to one workspace
corrupt each other, and dependency independence is not workspace independence.
Two tasks with no edge between them can both edit `src/auth.rs`.

Enabling parallelism requires isolation first:

- Git worktrees, one per concurrently executing task
- File ownership tracking, so overlapping writes are detected
- Merge coordination when worktrees rejoin
- Task-level locking for shared resources

`max_parallel_tasks` is honoured as a bound on the frontier, so the
configuration surface is right and only the safety mechanism is missing.

## Deadlines are enforced by the orchestrator

Not by the adapter. An adapter that ignores its timeout must not be able to hang
the session, so `tokio::time::timeout` wraps every `execute` call regardless of
what the adapter does internally.

## Consequences

**Good.** No cross-task contamination. No leaked processes. No corrupted
workspace. A hung agent cannot hang a session.

**Costs.** A subprocess spawn per task. Sequential execution where the DAG says
parallel is safe — the single largest performance gap in the current
implementation, and the one with the clearest path forward.
