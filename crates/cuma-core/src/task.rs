//! Tasks, the plan DAG, and execution outcomes.
//!
//! The planner produces a [`TaskGraph`]; the orchestrator walks it. The graph
//! owns dependency semantics — which tasks are ready, which are blocked, which
//! can run in parallel — so the orchestrator never has to reason about edges.

use crate::capability::CapabilitySet;
use crate::ids::{AgentId, AttemptId, ModelId, TaskId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// The kind of work a task represents.
///
/// This is a routing signal: adaptive scoring buckets historical success rates
/// by `(agent, model, task_type)`, so the type must be coarse enough that
/// buckets accumulate data and fine enough to be predictive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskType {
    /// Read and summarize an existing codebase.
    Inspection,
    /// Look something up.
    Research,
    /// Decide how something should be built.
    Design,
    /// Write or modify code.
    Implementation,
    /// Find and fix a defect.
    BugFix,
    /// Restructure without behaviour change.
    Refactor,
    /// Write or repair tests.
    Testing,
    /// Run a command and interpret the result.
    Validation,
    /// Produce prose.
    Documentation,
    /// Review a change.
    Review,
    /// Anything else.
    General,
}

impl TaskType {
    /// The capabilities this kind of work implies, before task-specific extras.
    pub fn baseline_capabilities(&self) -> CapabilitySet {
        use crate::capability::Capability as C;
        let caps: &[C] = match self {
            Self::Inspection => &[C::CodeComprehension, C::FileSystem],
            Self::Research => &[C::Research],
            Self::Design => &[C::Architecture, C::Planning],
            Self::Implementation => &[C::CodeGeneration, C::CodeEditing, C::FileSystem],
            Self::BugFix => &[C::Debugging, C::CodeEditing, C::CodeComprehension],
            Self::Refactor => &[C::Refactoring, C::CodeEditing, C::CodeComprehension],
            Self::Testing => &[C::Testing, C::CodeGeneration],
            Self::Validation => &[C::ShellExecution],
            Self::Documentation => &[C::Documentation],
            Self::Review => &[C::CodeReview, C::CodeComprehension],
            Self::General => &[],
        };
        caps.iter().cloned().collect()
    }

    /// A 0-1 prior on how demanding this kind of work is.
    ///
    /// Used only as a starting point: a task's own `complexity` overrides it
    /// once the planner or the user has an opinion.
    pub fn baseline_complexity(&self) -> f64 {
        match self {
            Self::Validation => 0.1,
            Self::Inspection | Self::Documentation => 0.3,
            Self::Testing | Self::Research => 0.4,
            Self::Review | Self::Refactor => 0.5,
            Self::Implementation | Self::General => 0.6,
            Self::BugFix => 0.7,
            Self::Design => 0.8,
        }
    }
}

/// How much damage a task could do if it goes wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    /// Read-only.
    #[default]
    ReadOnly,
    /// Writes files that git can restore.
    Low,
    /// Writes files and runs commands.
    Medium,
    /// Potentially destructive; requires an explicit policy to proceed.
    High,
}

/// Where a task is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Waiting on dependencies.
    #[default]
    Pending,
    /// Dependencies satisfied, not yet started.
    Ready,
    /// Assigned to an agent and executing.
    Running,
    /// Finished successfully.
    Completed,
    /// Finished unsuccessfully and out of retries.
    Failed,
    /// Skipped because a dependency failed.
    Skipped,
    /// Cancelled by the user or by a cascading failure.
    Cancelled,
}

impl TaskStatus {
    /// Whether the task will not change state again without intervention.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Skipped | Self::Cancelled
        )
    }
}

/// Token accounting for one attempt.
///
/// `reported` records whether the numbers came from the agent or were
/// estimated locally. Presenting an estimate as ground truth is the single
/// easiest way to make a cost dashboard lie, so the flag is not optional.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Input tokens.
    pub input: u64,
    /// Output tokens.
    pub output: u64,
    /// Tokens served from cache.
    pub cached: u64,
    /// Whether the agent reported these numbers (`true`) or we estimated them.
    pub reported: bool,
}

