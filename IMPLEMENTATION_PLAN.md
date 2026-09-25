# Implementation plan

What is built, how it is verified, and what comes next.

## Current state

746 tests passing across 20 crates. Clippy clean with warnings denied;
checked on the MSRV (1.88) with and without the `otel` feature. Verified
against a live ACP agent.

```
$ cargo test --workspace
PASSING: 746 | FAILING: 0
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
| A fallback names the agent that takes over, and the handoff is kept | `end_to_end::a_failing_agent_falls_back_…`, `a_handoff_is_kept_in_long_term_memory_with_its_receiver` |
| Every front end's sessions are recorded | `recorder::every_step_of_a_session_reaches_the_database` |
| Concurrent ACP sessions see only their own work | `acp_round_trip::concurrent_sessions_see_only_their_own_work` |
| An ACP prompt can be cancelled, and a session reloaded after a restart | `a_client_can_cancel_a_running_prompt`, `a_persisted_session_can_be_loaded_by_a_new_process` |
| An A2A peer asking for input is not a success | `a2a_lifecycle::a_peer_asking_for_input_is_a_failure_not_a_success` |
| A 0.3-only A2A peer is still reached | `a2a_lifecycle::a_peer_that_only_speaks_0_3_is_reached_by_falling_back` |
| An abandoned remote task is cancelled | `a2a_lifecycle::an_abandoned_remote_task_is_cancelled` |
| Claimed checksums and signatures earn nothing | `validation::claimed_checksums_and_signatures_earn_nothing` |
| A tampered signed skill is not installed, and a failing update keeps the old one | `manager::a_skill_whose_signature_does_not_match_is_not_installed`, `an_update_that_fails_verification_keeps_the_installed_version` |
| Two spellings of one file conflict | `ownership::two_spellings_of_one_file_now_conflict` |
| Isolated work lands uncommitted, and work that no longer applies is kept | `worktree_isolation::…`, `git::changes_that_no_longer_apply_are_refused_and_nothing_lands` |
| An MCP allowlist holds through the proxy | `tool_server::a_refused_tool_is_a_protocol_error`, plus the proxy smoke test below |
| A tool called `rtk` that is not RTK is never used | `rtk::a_different_tool_called_rtk_is_not_used` |
| Each ACP session works in the directory its client named | `acp_round_trip::each_session_works_in_the_directory_its_client_named` |
| An untrusted project's configuration is never applied | `harness::an_untrusted_workspace_is_served_with_cumas_own_configuration` |
| A2A tasks outlive a restart, and an interrupted one is reported, not re-run | `a2a_lifecycle::tasks_outlive_a_restart_and_an_interrupted_one_is_reported_not_rerun` |
| Credentials in `$HOME` are never mounted for an agent | `confine::credentials_in_home_are_never_bound_and_agent_state_is_writable` |
| Environment is reduced by name; values never reach a command line | `confine::only_the_baseline_and_named_variables_are_kept` |
| A worktree's repository stays writable to git inside the sandbox | `confine::a_worktree_gets_its_repository_git_directory_writable` |
| New, untracked work is checkpointed, and CUMA's own state is not | `git::new_files_nobody_has_added_yet_are_checkpointed_too`, `cumas_own_state_is_never_checkpointed` |
| An authenticated A2A server refuses callers without its token | `a2a_lifecycle::an_authenticated_server_refuses_callers_without_its_token` |
| One MCP server process answers every call; a dead one is replaced, its call not repeated | `provider::one_server_process_answers_every_call`, `a_server_that_died_is_replaced_and_the_failed_call_is_not_repeated` |

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

Found while completing the protocols and persistence:

5. **The ACP server ran each prompt inside the connection's dispatch loop**,
   blocking every other message — so `session/cancel` could never cancel, and a
   second session waited for the first.
6. **ACP and A2A servers forwarded every session's events to every client.**
   Fixed by choosing the session id before a run starts and filtering on it.
7. **Every attempt insert failed under foreign keys**: attempts referenced task
   rows written only at the end of a session. And `INSERT OR REPLACE` on
   sessions and tasks deleted and reinserted rows, cascading away their
   children.
8. **A failure the breaker still rated healthy was recorded as a success.**
9. **Skills were validated by the fields their manifest claimed**, and
   installed into a manager that lived as long as one command.
10. **The ai-memory adapter called a CLI command that does not exist** (`add`)
    and parsed none of the fields `search` actually returns.
11. **Aborting a run leaked ownership claims** — now released on drop.
12. **Output printed while a session ran was dropped** by aborting the printer
    before the bus drained.

The manual checks that complement the tests — an MCP proxy driven by a raw
JSON-RPC client, the skills signing loop from `keygen` to a refused tampered
update, the TUI driven in a pseudo-terminal, OTLP spans received by a local
listener, the ACP registry read from its cache — are the kind of thing a unit
test mocks away.

## Next

Every item this section used to list — the TUI loop, CUMA as an ACP server,
safe parallel execution, RTK, providers, sandbox enforcement, skill creation —
is built. What remains is in the [roadmap](docs/ROADMAP.md#what-is-not-built).

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
