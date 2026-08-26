//! Parallel execution, and the safety mechanism that gates it.
//!
//! The claim being tested is narrow and specific: independent tasks *do* run
//! concurrently, and tasks that would write the same paths *do not*, even
//! though the DAG says both are ready.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use async_trait::async_trait;
use cuma_config::{Config, LimitsConfig};
use cuma_core::ports::{AgentAdapter, ExecutionRequest, ExecutionUpdate, PlanningContext};
use cuma_core::{
    AgentDescriptor, AgentId, AgentProtocol, AttemptId, Capability, CapabilitySet,
    ExecutionOutcome, Risk, Task, TaskGraph, TaskSpec, TaskType, TokenUsage,
};
use cuma_orchestrator::Orchestrator;
use cuma_planner::HeuristicPlanner;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// An agent that records how many calls overlapped in time.
///
/// The high-water mark is the observable that distinguishes real concurrency
/// from a fast sequence: a sequential run never exceeds 1.
struct ConcurrencyProbe {
    id: AgentId,
    descriptor: AgentDescriptor,
    in_flight: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    hold: Duration,
}

impl ConcurrencyProbe {
    fn new(id: &str, hold: Duration) -> Self {
        let mut descriptor = AgentDescriptor::new(id, id, AgentProtocol::Native)
            .with_capabilities(all_capabilities());
        descriptor.models.clear();

        Self {
            id: AgentId::new(id),
            descriptor,
            in_flight: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
            hold,
        }
    }

    fn peak_handle(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.peak)
    }
}

#[async_trait]
impl AgentAdapter for ConcurrencyProbe {
    fn agent_id(&self) -> &AgentId {
        &self.id
    }

    async fn describe(&self) -> cuma_core::error::Result<AgentDescriptor> {
        Ok(self.descriptor.clone())
    }

    async fn execute(
        &self,
        _request: ExecutionRequest,
        _updates: tokio::sync::mpsc::Sender<ExecutionUpdate>,
    ) -> cuma_core::error::Result<ExecutionOutcome> {
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);

        tokio::time::sleep(self.hold).await;

        self.in_flight.fetch_sub(1, Ordering::SeqCst);

        Ok(ExecutionOutcome {
            attempt_id: AttemptId::generate(),
            agent_id: self.id.clone(),
            model_id: None,
            success: true,
            output: "done".to_owned(),
            changed_files: Vec::new(),
            tokens: TokenUsage::reported(10, 10),
            latency_ms: 1,
            failure_class: None,
            failure_reason: None,
        })
    }
}

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

/// A planner that returns a fixed graph, so a test controls the shape exactly.
struct FixedPlanner(TaskGraph);

#[async_trait]
impl cuma_core::ports::Planner for FixedPlanner {
    async fn plan(
        &self,
        _goal: &str,
        _context: &PlanningContext,
    ) -> cuma_core::error::Result<TaskGraph> {
        Ok(self.0.clone())
    }
}

fn config(max_parallel: usize) -> Config {
    Config {
        limits: LimitsConfig {
            max_parallel_tasks: max_parallel,
            max_retries: 1,
            task_timeout_secs: 10,
            ..LimitsConfig::default()
        },
        ..Config::default()
    }
}