impl TokenUsage {
    /// Usage reported by the agent itself.
    pub fn reported(input: u64, output: u64) -> Self {
        Self {
            input,
            output,
            cached: 0,
            reported: true,
        }
    }

    /// Usage we estimated locally.
    pub fn estimated(input: u64, output: u64) -> Self {
        Self {
            input,
            output,
            cached: 0,
            reported: false,
        }
    }

    /// Input plus output. Cached tokens are already counted in `input`.
    pub fn total(&self) -> u64 {
        self.input.saturating_add(self.output)
    }

    /// Merge two usage records. The result is reported only if both were.
    pub fn merge(self, other: Self) -> Self {
        Self {
            input: self.input.saturating_add(other.input),
            output: self.output.saturating_add(other.output),
            cached: self.cached.saturating_add(other.cached),
            reported: self.reported && other.reported,
        }
    }
}

/// What the planner decided a task is, before anything executes it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSpec {
    /// One-line description handed to the executing agent.
    pub description: String,
    /// The kind of work.
    pub task_type: TaskType,
    /// Capabilities the executing agent must have.
    pub required_capabilities: CapabilitySet,
    /// Tasks that must complete first.
    pub dependencies: Vec<TaskId>,
    /// Higher runs first among ready tasks.
    pub priority: u8,
    /// Blast radius.
    pub risk: Risk,
    /// 0-1 difficulty estimate.
    pub complexity: f64,
    /// Rough token cost estimate, used for budget checks before execution.
    pub estimated_tokens: Option<u64>,
}

impl TaskSpec {
    /// A spec with capabilities and complexity derived from `task_type`.
    pub fn new(description: impl Into<String>, task_type: TaskType) -> Self {
        Self {
            description: description.into(),
            task_type,
            required_capabilities: task_type.baseline_capabilities(),
            dependencies: Vec::new(),
            priority: 5,
            risk: Risk::default(),
            complexity: task_type.baseline_complexity(),
            estimated_tokens: None,
        }
    }

    /// Declare a dependency on another task.
    #[must_use]
    pub fn depends_on(mut self, task: TaskId) -> Self {
        self.dependencies.push(task);
        self
    }

    /// Override the blast radius.
    #[must_use]
    pub fn with_risk(mut self, risk: Risk) -> Self {
        self.risk = risk;
        self
    }

    /// Override the complexity estimate, clamped to `[0.0, 1.0]`.
    #[must_use]
    pub fn with_complexity(mut self, complexity: f64) -> Self {
        self.complexity = complexity.clamp(0.0, 1.0);
        self
    }

    /// Require an additional capability beyond the type's baseline.
    #[must_use]
    pub fn requiring(mut self, capability: crate::capability::Capability) -> Self {
        self.required_capabilities.insert(capability);
        self
    }
}

/// The result of one execution attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionOutcome {
    /// Which attempt this was.
    pub attempt_id: AttemptId,
    /// Which agent ran it.
    pub agent_id: AgentId,
    /// Which model, when the agent disclosed one.
    pub model_id: Option<ModelId>,
    /// Whether the task succeeded.
    pub success: bool,
    /// The agent's textual output.
    pub output: String,
    /// Files the agent reported changing.
    pub changed_files: Vec<String>,
    /// Token accounting.
    pub tokens: TokenUsage,
    /// Wall-clock duration.
    pub latency_ms: u64,
    /// Failure classification, when it failed.
    pub failure_class: Option<crate::error::ErrorClass>,
    /// Human-readable failure reason.
    pub failure_reason: Option<String>,
}

/// A task, as tracked by the orchestrator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    /// Stable identifier.
    pub id: TaskId,
    /// Parent, for subtasks produced by re-planning.
    pub parent_id: Option<TaskId>,
    /// What the planner decided.
    pub spec: TaskSpec,
    /// Current status.
    pub status: TaskStatus,
    /// Currently or last assigned agent.
    pub assigned_agent: Option<AgentId>,
    /// Currently or last assigned model.
    pub assigned_model: Option<ModelId>,
    /// Every attempt made, in order. Retries and fallbacks are auditable here.
    pub attempts: Vec<ExecutionOutcome>,
    /// Artifacts produced (file paths, URLs, identifiers).
    pub artifacts: Vec<String>,
}

