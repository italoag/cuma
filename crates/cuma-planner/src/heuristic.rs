//! Rule-based decomposition.
//!
//! The heuristics recognize the *shape* of a request — does it change code,
//! does it need research, does it mention tests — and emit the standard
//! pipeline for that shape. This is unglamorous and it is also predictable,
//! auditable and free, which matters for the component that decides how every
//! subsequent token gets spent.

use async_trait::async_trait;
use cuma_core::error::Result;
use cuma_core::ports::{Planner, PlanningContext};
use cuma_core::{Capability, Risk, Task, TaskGraph, TaskId, TaskSpec, TaskType};

/// A deterministic, rule-based planner.
#[derive(Debug, Clone, Default)]
pub struct HeuristicPlanner {
    /// Whether to emit a validation task after any code-changing work.
    ///
    /// On by default: a plan that writes code and never runs anything has no
    /// way of telling the orchestrator it failed.
    pub always_validate: bool,
}

impl HeuristicPlanner {
    /// A planner that appends validation to code-changing plans.
    pub fn new() -> Self {
        Self {
            always_validate: true,
        }
    }
}

/// The signals the heuristics extract from a goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GoalShape {
    changes_code: bool,
    needs_research: bool,
    mentions_tests: bool,
    is_investigation: bool,
    is_review: bool,
    is_documentation: bool,
    is_design_heavy: bool,
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| haystack.contains(n))
}

/// Classify a goal. Matching is on lowercase substrings and covers English and
/// Portuguese, since the project's own documentation is written in both.
fn shape_of(goal: &str) -> GoalShape {
    let g = goal.to_lowercase();

    let changes_code = contains_any(
        &g,
        &[
            "implement", "implementa", "add ", "adicion", "create", "cri", "write", "escrev",
            "fix", "corrig", "conserta", "refactor", "refator", "update", "atualiz", "migrate",
            "migra", "rename", "renome", "remove", "remov", "delete", "port ", "upgrade",
        ],
    );

    let is_investigation = contains_any(
        &g,
        &[
            "why", "por que", "porque", "investigate", "investig", "debug", "diagnos",
            "understand", "entend", "explain", "explic", "analyz", "analis", "audit",
        ],
    );

    let is_review = contains_any(&g, &["review", "revis", "critique", "avali"]);
    let is_documentation = contains_any(&g, &["document", "documenta", "readme", "changelog"]);

    let mentions_tests = contains_any(
        &g,
        &["test", "teste", "spec", "coverage", "cobertura", "ci "],
    );

    let needs_research = contains_any(
        &g,
        &[
            "research", "pesquis", "how to", "como ", "best practice", "boas prátic",
            "compare", "compar", "evaluate", "oauth", "protocol", "protocolo", "spec",
            "library", "bibliotec", "sdk",
        ],
    );

    let is_design_heavy = contains_any(
        &g,
        &[
            "architect", "arquitet", "design", "desenh", "redesign", "restructure",
            "reestrutur", "rewrite", "reescrev", "migrate", "strategy", "estratégia",
        ],
    );

    GoalShape {
        changes_code,
        needs_research,
        mentions_tests,
        is_investigation,
        is_review,
        is_documentation,
        is_design_heavy,
    }
}

/// Add a task depending on everything in `after`, returning its id.
fn push(graph: &mut TaskGraph, mut spec: TaskSpec, after: &[TaskId]) -> TaskId {
    for dep in after {
        spec.dependencies.push(dep.clone());
    }
    graph.add(Task::new(spec))
}

