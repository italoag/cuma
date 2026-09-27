# Orchestration

## The loop

```
plan ──> for each ready wave ──> route ──> execute ──> classify
                                    ▲                     │
                                    └──── retry ──────────┤
                                    └──── reroute ────────┤
                                          replan ─────────┤
                                          give up ────────┘
```

## Planning

`HeuristicPlanner` is the default: rule-based, deterministic, no model call. A
planner that needs a network round trip before the harness can do anything is a
planner that fails when the network does.

It recognizes the *shape* of a request — does it change code, does it need
research, does it mention tests — and emits the standard pipeline for that
shape. Matching covers English and Portuguese.

```
"implement OAuth and fix the tests"
   1  Inspection      inspect the repository
   2  Research        OAuth2 authorization code flow      (depends on 1)
   3  Implementation  implement it                        (depends on 1, 2)
   4  Testing         write or update tests               (depends on 3)
   5  Validation      run the suite                       (depends on 3, 4)
   6  Review          review the changes                  (depends on 5)
```

A pure investigation — *"why is the build slow?"* — produces a read-only plan.
Inventing an implementation task the user did not ask for is scope creep.

`LlmPlanner` handles goals the heuristics do not recognize, treating model output
as untrusted data: malformed lines are skipped, forward and self references are
dropped so the graph is acyclic by construction, and it **always** falls back to
heuristics rather than failing the session.

## The DAG

`TaskGraph` owns dependency semantics so the orchestrator never reasons about
edges:

- `validate()` rejects dangling dependencies and cycles at submission, not at
  deadlock
- `ready_tasks()` returns the parallel frontier, priority-ordered
- `cascade_skip()` marks transitive dependents skipped and *reports which*, so a
  user is not left working out why half the plan never ran

## Context assembly

The goal is maximum useful information per token sent. A task receives:

1. Its handoff, if this is a fallback — highest value, so first
2. Recalled memories, if any
3. Its own description
4. **Completed dependencies' outputs**, truncated
5. Its own previous failures
6. What long-term memory knows about *this task* — up to three memories,
   recalled once, labelled as background notes rather than instructions
7. The instructions of enabled skills providing the capabilities it needs,
   labelled by trust (see [SKILLS.md](SKILLS.md))

Not: the session transcript, the whole plan, or sibling task outputs. A sibling
running in parallel has nothing this task needs.

Prompts are trimmed to the model's window (60% of it, leaving the agent room for
its own tool output). An over-budget prompt is a guaranteed `ContextOverflow`; a
trimmed one usually still works.

## Failure handling

Classification drives everything:

| Class | Reaction |
|---|---|
| `RateLimit` | Back off with jitter, retry the **same** agent |
| `Timeout` | Bounded retry, then reroute |
| `ConnectionFailure` | Bounded retry, then reroute |
| `AgentCrash` | Reroute **immediately** — retrying a dead process wastes an attempt |
| `QuotaExceeded` | Reroute; retrying the same target is pointless |
| `ModelUnavailable` | Reroute to another model |
| `ContextOverflow` | **Replan** — a different agent with the same oversized prompt fails identically |
| `AuthenticationFailure` | Give up. A rejected credential does not become valid |
| `SecurityViolation` | Give up. Never retried, never downgraded |
| `Cancelled` | Give up. Not a failure |

Three invariants, each backed by tests:

- **Retries are bounded.** `every_failure_sequence_terminates` walks every class
  and asserts termination.
- **Failures are never silent.** Every decision becomes an event.
- **Reaction follows classification**, never an error's text.

### Retry versus reroute

A distinction that matters. Excluding every *failed* target from routing would
turn every retry into a reroute — and with one agent configured, into an
immediate session failure. The orchestrator tracks targets the resilience layer
has **abandoned**, which is a smaller set.

## Circuit breakers

Keyed per agent *and* per agent+model.

```
Closed ──(N consecutive failures)──> Open
  ▲                                    │
  │                              (cooldown elapses)
  │                                    ▼
  └──(probe succeeds)───────────── HalfOpen ──(probe fails)──> Open
```

- A `ModelUnavailable` failure trips only that model's breaker — one overloaded
  model must not disqualify its siblings.