impl Task {
    /// A pending task from a spec.
    pub fn new(spec: TaskSpec) -> Self {
        Self {
            id: TaskId::generate(),
            parent_id: None,
            spec,
            status: TaskStatus::Pending,
            assigned_agent: None,
            assigned_model: None,
            attempts: Vec::new(),
            artifacts: Vec::new(),
        }
    }

    /// A pending task with a caller-chosen id, for deterministic tests and
    /// for plans whose dependency edges are written before the tasks exist.
    pub fn with_id(id: TaskId, spec: TaskSpec) -> Self {
        Self {
            id,
            ..Self::new(spec)
        }
    }

    /// How many times this task has been attempted.
    pub fn attempt_count(&self) -> usize {
        self.attempts.len()
    }

    /// The agent+model pairs that have already failed this task.
    ///
    /// The router uses this to avoid re-selecting a target that just failed —
    /// the difference between a fallback and an infinite loop.
    pub fn failed_targets(&self) -> Vec<(AgentId, Option<ModelId>)> {
        self.attempts
            .iter()
            .filter(|a| !a.success)
            .map(|a| (a.agent_id.clone(), a.model_id.clone()))
            .collect()
    }

    /// The successful outcome, if the task has one.
    pub fn successful_outcome(&self) -> Option<&ExecutionOutcome> {
        self.attempts.iter().find(|a| a.success)
    }

    /// Total tokens across every attempt, including failed ones.
    ///
    /// Failed attempts cost real money, so excluding them would understate
    /// spend exactly when the harness is behaving worst.
    pub fn total_tokens(&self) -> TokenUsage {
        self.attempts
            .iter()
            .fold(TokenUsage::reported(0, 0), |acc, a| acc.merge(a.tokens))
    }
}

/// A directed acyclic graph of tasks.
///
/// Construction rejects cycles up front (see [`TaskGraph::validate`]) so the
/// orchestrator can assume progress is always possible.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskGraph {
    tasks: BTreeMap<TaskId, Task>,
    /// Insertion order, so that equally-ready, equally-prioritized tasks run
    /// in the order the planner wrote them.
    order: Vec<TaskId>,
}

impl TaskGraph {
    /// An empty graph.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a task, returning its id.
    pub fn add(&mut self, task: Task) -> TaskId {
        let id = task.id.clone();
        if self.tasks.insert(id.clone(), task).is_none() {
            self.order.push(id.clone());
        }
        id
    }

    /// Number of tasks.
    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    /// Whether the graph has no tasks.
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    /// Borrow a task.
    pub fn get(&self, id: &TaskId) -> Option<&Task> {
        self.tasks.get(id)
    }

    /// Mutably borrow a task.
    pub fn get_mut(&mut self, id: &TaskId) -> Option<&mut Task> {
        self.tasks.get_mut(id)
    }

    /// Iterate tasks in planner order.
    pub fn iter(&self) -> impl Iterator<Item = &Task> {
        self.order.iter().filter_map(|id| self.tasks.get(id))
    }

    /// Reject dangling dependencies and cycles.
    ///
    /// Called by the orchestrator before execution begins: a plan that cannot
    /// make progress should fail loudly at submission, not deadlock later.
    pub fn validate(&self) -> crate::error::Result<()> {
        for task in self.iter() {
            for dep in &task.spec.dependencies {
                if !self.tasks.contains_key(dep) {
                    return Err(crate::error::MetaAgentError::Configuration(format!(
                        "task {} depends on unknown task {dep}",
                        task.id
                    )));
                }
            }
        }

        // Kahn's algorithm: if we cannot drain every node, what remains is a cycle.
        let mut indegree: BTreeMap<&TaskId, usize> =
            self.tasks.keys().map(|id| (id, 0usize)).collect();
        for task in self.iter() {
            for dep in &task.spec.dependencies {
                if self.tasks.contains_key(dep) {
                    *indegree.entry(&task.id).or_insert(0) += 1;
                    let _ = dep;
                }
            }
        }

        let mut queue: VecDeque<&TaskId> = indegree
            .iter()
            .filter(|&(_, &d)| d == 0)
            .map(|(id, _)| *id)
            .collect();
        let mut visited = 0usize;

        while let Some(id) = queue.pop_front() {
            visited += 1;
            for task in self.iter() {
                if task.spec.dependencies.contains(id)
                    && let Some(d) = indegree.get_mut(&task.id)
                {
                    *d -= 1;
                    if *d == 0 {
                        queue.push_back(&task.id);
                    }
                }
            }
        }

        if visited != self.tasks.len() {
            return Err(crate::error::MetaAgentError::Configuration(
                "task graph contains a dependency cycle".to_owned(),
            ));
        }

        Ok(())
    }