#[async_trait]
impl Planner for HeuristicPlanner {
    async fn plan(&self, goal: &str, context: &PlanningContext) -> Result<TaskGraph> {
        let goal = goal.trim();
        if goal.is_empty() {
            return Err(cuma_core::MetaAgentError::Configuration(
                "cannot plan an empty goal".to_owned(),
            ));
        }

        let shape = shape_of(goal);
        let mut graph = TaskGraph::new();

        // Every plan starts by looking at the repository. Without it, the
        // executing agent has to rediscover the project on every task.
        let mut inspect_spec =
            TaskSpec::new(format!("Inspect the repository to understand: {goal}"), TaskType::Inspection);
        inspect_spec.priority = 9;
        let inspect = graph.add(Task::new(inspect_spec));

        // Research and design can run concurrently with each other; both only
        // need the inspection.
        let mut prerequisites = vec![inspect.clone()];

        if shape.needs_research {
            let research = push(
                &mut graph,
                TaskSpec::new(
                    format!("Research what is needed to: {goal}"),
                    TaskType::Research,
                )
                .requiring(Capability::Research),
                &[inspect.clone()],
            );
            prerequisites.push(research);
        }

        if shape.is_design_heavy {
            let design = push(
                &mut graph,
                TaskSpec::new(format!("Design the approach for: {goal}"), TaskType::Design)
                    .with_complexity(0.85),
                &[inspect.clone()],
            );
            prerequisites.push(design);
        }

        if shape.is_investigation && !shape.changes_code {
            // A pure investigation ends at its findings; inventing an
            // implementation task the user did not ask for would be scope creep.
            push(
                &mut graph,
                TaskSpec::new(
                    format!("Investigate and report findings for: {goal}"),
                    TaskType::BugFix,
                )
                .with_risk(Risk::ReadOnly),
                &prerequisites,
            );
            graph.validate()?;
            return Ok(graph);
        }

        if shape.is_review && !shape.changes_code {
            push(
                &mut graph,
                TaskSpec::new(format!("Review: {goal}"), TaskType::Review)
                    .with_risk(Risk::ReadOnly),
                &prerequisites,
            );
            graph.validate()?;
            return Ok(graph);
        }

        if shape.is_documentation && !shape.changes_code {
            push(
                &mut graph,
                TaskSpec::new(format!("Write documentation for: {goal}"), TaskType::Documentation)
                    .with_risk(Risk::Low),
                &prerequisites,
            );
            graph.validate()?;
            return Ok(graph);
        }

        let task_type = if shape.is_investigation {
            TaskType::BugFix
        } else {
            TaskType::Implementation
        };

        let implement = push(
            &mut graph,
            TaskSpec::new(format!("Implement: {goal}"), task_type).with_risk(Risk::Medium),
            &prerequisites,
        );

        let mut validation_deps = vec![implement.clone()];

        if shape.mentions_tests {
            let tests = push(
                &mut graph,
                TaskSpec::new(format!("Write or update tests for: {goal}"), TaskType::Testing)
                    .with_risk(Risk::Low),
                &[implement.clone()],
            );
            validation_deps.push(tests);
        }

        if self.always_validate {
            let validate = push(
                &mut graph,
                TaskSpec::new(
                    "Run the project's tests and report failures".to_owned(),
                    TaskType::Validation,
                )
                .requiring(Capability::ShellExecution)
                .with_risk(Risk::Low),
                &validation_deps,
            );

            push(
                &mut graph,
                TaskSpec::new(format!("Review the changes made for: {goal}"), TaskType::Review)
                    .with_risk(Risk::ReadOnly),
                &[validate],
            );
        }

        // A plan whose tasks nothing can run is worse than no plan: the
        // orchestrator would route every one of them into a dead end.
        if !context.available_capabilities.is_empty() {
            let unroutable: Vec<String> = graph
                .iter()
                .filter(|task| {
                    !context
                        .available_capabilities
                        .match_against(&task.spec.required_capabilities)
                        .is_complete()
                })
                .map(|task| task.spec.description.clone())
                .collect();

            if !unroutable.is_empty() {
                tracing::warn!(
                    tasks = ?unroutable,
                    "planned tasks require capabilities no registered agent provides"
                );
            }
        }

        graph.validate()?;
        Ok(graph)
    }

