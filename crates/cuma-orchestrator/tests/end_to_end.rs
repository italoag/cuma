//! End-to-end tests for the two scenarios the product definition calls
//! "done": a goal executed through plan → route → execute → record, and a
//! failure recovered through classify → reroute → handoff → continue, both
//! without manual intervention.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use cuma_config::{Config, LimitsConfig, RouterConfig, RoutingStrategy};
use cuma_core::{
    AgentDescriptor, AgentId, AgentProtocol, Capability, CapabilitySet, CostProfile, EventKind,
    HealthState, Known, ModelDescriptor, TaskStatus,
};
use cuma_orchestrator::Orchestrator;
use cuma_planner::HeuristicPlanner;
use cuma_testkit::{Behaviour, MockAgent};
use std::sync::Arc;

/// Every capability the heuristic planner can ask for, so tests exercise
/// routing and resilience rather than tripping the capability filter.
fn all_capabilities() -> CapabilitySet {
    [
        Capability::CodeComprehension,
        Capability::CodeGeneration,
        Capability::CodeEditing,
        Capability::Debugging,
        Capability::Refactoring,
        Capability::Testing,
        Capability::ShellExecution,
        Capability::FileSystem,
        Capability::VersionControl,
        Capability::Research,
        Capability::Documentation,
        Capability::Architecture,
        Capability::CodeReview,
        Capability::Planning,
        Capability::ToolUse,
    ]
    .into_iter()
    .collect()
}

fn descriptor(id: &str, input_price: f64, output_price: f64) -> AgentDescriptor {
    let mut agent =
        AgentDescriptor::new(id, id, AgentProtocol::Native).with_capabilities(all_capabilities());

    let mut model = ModelDescriptor::minimal(agent.id.clone(), format!("{id}-model"), id);
    model.context_window = Known::Reported(200_000);
    model.cost = CostProfile {
        input_per_mtok: Known::Reported(input_price),
        output_per_mtok: Known::Reported(output_price),
        cache_read_per_mtok: Known::Unknown,
    };
    agent.models.push(model);
    agent
}

fn config() -> Config {
    Config {
        router: RouterConfig {
            strategy: RoutingStrategy::Balanced,
            weights: RoutingStrategy::Balanced.default_weights(),
            ..RouterConfig::default()
        },
        limits: LimitsConfig {
            max_parallel_tasks: 4,
            max_retries: 3,
            task_timeout_secs: 5,
            ..LimitsConfig::default()
        },
        ..Config::default()
    }
}

async fn orchestrator_with(agents: Vec<MockAgent>) -> Orchestrator {
    let mut orchestrator = Orchestrator::new(
        config(),
        Arc::new(HeuristicPlanner::new()),
        std::env::temp_dir(),
    );

    for agent in agents {
        orchestrator
            .add_agent(Arc::new(agent))
            .await
            .expect("adding a mock agent should not fail");
    }

    orchestrator
}

/// Drain the event bus into a vector while a session runs.
fn collect_events(orchestrator: &Orchestrator) -> tokio::task::JoinHandle<Vec<cuma_core::Event>> {
    let mut rx = orchestrator.events().subscribe();
    tokio::spawn(async move {
        let mut events = Vec::new();
        while let Ok(event) = rx.recv().await {
            let done = matches!(event.kind, EventKind::SessionCompleted { .. });
            events.push(event);
            if done {
                break;
            }
        }
        events
    })
}