    /// Tasks whose dependencies have all completed, highest priority first.
    ///
    /// This is the orchestrator's parallelism frontier: everything returned
    /// here may run concurrently as far as *dependencies* are concerned.
    /// Workspace conflicts are a separate concern handled by file ownership.
    pub fn ready_tasks(&self) -> Vec<&Task> {
        let mut ready: Vec<&Task> = self
            .iter()
            .filter(|t| matches!(t.status, TaskStatus::Pending | TaskStatus::Ready))
            .filter(|t| {
                t.spec.dependencies.iter().all(|dep| {
                    self.tasks
                        .get(dep)
                        .is_some_and(|d| d.status == TaskStatus::Completed)
                })
            })
            .collect();

        // Stable sort: priority decides, insertion order breaks ties.
        ready.sort_by(|a, b| b.spec.priority.cmp(&a.spec.priority));
        ready
    }

    /// Whether every task has reached a terminal state.
    pub fn is_complete(&self) -> bool {
        self.tasks.values().all(|t| t.status.is_terminal())
    }

    /// Mark every task transitively depending on `failed` as skipped.
    ///
    /// Returns the ids that were skipped, so the caller can report them rather
    /// than leaving the user to work out why half the plan never ran.
    pub fn cascade_skip(&mut self, failed: &TaskId) -> Vec<TaskId> {
        let mut skipped = Vec::new();
        let mut frontier: BTreeSet<TaskId> = BTreeSet::new();
        frontier.insert(failed.clone());

        loop {
            let dependents: Vec<TaskId> = self
                .tasks
                .values()
                .filter(|t| !t.status.is_terminal())
                .filter(|t| t.spec.dependencies.iter().any(|d| frontier.contains(d)))
                .map(|t| t.id.clone())
                .collect();

            if dependents.is_empty() {
                break;
            }

            for id in dependents {
                if let Some(task) = self.tasks.get_mut(&id) {
                    task.status = TaskStatus::Skipped;
                }
                frontier.insert(id.clone());
                skipped.push(id);
            }
        }

        skipped
    }

    /// Tokens consumed across the whole plan.
    pub fn total_tokens(&self) -> TokenUsage {
        self.tasks
            .values()
            .fold(TokenUsage::reported(0, 0), |acc, t| {
                acc.merge(t.total_tokens())
            })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn graph_with_chain() -> (TaskGraph, TaskId, TaskId, TaskId) {
        let mut g = TaskGraph::new();
        let a = g.add(Task::new(TaskSpec::new("inspect", TaskType::Inspection)));
        let b = g.add(Task::new(
            TaskSpec::new("implement", TaskType::Implementation).depends_on(a.clone()),
        ));
        let c = g.add(Task::new(
            TaskSpec::new("test", TaskType::Testing).depends_on(b.clone()),
        ));
        (g, a, b, c)
    }

    #[test]
    fn only_dependency_free_tasks_are_ready() {
        let (g, a, _, _) = graph_with_chain();
        let ready = g.ready_tasks();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, a);
    }

