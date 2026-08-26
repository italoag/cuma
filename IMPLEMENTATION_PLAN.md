# Implementation plan

What is built, how it is verified, and what comes next.

## Current state

380 tests passing across 17 crates. Zero build warnings. Verified against a
live ACP agent.

```
$ cargo test --workspace
PASSING: 380 | FAILING: 0
```

## Verification, by concern

Each row is a claim the architecture makes and the test that holds it to it.

| Claim | Held by |
|---|---|
| A cheap task routes to a cheap capable agent | `router::cost_first_routing_prefers_the_cheaper_agent_for_real_work` |
| A complex task routes to a stronger agent | `router::a_complex_task_goes_to_the_stronger_agent_under_quality_first` |
| An agent lacking a capability is never selected, however cheap | `router::an_agent_lacking_a_required_capability_is_never_selected` |
| A rate-limited agent falls back | `orchestrator::a_failing_agent_falls_back_to_another_and_the_session_still_succeeds` |
| An unhealthy agent is excluded | `router::an_open_circuit_breaker_removes_an_agent_from_the_pool` |
| A cost limit is respected | `router::a_candidate_that_would_blow_the_budget_is_filtered` |
| Every failure sequence terminates | `resilience::every_failure_sequence_terminates` |
| Cancellation never trips a breaker | `resilience::cancellations_never_trip_a_breaker` |
| One bad model does not disable its agent | `resilience::one_bad_model_does_not_disable_its_whole_agent` |
| A manifest cannot claim trust it has not earned | `skills::a_manifest_cannot_talk_itself_up` |
| An unpriced attempt is never counted as free | `usage::an_unpriced_attempt_is_never_counted_as_free` |
| History overturns a static preference | `router::observed_history_can_overturn_a_static_preference` |
| Routing history survives a restart | `persistence::routing_history_survives_a_restart` |
| Every decision is explainable after the fact | `orchestrator::every_routing_decision_is_explainable_after_the_fact` |

### The two "done" scenarios

Both from the product definition, both tested end to end:

**Happy path** —
`a_goal_runs_end_to_end_through_plan_route_execute_and_record`: a goal is
planned, decomposed, routed, executed, validated, accounted for, and every stage
announced on the event bus.

**Recovery** —
`a_failing_agent_falls_back_to_another_and_the_session_still_succeeds`: an agent
crashes, the failure is classified, the breaker trips, the router selects a
different agent, context is handed over, and the session completes. No manual
intervention.

## Mock agents

`cuma-testkit` reproduces every failure mode the resilience layer claims to
handle, deterministically and without spending a token: success, slow response,
timeout, rate limit, quota exhaustion, partial stream, crash, invalid response,
auth failure, context overflow, and a task the agent honestly reports as failed.

Behaviour can vary per attempt, which is what makes retry and fallback testable
rather than merely assertable.

## Bugs found by the tests

Recorded because they are the argument for having written them:

1. **A same-target retry excluded the agent it was retrying.** The router was
   given every *failed* target rather than only the ones resilience had
   *abandoned*. With one agent configured, every rate limit became an immediate
   session failure. Fixed by distinguishing the two.

2. **Context assembly received an empty graph**, so dependency outputs never
   reached the executing agent — the context manager was doing nothing.

3. **Untrusted Agent Card tags flowed into capability names unsanitized**,
   including path separators. Fixed by sanitizing at the trust boundary.

4. **A separator-only line in an LLM plan parsed into a task** whose description
   was a stray pipe character.

## Next, in order

Ordered by what unblocks the most.

### 1. TUI event loop — Milestone 10

The state machine (`cuma_tui::AppState`) and the view functions are written and
tested without a terminal. What remains is the crossterm loop: subscribe, fold,
draw, handle keys.

*Why first:* it is the only unfinished item whose design is already settled.

### 2. CUMA as an ACP server

The primary architectural goal. An editor selects one agent; behind it, the
whole routing apparatus.

```
JetBrains ──ACP──> CUMA ──┬──ACP──> Codex
                          ├──ACP──> Claude Code
                          └──A2A──> remote architect
```

The client half is done and the SDK supports the agent role. This is a new
`cuma-server-acp` crate that implements `Agent` and forwards to the orchestrator.

### 3. Safe parallel execution

`TaskGraph::ready_tasks` already computes the parallel frontier correctly; the
orchestrator runs it sequentially because concurrent writers to one workspace
corrupt each other. The missing piece is isolation: git worktrees per task, file
ownership tracking, and merge coordination.

*Not a scheduling problem. A safety problem.*

### 4. RTK integration

Detect on `PATH`, wrap shell-heavy tool calls, record tokens saved. The config
surface and the usage counter already exist.

### 5. Provider adapters

A concrete `LlmProvider` so `LlmPlanner` has something to call. Behind the port,
in a `cuma-providers` crate — never scattered through the domain.

### 6. Sandbox enforcement

`security.sandbox` is configured and unenforced. Wire `sandbox_command` into
agent and skill execution.

### 7. Skill creation

Generating a skill that does not exist, testing it, validating it, registering
it. The highest-risk feature in the brief, deliberately last, and gated behind
`skills.allow_creation = false` by default.

## Standing constraints

These hold for everything above.

- Nothing protocol-shaped enters `cuma-core`.
- No `unwrap()` or `expect()` in production paths — enforced by
  `clippy::unwrap_used = "deny"` at the workspace level.
- Every new failure mode gets a mock agent before it gets a handler.
- Every routing change gets a test that pins the *behaviour*, not the score.
- An estimate is never rendered as a measurement.
- Defaults deny.

## Commands

```bash
cargo test --workspace          # everything
cargo clippy --workspace        # lints, including the deny list
cargo build --release           # the cuma binary

cuma doctor                     # check the installation
cuma explain "<goal>"           # plan and route without executing
cuma run "<goal>"               # execute
cuma usage                      # tokens, cost, outcomes
```