// ---------------------------------------------------------------------------
// Scenario 1: the happy path, end to end
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_goal_runs_end_to_end_through_plan_route_execute_and_record() {
    let agent = MockAgent::always("worker", Behaviour::ok("done"))
        .with_descriptor(descriptor("worker", 3.0, 15.0));

    let orchestrator = orchestrator_with(vec![agent]).await;
    let events = collect_events(&orchestrator);

    let result = orchestrator.run("add a health endpoint").await.unwrap();

    assert!(result.success, "summary was: {}", result.summary);
    assert!(result.completed_tasks() > 1, "the goal should decompose");
    assert_eq!(result.failed_tasks(), 0);
    assert!(result.graph.is_complete());

    // Usage was recorded for every task, with a cost, because pricing is known.
    assert_eq!(result.usage.attempts as usize, result.completed_tasks());
    assert!(result.usage.total_tokens() > 0);
    assert!(result.spent_usd > 0.0);
    assert_eq!(result.usage.attempts_without_pricing, 0);

    // And the whole pipeline announced itself on the bus.
    let events = events.await.unwrap();
    let kinds: Vec<&str> = events
        .iter()
        .map(|e| match &e.kind {
            EventKind::SessionStarted { .. } => "session_started",
            EventKind::TaskPlanned { .. } => "task_planned",
            EventKind::AgentSelected { .. } => "agent_selected",
            EventKind::AgentStarted { .. } => "agent_started",
            EventKind::TaskCompleted { .. } => "task_completed",
            EventKind::UsageRecorded { .. } => "usage_recorded",
            EventKind::SessionCompleted { .. } => "session_completed",
            _ => "other",
        })
        .collect();

    for expected in [
        "session_started",
        "task_planned",
        "agent_selected",
        "agent_started",
        "task_completed",
        "usage_recorded",
        "session_completed",
    ] {
        assert!(
            kinds.contains(&expected),
            "no {expected} event in {kinds:?}"
        );
    }
}

#[tokio::test]
async fn every_routing_decision_is_explainable_after_the_fact() {
    let orchestrator = orchestrator_with(vec![
        MockAgent::always("a", Behaviour::ok("done")).with_descriptor(descriptor("a", 3.0, 15.0)),
        MockAgent::always("b", Behaviour::ok("done")).with_descriptor(descriptor("b", 1.0, 5.0)),
    ])
    .await;

    let events = collect_events(&orchestrator);
    orchestrator.run("add a health endpoint").await.unwrap();

    let explanations: Vec<String> = events
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.kind {
            EventKind::AgentSelected { explanation, .. } => Some(explanation),
            _ => None,
        })
        .collect();

    assert!(!explanations.is_empty());
    for explanation in &explanations {
        assert!(explanation.contains("Selected:"));
        assert!(explanation.contains("capability & quality"));
        assert!(explanation.contains("Alternatives:"), "got:\n{explanation}");
    }
}

// ---------------------------------------------------------------------------
// Scenario 2: failure, classification, fallback, handoff, continuation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_failing_agent_falls_back_to_another_and_the_session_still_succeeds() {
    // The preferred agent is cheaper, so the router picks it first — and it
    // crashes every time. The session must finish anyway.
    let broken = MockAgent::always(
        "broken",
        Behaviour::Crash {
            message: "agent process exited with code 139".into(),
        },
    )
    .with_descriptor(descriptor("broken", 0.1, 0.5));

    let working = MockAgent::always("working", Behaviour::ok("done"))
        .with_descriptor(descriptor("working", 10.0, 50.0));
    let working_calls = working.call_counter();

    let orchestrator = orchestrator_with(vec![broken, working]).await;
    let events = collect_events(&orchestrator);

    let result = orchestrator.run("add a health endpoint").await.unwrap();

    assert!(
        result.success,
        "the session must survive a dead agent: {}",
        result.summary
    );
    assert!(
        working_calls.load(std::sync::atomic::Ordering::SeqCst) > 0,
        "the healthy agent should have picked up the work"
    );

    let events = events.await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(&e.kind, EventKind::AgentFailed { class, .. }
                if *class == cuma_core::ErrorClass::AgentCrash)),
        "the crash should be classified, not just logged"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e.kind, EventKind::FallbackSelected { .. })),
        "and it should trigger a visible fallback"
    );
}

#[tokio::test]
async fn a_rate_limited_agent_is_retried_before_being_abandoned() {
    // Rate limits are transient: the policy should back off and try the same
    // agent again rather than immediately burning a fallback.
    let flaky = MockAgent::scripted(
        "flaky",
        vec![
            Behaviour::RateLimit {
                retry_after_ms: Some(1),
            },
            Behaviour::ok("succeeded on the retry"),
        ],
    )
    .with_descriptor(descriptor("flaky", 0.1, 0.5));
    let flaky_calls = flaky.call_counter();

    let orchestrator = orchestrator_with(vec![flaky]).await;
    let events = collect_events(&orchestrator);

    let result = orchestrator.run("write the docs").await.unwrap();

    assert!(result.success, "{}", result.summary);
    assert!(
        flaky_calls.load(std::sync::atomic::Ordering::SeqCst) >= 2,
        "the same agent should have been retried"
    );

    assert!(
        events
            .await
            .unwrap()
            .iter()
            .any(|e| matches!(e.kind, EventKind::RetryScheduled { .. })),
        "and the retry should be auditable"
    );
}