    #[test]
    fn completing_a_dependency_unblocks_its_dependent() {
        let (mut g, a, b, _) = graph_with_chain();
        g.get_mut(&a).unwrap().status = TaskStatus::Completed;

        let ready = g.ready_tasks();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, b);
    }

    #[test]
    fn independent_tasks_are_all_ready_at_once() {
        let mut g = TaskGraph::new();
        g.add(Task::new(TaskSpec::new("docs", TaskType::Documentation)));
        g.add(Task::new(TaskSpec::new("research", TaskType::Research)));
        g.add(Task::new(TaskSpec::new("review", TaskType::Review)));

        assert_eq!(g.ready_tasks().len(), 3, "these can run in parallel");
    }

    #[test]
    fn ready_tasks_are_ordered_by_priority() {
        let mut g = TaskGraph::new();
        let mut low = TaskSpec::new("low", TaskType::General);
        low.priority = 1;
        let mut high = TaskSpec::new("high", TaskType::General);
        high.priority = 9;

        g.add(Task::new(low));
        let high_id = g.add(Task::new(high));

        assert_eq!(g.ready_tasks()[0].id, high_id);
    }

    #[test]
    fn a_dependency_cycle_is_rejected_rather_than_deadlocking() {
        let mut g = TaskGraph::new();
        let a = TaskId::new("a");
        let b = TaskId::new("b");
        g.add(Task::with_id(
            a.clone(),
            TaskSpec::new("a", TaskType::General).depends_on(b.clone()),
        ));
        g.add(Task::with_id(
            b,
            TaskSpec::new("b", TaskType::General).depends_on(a),
        ));

        let err = g.validate().unwrap_err();
        assert!(err.to_string().contains("cycle"), "got: {err}");
    }

    #[test]
    fn a_dangling_dependency_is_rejected() {
        let mut g = TaskGraph::new();
        g.add(Task::new(
            TaskSpec::new("orphan", TaskType::General).depends_on(TaskId::new("nowhere")),
        ));
        assert!(g.validate().unwrap_err().to_string().contains("unknown"));
    }

    #[test]
    fn a_valid_chain_passes_validation() {
        let (g, _, _, _) = graph_with_chain();
        assert!(g.validate().is_ok());
    }

    #[test]
    fn a_failure_cascades_to_every_transitive_dependent() {
        let (mut g, a, b, c) = graph_with_chain();
        g.get_mut(&a).unwrap().status = TaskStatus::Failed;

        let skipped = g.cascade_skip(&a);
        assert_eq!(skipped.len(), 2);
        assert_eq!(g.get(&b).unwrap().status, TaskStatus::Skipped);
        assert_eq!(g.get(&c).unwrap().status, TaskStatus::Skipped);
        assert!(g.is_complete());
    }

    #[test]
    fn failed_targets_are_remembered_so_fallback_does_not_loop() {
        let mut task = Task::new(TaskSpec::new("x", TaskType::General));
        task.attempts.push(ExecutionOutcome {
            attempt_id: AttemptId::generate(),
            agent_id: AgentId::new("codex"),
            model_id: Some(ModelId::new("m1")),
            success: false,
            output: String::new(),
            changed_files: vec![],
            tokens: TokenUsage::default(),
            latency_ms: 10,
            failure_class: Some(crate::error::ErrorClass::RateLimit),
            failure_reason: Some("429".into()),
        });

        let failed = task.failed_targets();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].0, AgentId::new("codex"));
    }

    #[test]
    fn token_totals_include_failed_attempts() {
        let mut task = Task::new(TaskSpec::new("x", TaskType::General));
        for success in [false, true] {
            task.attempts.push(ExecutionOutcome {
                attempt_id: AttemptId::generate(),
                agent_id: AgentId::new("a"),
                model_id: None,
                success,
                output: String::new(),
                changed_files: vec![],
                tokens: TokenUsage::reported(100, 50),
                latency_ms: 1,
                failure_class: None,
                failure_reason: None,
            });
        }
        assert_eq!(task.total_tokens().total(), 300);
    }

    #[test]
    fn estimated_usage_taints_a_merged_total() {
        let merged = TokenUsage::reported(10, 10).merge(TokenUsage::estimated(10, 10));
        assert!(
            !merged.reported,
            "a total built on an estimate is an estimate"
        );
    }
}