- A crash trips the agent's, taking every model with it.
- Cancellation and local configuration errors never trip anything. A user
  pressing Ctrl-C three times must not take a healthy agent out of rotation.

## Handoff

When agent B takes over from A, it does not replay A's transcript:

```markdown
## Handoff
Task: implement OAuth
Previous agent: codex (handed over because: rate limited)

### Already done
- wrote the token endpoint
### Still to do
- wire up refresh
### Warnings
- codex failed: 429 Too Many Requests
### Files changed
- src/auth.rs
```

Empty sections are omitted. The whole point is spending as few tokens as
possible.

Two events make a fallback visible: `FallbackSelected { from, to }` names the
agent that actually takes over — the routing probe that decided a reroute was
possible already chose it — and `HandoffPerformed` is published on the pass
where the new agent receives the handoff. The handoff is also kept in
long-term memory (see [MEMORY.md](MEMORY.md)).

## Recording

Every step is written to the runtime database as it happens — session start,
each routing decision with its explanation, each attempt, each change in an
agent's health, the final state of every task — through the `SessionRecorder`
port. Front ends differ; the record does not.

## Deadlines

Enforced by the orchestrator with `tokio::time::timeout`, not delegated to
adapters. An adapter that ignores its timeout must not be able to hang a session.

## Parallelism

`ready_tasks()` computes the dependency frontier; an `OwnershipLedger` decides
which of it may actually run together.

The distinction that matters: **dependency independence is not workspace
independence.** Two tasks with no edge between them can both edit
`src/auth.rs`, and whichever writes second silently discards the other's work.

- Writing tasks claim the paths they will write, on **prefixes** — a task
  owning `src/auth/` conflicts with one owning `src/auth/token.rs`.
- A conflicting task is **deferred**, not failed: it runs in a later wave.
- Prediction is **grounded and pessimistic**. Paths named in a description are
  resolved against an index of the workspace's files — so "fix auth.rs" and
  "edit src/auth.rs" claim the same file — and files a task's dependencies
  changed are claimed too. A description naming no paths still claims the
  whole workspace. A false serialization costs latency; a false
  parallelization costs the user's work.
- Read-only tasks never contend.
- `max_parallel_tasks` bounds the wave.

Claims are released when a task reaches a terminal state, successful or not,
and also when a run is aborted mid-wave — a cancelled ACP prompt or A2A task —
because the release happens on drop. A task that kept its claims would lock
those paths for every later session.

**Worktree isolation** (`limits.isolation = "worktree"`) adds a second layer
for when prediction is wrong: each writing task runs in a detached worktree
built from a snapshot of the live tree, and its work is applied back as
uncommitted changes. Work that no longer applies fails the task and is kept in
its worktree for a manual merge. Nothing is committed on the user's behalf.

See [ADR-011](adr/ADR-011-workspace-isolation.md) and
[ADR-014](adr/ADR-014-worktree-isolation.md).

## Workspace safety

Before anything writes, CUMA detects the repository and — under
`security.checkpoint_before_write` — saves the working tree, untracked files
included, as a commit written through a temporary index and anchored under
`refs/cuma/checkpoints/`. Nothing the user sees moves: not the tree, not the
index, not a branch. A checkpoint that reverted the tree would change the task
the agent was given. `git restore --source=<commit> --worktree --overlay -- .`
brings the saved files back without deleting anything. CUMA's own state
(`.cuma/runtime.db*`, `acp-sessions/`, `cache/`) is never checkpointed, and a
`.cuma/.gitignore` keeps it out of `git status`.

Agents run their own shell commands, so what confines them is the sandbox
they are launched in (ai-jail, bubblewrap, `sandbox-exec` or firejail — see
[SECURITY.md](SECURITY.md#agents-themselves))
and RTK hooks installed into the agents themselves (`rtk init`). Commands CUMA
prepares itself go through one pipeline:

```
screen (refuse destructive) ──> RTK (filter output) ──> sandbox (confine)
```

Screening first means a refused command is never wrapped or spawned; RTK
before the sandbox means the sandbox confines the whole pipeline rather than
RTK escaping it. RTK is used only once `rtk gain` proves the binary is RTK and
not the unrelated crate of the same name.