#[tokio::test]
async fn an_agent_that_keeps_crashing_has_its_circuit_breaker_opened() {
    let broken = MockAgent::always(
        "broken",
        Behaviour::Crash {
            message: "boom".into(),
        },
    )
    .with_descriptor(descriptor("broken", 0.1, 0.5));

    let working = MockAgent::always("working", Behaviour::ok("done"))
        .with_descriptor(descriptor("working", 10.0, 50.0));

    let orchestrator = orchestrator_with(vec![broken, working]).await;
    orchestrator.run("add a health endpoint").await.unwrap();

    let broken_id = AgentId::new("broken");
    assert_eq!(
        orchestrator.breakers().health(&broken_id),
        HealthState::Unavailable,
        "sustained crashes should take an agent out of rotation"
    );
    assert!(!orchestrator.breakers().allows(&broken_id, None));

    // And that state reached the registry, so it survives into the next session.
    let stored = orchestrator.agents().get(&broken_id).await.unwrap();
    assert!(!stored.is_routable());
}

#[tokio::test]
async fn authentication_failures_are_not_retried() {
    let unauthenticated = MockAgent::always("locked", Behaviour::AuthFailure)
        .with_descriptor(descriptor("locked", 0.1, 0.5));
    let calls = unauthenticated.call_counter();

    let orchestrator = orchestrator_with(vec![unauthenticated]).await;
    let result = orchestrator.run("write the docs").await.unwrap();

    assert!(!result.success);
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a rejected credential does not become valid on a second try"
    );
}

#[tokio::test]
async fn a_failed_task_cascades_to_the_tasks_that_depend_on_it() {
    let broken = MockAgent::always("broken", Behaviour::AuthFailure)
        .with_descriptor(descriptor("broken", 0.1, 0.5));

    let orchestrator = orchestrator_with(vec![broken]).await;
    let events = collect_events(&orchestrator);

    let result = orchestrator
        .run("implement OAuth and fix the tests")
        .await
        .unwrap();

    assert!(!result.success);
    assert_eq!(result.failed_tasks(), 1, "only the first task truly failed");
    assert!(
        result.skipped_tasks() > 0,
        "its dependents should be skipped, not attempted"
    );
    assert!(
        result.graph.is_complete(),
        "the session must still terminate"
    );

    assert!(
        events
            .await
            .unwrap()
            .iter()
            .any(|e| matches!(e.kind, EventKind::TaskSkipped { .. })),
        "and the user should be told which tasks were skipped and why"
    );
}

#[tokio::test]
async fn a_session_with_no_reachable_agent_fails_cleanly_rather_than_hanging() {
    let orchestrator = orchestrator_with(vec![]).await;
    let result = orchestrator.run("do something").await.unwrap();

    assert!(!result.success);
    assert!(result.graph.is_complete());
    assert_eq!(result.usage.attempts, 0, "nothing should have been spent");
}

#[tokio::test]
async fn a_hung_agent_is_cut_off_at_its_deadline() {
    let hung =
        MockAgent::always("hung", Behaviour::Timeout).with_descriptor(descriptor("hung", 0.1, 0.5));

    let mut config = config();
    config.limits.task_timeout_secs = 1;
    config.limits.max_retries = 1;

    let mut orchestrator = Orchestrator::new(
        config,
        Arc::new(HeuristicPlanner::new()),
        std::env::temp_dir(),
    );
    orchestrator.add_agent(Arc::new(hung)).await.unwrap();

    let started = std::time::Instant::now();
    let result = orchestrator.run("write the docs").await.unwrap();

    assert!(!result.success);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(30),
        "a hung agent must not hang the session; took {:?}",
        started.elapsed()
    );
}