    async fn replan(
        &self,
        graph: &TaskGraph,
        failed: &Task,
        reason: &str,
    ) -> Result<Option<TaskGraph>> {
        // Context overflow is the case re-planning genuinely fixes: split the
        // task so each half carries less context. Anything else is better
        // handled by the router than by rewriting the plan.
        if !reason.to_lowercase().contains("context") {
            return Ok(None);
        }

        // Splitting a task that is already a split would recurse without bound.
        if failed.parent_id.is_some() {
            return Ok(None);
        }

        let mut revised = graph.clone();
        let Some(target) = revised.get_mut(&failed.id) else {
            return Ok(None);
        };
        target.status = cuma_core::TaskStatus::Skipped;

        let base = failed.spec.clone();
        let halves = [
            format!("{} (part 1 of 2: narrow the scope to the smallest change)", base.description),
            format!("{} (part 2 of 2: complete the remaining work)", base.description),
        ];

        let mut previous: Option<TaskId> = None;
        for description in halves {
            let mut spec = base.clone();
            spec.description = description;
            spec.estimated_tokens = base.estimated_tokens.map(|t| t / 2);

            if let Some(prev) = &previous {
                spec.dependencies = vec![prev.clone()];
            }

            let mut task = Task::new(spec);
            task.parent_id = Some(failed.id.clone());
            previous = Some(revised.add(task));
        }

        revised.validate()?;
        Ok(Some(revised))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    async fn plan(goal: &str) -> TaskGraph {
        HeuristicPlanner::new()
            .plan(goal, &PlanningContext::default())
            .await
            .unwrap()
    }

    fn types(graph: &TaskGraph) -> Vec<TaskType> {
        graph.iter().map(|t| t.spec.task_type).collect()
    }

    #[tokio::test]
    async fn an_empty_goal_is_rejected() {
        let result = HeuristicPlanner::new()
            .plan("   ", &PlanningContext::default())
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn every_plan_starts_by_inspecting_the_repository() {
        let graph = plan("fix the login bug").await;
        assert_eq!(graph.iter().next().unwrap().spec.task_type, TaskType::Inspection);
    }

    #[tokio::test]
    async fn every_plan_is_a_valid_dag() {
        for goal in [
            "implement OAuth authentication and fix the tests",
            "why is the build slow",
            "review the pending changes",
            "document the API",
            "refactor the router",
            "research the best websocket library",
        ] {
            let graph = plan(goal).await;
            assert!(graph.validate().is_ok(), "invalid plan for {goal:?}");
            assert!(!graph.is_empty());
        }
    }

    #[tokio::test]
    async fn the_documented_oauth_example_decomposes_as_specified() {
        // "Implemente autenticação OAuth nesse projeto e corrija os testes."
        let graph = plan("Implemente autenticação OAuth nesse projeto e corrija os testes").await;
        let types = types(&graph);

        assert!(types.contains(&TaskType::Inspection));
        assert!(types.contains(&TaskType::Research), "OAuth needs research");
        assert!(types.contains(&TaskType::Implementation));
        assert!(types.contains(&TaskType::Testing), "the goal mentions tests");
        assert!(types.contains(&TaskType::Validation), "and they must be run");
        assert!(types.contains(&TaskType::Review));
    }

    #[tokio::test]
    async fn research_and_design_can_run_in_parallel_after_inspection() {
        let graph = plan("redesign and migrate the storage layer using a new library").await;

        let mut graph = graph;
        let inspect = graph.iter().next().unwrap().id.clone();
        graph.get_mut(&inspect).unwrap().status = cuma_core::TaskStatus::Completed;

        let ready = graph.ready_tasks();
        assert!(
            ready.len() >= 2,
            "research and design should unblock together, got {}",
            ready.len()
        );
    }

    #[tokio::test]
    async fn a_pure_investigation_does_not_invent_an_implementation_task() {
        let graph = plan("why is the test suite flaky").await;
        let types = types(&graph);

        assert!(!types.contains(&TaskType::Implementation), "the user asked a question");
        assert!(graph.iter().all(|t| t.spec.risk == Risk::ReadOnly));
    }

    #[tokio::test]
    async fn a_review_request_produces_a_read_only_plan() {
        let graph = plan("review the changes on this branch").await;
        assert!(graph.iter().all(|t| t.spec.risk == Risk::ReadOnly));
        assert!(types(&graph).contains(&TaskType::Review));
    }

    #[tokio::test]
    async fn code_changing_plans_end_in_validation_and_review() {
        let graph = plan("add a health endpoint").await;
        let types = types(&graph);
        assert!(types.contains(&TaskType::Validation));
        assert!(types.contains(&TaskType::Review));
    }

    #[tokio::test]
    async fn validation_can_be_turned_off() {
        let planner = HeuristicPlanner {
            always_validate: false,
        };
        let graph = planner
            .plan("add a health endpoint", &PlanningContext::default())
            .await
            .unwrap();
        assert!(!types(&graph).contains(&TaskType::Validation));
    }

    #[tokio::test]
    async fn a_context_overflow_splits_the_offending_task_in_two() {
        let planner = HeuristicPlanner::new();
        let graph = plan("implement OAuth").await;

        let target = graph
            .iter()
            .find(|t| t.spec.task_type == TaskType::Implementation)
            .unwrap()
            .clone();

        let revised = planner
            .replan(&graph, &target, "ContextOverflow requires a smaller plan")
            .await
            .unwrap()
            .expect("context overflow should produce a revised plan");

        assert_eq!(revised.len(), graph.len() + 2);
        assert_eq!(
            revised.get(&target.id).unwrap().status,
            cuma_core::TaskStatus::Skipped
        );
        assert!(revised.validate().is_ok());

        let children: Vec<_> = revised
            .iter()
            .filter(|t| t.parent_id.as_ref() == Some(&target.id))
            .collect();
        assert_eq!(children.len(), 2);
    }

    #[tokio::test]
    async fn splitting_does_not_recurse_without_bound() {
        let planner = HeuristicPlanner::new();
        let graph = plan("implement OAuth").await;

        let mut already_split = graph
            .iter()
            .find(|t| t.spec.task_type == TaskType::Implementation)
            .unwrap()
            .clone();
        already_split.parent_id = Some(TaskId::new("some-parent"));

        let result = planner
            .replan(&graph, &already_split, "ContextOverflow again")
            .await
            .unwrap();

        assert!(result.is_none(), "a split of a split must not split again");
    }

    #[tokio::test]
    async fn failures_that_replanning_cannot_fix_are_left_to_the_router() {
        let planner = HeuristicPlanner::new();
        let graph = plan("implement OAuth").await;
        let target = graph.iter().next().unwrap().clone();

        let result = planner
            .replan(&graph, &target, "RateLimit is transient")
            .await
            .unwrap();

        assert!(result.is_none(), "rerouting, not replanning, handles a rate limit");
    }
}