async fn run_with(
    graph: TaskGraph,
    probe: ConcurrencyProbe,
    max_parallel: usize,
) -> Arc<AtomicUsize> {
    let peak = probe.peak_handle();

    let mut orchestrator = Orchestrator::new(
        config(max_parallel),
        Arc::new(FixedPlanner(graph)),
        std::env::temp_dir(),
    );
    orchestrator.add_agent(Arc::new(probe)).await.unwrap();

    let result = orchestrator.run("go").await.unwrap();
    assert!(result.success, "{}", result.summary);

    peak
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn tasks_writing_different_paths_run_concurrently() {
    let mut graph = TaskGraph::new();
    for path in ["src/auth.rs", "src/router.rs", "src/store.rs"] {
        graph.add(Task::new(
            TaskSpec::new(format!("Edit {path}"), TaskType::Implementation).with_risk(Risk::Medium),
        ));
    }

    let peak = run_with(
        graph,
        ConcurrencyProbe::new("worker", Duration::from_millis(120)),
        4,
    )
    .await;

    assert!(
        peak.load(Ordering::SeqCst) > 1,
        "independent tasks should have overlapped, peak was {}",
        peak.load(Ordering::SeqCst)
    );
}

#[tokio::test]
async fn tasks_writing_the_same_file_are_serialized_despite_being_dependency_ready() {
    // Neither task depends on the other, so the DAG says both are ready. They
    // both write `src/auth.rs`, so exactly one may run at a time.
    let mut graph = TaskGraph::new();
    for description in [
        "Edit src/auth.rs to add the token endpoint",
        "Edit src/auth.rs to add refresh handling",
    ] {
        graph.add(Task::new(
            TaskSpec::new(description, TaskType::Implementation).with_risk(Risk::Medium),
        ));
    }

    let peak = run_with(
        graph,
        ConcurrencyProbe::new("worker", Duration::from_millis(120)),
        4,
    )
    .await;

    assert_eq!(
        peak.load(Ordering::SeqCst),
        1,
        "two tasks writing the same file must never overlap"
    );
}

#[tokio::test]
async fn a_task_inside_a_directory_another_task_owns_is_serialized() {
    let mut graph = TaskGraph::new();
    graph.add(Task::new(
        TaskSpec::new("Restructure src/auth/ entirely", TaskType::Refactor).with_risk(Risk::Medium),
    ));
    graph.add(Task::new(
        TaskSpec::new("Edit src/auth/token.rs", TaskType::Implementation).with_risk(Risk::Medium),
    ));

    let peak = run_with(
        graph,
        ConcurrencyProbe::new("worker", Duration::from_millis(120)),
        4,
    )
    .await;

    assert_eq!(
        peak.load(Ordering::SeqCst),
        1,
        "a directory claim must exclude files inside it"
    );
}

#[tokio::test]
async fn read_only_tasks_never_contend_with_anything() {
    // Inspection and review cannot corrupt anything, so they run together
    // regardless of what paths they mention.
    let mut graph = TaskGraph::new();
    for description in [
        "Inspect src/auth.rs",
        "Review src/auth.rs",
        "Read src/auth.rs again",
    ] {
        graph.add(Task::new(
            TaskSpec::new(description, TaskType::Inspection).with_risk(Risk::ReadOnly),
        ));
    }

    let peak = run_with(
        graph,
        ConcurrencyProbe::new("worker", Duration::from_millis(120)),
        4,
    )
    .await;

    assert!(
        peak.load(Ordering::SeqCst) > 1,
        "read-only work should not be serialized, peak was {}",
        peak.load(Ordering::SeqCst)
    );
}

#[tokio::test]
async fn a_task_whose_paths_cannot_be_predicted_runs_alone() {
    // Nothing path-shaped in the description, so the whole workspace is
    // claimed. Pessimistic on purpose.
    let mut graph = TaskGraph::new();
    graph.add(Task::new(
        TaskSpec::new("Implement OAuth authentication", TaskType::Implementation)
            .with_risk(Risk::Medium),
    ));
    graph.add(Task::new(
        TaskSpec::new("Edit src/router.rs", TaskType::Implementation).with_risk(Risk::Medium),
    ));

    let peak = run_with(
        graph,
        ConcurrencyProbe::new("worker", Duration::from_millis(120)),
        4,
    )
    .await;

    assert_eq!(
        peak.load(Ordering::SeqCst),
        1,
        "an unpredictable task must not run beside anything"
    );
}

#[tokio::test]
async fn max_parallel_tasks_bounds_concurrency() {
    let mut graph = TaskGraph::new();
    for index in 0..6 {
        graph.add(Task::new(
            TaskSpec::new(format!("Edit src/file{index}.rs"), TaskType::Implementation)
                .with_risk(Risk::Medium),
        ));
    }

    let peak = run_with(
        graph,
        ConcurrencyProbe::new("worker", Duration::from_millis(80)),
        2,
    )
    .await;

    let observed = peak.load(Ordering::SeqCst);
    assert!(observed > 1, "should have run in parallel at all");
    assert!(
        observed <= 2,
        "max_parallel_tasks = 2 must be a real bound, saw {observed}"
    );
}

#[tokio::test]
async fn serialized_tasks_all_still_complete() {
    // The point of deferring a contending task is that it runs *later*, not
    // that it is dropped.
    let mut graph = TaskGraph::new();
    for index in 0..4 {
        graph.add(Task::new(
            TaskSpec::new(
                format!("Edit src/auth.rs step {index}"),
                TaskType::Implementation,
            )
            .with_risk(Risk::Medium),
        ));
    }

    let probe = ConcurrencyProbe::new("worker", Duration::from_millis(20));
    let peak = probe.peak_handle();

    let mut orchestrator = Orchestrator::new(
        config(4),
        Arc::new(FixedPlanner(graph)),
        std::env::temp_dir(),
    );
    orchestrator.add_agent(Arc::new(probe)).await.unwrap();

    let result = orchestrator.run("go").await.unwrap();

    assert!(result.success, "{}", result.summary);
    assert_eq!(result.completed_tasks(), 4, "every task must still run");
    assert_eq!(peak.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn the_default_heuristic_plan_still_completes_under_parallelism() {
    // A real plan mixes read-only and writing tasks with dependencies; it must
    // still terminate with everything done.
    let probe = ConcurrencyProbe::new("worker", Duration::from_millis(5));

    let mut orchestrator = Orchestrator::new(
        config(4),
        Arc::new(HeuristicPlanner::new()),
        std::env::temp_dir(),
    );
    orchestrator.add_agent(Arc::new(probe)).await.unwrap();

    let result = orchestrator
        .run("implement OAuth authentication and fix the tests")
        .await
        .unwrap();

    assert!(result.success, "{}", result.summary);
    assert_eq!(result.failed_tasks(), 0);
    assert!(result.graph.is_complete());
}

// ---------------------------------------------------------------------------
// Command preparation: screening, RTK, sandboxing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_destructive_command_is_refused_before_it_can_be_wrapped_or_spawned() {
    let orchestrator = Orchestrator::new(
        config(1),
        Arc::new(HeuristicPlanner::new()),
        std::env::temp_dir(),
    );

    let refused = orchestrator
        .prepare_command("git reset --hard")
        .unwrap_err();
    assert!(
        refused.contains("allow_destructive_operations"),
        "the refusal must say how to permit it: {refused}"
    );
}

#[tokio::test]
async fn an_ordinary_command_is_permitted() {
    let orchestrator = Orchestrator::new(
        config(1),
        Arc::new(HeuristicPlanner::new()),
        std::env::temp_dir(),
    );

    let prepared = orchestrator.prepare_command("cargo test").unwrap();
    assert!(
        prepared.contains("cargo test"),
        "the command must survive preparation: {prepared}"
    );
}

#[tokio::test]
async fn an_explicit_policy_permits_a_destructive_command() {
    let mut config = config(1);
    config.security.allow_destructive_operations = true;

    let orchestrator = Orchestrator::new(
        config,
        Arc::new(HeuristicPlanner::new()),
        std::env::temp_dir(),
    );

    assert!(orchestrator.prepare_command("git reset --hard").is_ok());
}

#[tokio::test]
async fn rtk_savings_accumulate_into_the_usage_ledger() {
    let orchestrator = Orchestrator::new(
        config(1),
        Arc::new(HeuristicPlanner::new()),
        std::env::temp_dir(),
    );

    let raw = "passing test\n".repeat(500);
    let filtered = "1 failure: test_auth\n";

    orchestrator
        .record_rtk_saving(cuma_workspace::Saving::between(&raw, filtered))
        .await;

    let saved = orchestrator.usage_snapshot().await.rtk_tokens_saved();
    assert!(saved > 1_000, "saved {saved}");
}

#[tokio::test]
async fn a_filter_that_saved_nothing_records_nothing() {
    let orchestrator = Orchestrator::new(
        config(1),
        Arc::new(HeuristicPlanner::new()),
        std::env::temp_dir(),
    );

    orchestrator
        .record_rtk_saving(cuma_workspace::Saving::between("short", "short"))
        .await;

    assert_eq!(orchestrator.usage_snapshot().await.rtk_tokens_saved(), 0);
}