// ---------------------------------------------------------------------------
// Routing behaviour observed through a whole session
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cost_first_routing_prefers_the_cheaper_agent_for_real_work() {
    let cheap = MockAgent::always("cheap", Behaviour::ok("done"))
        .with_descriptor(descriptor("cheap", 0.25, 1.25));
    let cheap_calls = cheap.call_counter();

    let premium = MockAgent::always("premium", Behaviour::ok("done"))
        .with_descriptor(descriptor("premium", 15.0, 75.0));
    let premium_calls = premium.call_counter();

    let mut config = config();
    config.router.strategy = RoutingStrategy::CostFirst;
    config.router.weights = RoutingStrategy::CostFirst.default_weights();

    let mut orchestrator = Orchestrator::new(
        config,
        Arc::new(HeuristicPlanner::new()),
        std::env::temp_dir(),
    );
    orchestrator.add_agent(Arc::new(cheap)).await.unwrap();
    orchestrator.add_agent(Arc::new(premium)).await.unwrap();

    let result = orchestrator.run("add a health endpoint").await.unwrap();
    assert!(result.success);

    use std::sync::atomic::Ordering::SeqCst;
    assert!(
        cheap_calls.load(SeqCst) > 0 && premium_calls.load(SeqCst) == 0,
        "cost-first sent {} tasks to cheap and {} to premium",
        cheap_calls.load(SeqCst),
        premium_calls.load(SeqCst)
    );
}

#[tokio::test]
async fn a_session_budget_stops_spending_rather_than_running_to_completion() {
    let expensive = MockAgent::always("expensive", Behaviour::ok("done"))
        .with_descriptor(descriptor("expensive", 15.0, 75.0))
        // A very large token report, so one task blows a tiny budget.
        .with_tokens(cuma_core::TokenUsage::reported(500_000, 500_000));

    let mut config = config();
    config.limits.max_cost_usd = Some(0.05);

    let mut orchestrator = Orchestrator::new(
        config,
        Arc::new(HeuristicPlanner::new()),
        std::env::temp_dir(),
    );
    orchestrator.add_agent(Arc::new(expensive)).await.unwrap();

    let result = orchestrator.run("add a health endpoint").await.unwrap();

    assert!(
        !result.success,
        "the budget should have stopped the session"
    );
    assert!(
        result.completed_tasks() < result.graph.len(),
        "not every task should have run"
    );
}

#[tokio::test]
async fn observed_outcomes_accumulate_into_routing_history() {
    let orchestrator = orchestrator_with(vec![
        MockAgent::always("a", Behaviour::ok("done")).with_descriptor(descriptor("a", 3.0, 15.0)),
    ])
    .await;

    orchestrator.run("add a health endpoint").await.unwrap();

    let history = orchestrator.history_snapshot().await;
    assert!(
        !history.is_empty(),
        "the session should have taught the router something"
    );
}

#[tokio::test]
async fn usage_is_broken_down_by_agent_and_task_type() {
    let orchestrator = orchestrator_with(vec![
        MockAgent::always("a", Behaviour::ok("done")).with_descriptor(descriptor("a", 3.0, 15.0)),
    ])
    .await;

    orchestrator.run("add a health endpoint").await.unwrap();

    let usage = orchestrator.usage_snapshot().await;
    assert_eq!(usage.by_agent().len(), 1);
    assert!(
        usage.by_task_type().len() > 1,
        "a plan spans several task types"
    );
    assert!(usage.by_model().len() == 1);
}

#[tokio::test]
async fn a_task_the_agent_reports_as_failed_is_not_marked_completed() {
    let honest = MockAgent::always(
        "honest",
        Behaviour::TaskFailure {
            reason: "the tests still fail".into(),
        },
    )
    .with_descriptor(descriptor("honest", 1.0, 1.0));

    let orchestrator = orchestrator_with(vec![honest]).await;
    let result = orchestrator.run("write the docs").await.unwrap();

    assert!(!result.success);
    assert!(
        result.graph.iter().any(|t| t.status == TaskStatus::Failed),
        "an agent that says it failed must be believed"
    );
}
