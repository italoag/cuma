//! Minimal context assembly.
//!
//! The goal is to maximize useful information per token sent. That means the
//! executing agent gets: the task, the outputs of the tasks it depends on, any
//! handoff, and nothing else. It specifically does not get the whole session
//! transcript, the whole plan, or the full text of every sibling task's output.

use async_trait::async_trait;
use cuma_core::error::Result;
use cuma_core::ports::{ContextManager, MemoryEntry};
use cuma_core::{AgentHandoff, Task, TaskGraph};

/// Characters per token, for budget estimation.
///
/// A rough English/code average. Precise tokenization would require a
/// tokenizer per model, and the budget only needs to be approximately right —
/// being 15% conservative costs far less than overflowing a context window.
const CHARS_PER_TOKEN: usize = 4;

/// Assembles the smallest useful prompt for a task.
#[derive(Debug, Clone, Default)]
pub struct MinimalContextManager {
    /// Memories to prepend, already filtered for relevance by the memory store.
    pub memories: Vec<MemoryEntry>,
    /// Characters of a dependency's output to carry forward.
    ///
    /// Dependency outputs are summaries, not transcripts; truncating them is
    /// the single biggest token saving available without a model call.
    pub max_dependency_chars: usize,
}

impl MinimalContextManager {
    /// A manager with sensible truncation and no memories.
    pub fn new() -> Self {
        Self {
            memories: Vec::new(),
            max_dependency_chars: 2_000,
        }
    }

    /// Attach recalled memories.
    #[must_use]
    pub fn with_memories(mut self, memories: Vec<MemoryEntry>) -> Self {
        self.memories = memories;
        self
    }

    /// Truncate `text` on a character boundary, marking that it was cut.
    fn truncate(text: &str, max_chars: usize) -> String {
        if text.chars().count() <= max_chars {
            return text.to_owned();
        }

        let kept: String = text.chars().take(max_chars).collect();
        format!(
            "{kept}\n[... truncated, {} chars omitted]",
            text.chars().count() - max_chars
        )
    }

    /// Approximate token count for a prompt.
    pub fn estimate_tokens(text: &str) -> u64 {
        (text.len() / CHARS_PER_TOKEN) as u64
    }
}

#[async_trait]
impl ContextManager for MinimalContextManager {
    async fn assemble(
        &self,
        task: &Task,
        graph: &TaskGraph,
        handoff: Option<&AgentHandoff>,
        token_budget: u64,
    ) -> Result<String> {
        let mut prompt = String::new();

        // A handoff goes first: it is the highest-value context there is, and
        // if anything gets dropped to fit the budget it must not be this.
        if let Some(handoff) = handoff {
            prompt.push_str(&handoff.to_prompt());
            prompt.push('\n');
        }

        if !self.memories.is_empty() {
            prompt.push_str("## Prior knowledge about this project\n\n");
            for memory in &self.memories {
                prompt.push_str(&format!("- ({}) {}\n", memory.kind, memory.content));
            }
            prompt.push('\n');
        }

        prompt.push_str("## Task\n\n");
        prompt.push_str(&task.spec.description);
        prompt.push_str("\n\n");

        // Only *completed dependencies* contribute. A sibling task running in
        // parallel has nothing this task needs, and including it would leak
        // tokens for no benefit.
        let dependency_outputs: Vec<(&str, &str)> = task
            .spec
            .dependencies
            .iter()
            .filter_map(|id| graph.get(id))
            .filter_map(|dep| {
                dep.successful_outcome()
                    .map(|outcome| (dep.spec.description.as_str(), outcome.output.as_str()))
            })
            .collect();

        if !dependency_outputs.is_empty() {
            prompt.push_str("## Results of prerequisite work\n\n");
            for (description, output) in dependency_outputs {
                prompt.push_str(&format!("### {description}\n"));
                prompt.push_str(&Self::truncate(output, self.max_dependency_chars));
                prompt.push_str("\n\n");
            }
        }

        // Previous failures on *this* task are worth carrying: they stop the
        // next agent walking into the same wall.
        let failures: Vec<&str> = task
            .attempts
            .iter()
            .filter(|a| !a.success)
            .filter_map(|a| a.failure_reason.as_deref())
            .collect();

        if !failures.is_empty() {
            prompt.push_str("## Previous attempts on this task failed\n\n");
            for reason in failures {
                prompt.push_str(&format!("- {reason}\n"));
            }
            prompt.push('\n');
        }

        // Enforce the budget by trimming the least valuable section rather
        // than failing: an over-budget prompt is a guaranteed ContextOverflow,
        // and a trimmed one usually still works.
        let budget_chars = (token_budget as usize).saturating_mul(CHARS_PER_TOKEN);
        if budget_chars > 0 && prompt.len() > budget_chars {
            tracing::warn!(
                task = %task.id,
                prompt_chars = prompt.len(),
                budget_chars,
                "assembled context exceeds the budget; truncating"
            );
            prompt = Self::truncate(&prompt, budget_chars);
        }

        Ok(prompt)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use cuma_core::{AgentId, AttemptId, ExecutionOutcome, TaskSpec, TaskType, TokenUsage};

    fn completed_task(description: &str, output: &str) -> Task {
        let mut task = Task::new(TaskSpec::new(description, TaskType::Inspection));
        task.status = cuma_core::TaskStatus::Completed;
        task.attempts.push(ExecutionOutcome {
            attempt_id: AttemptId::generate(),
            agent_id: AgentId::new("a"),
            model_id: None,
            success: true,
            output: output.to_owned(),
            changed_files: vec![],
            tokens: TokenUsage::reported(10, 10),
            latency_ms: 1,
            failure_class: None,
            failure_reason: None,
        });
        task
    }

    #[tokio::test]
    async fn the_prompt_always_states_the_task() {
        let graph = TaskGraph::new();
        let task = Task::new(TaskSpec::new("write the parser", TaskType::Implementation));

        let prompt = MinimalContextManager::new()
            .assemble(&task, &graph, None, 100_000)
            .await
            .unwrap();

        assert!(prompt.contains("## Task"));
        assert!(prompt.contains("write the parser"));
    }

    #[tokio::test]
    async fn only_completed_dependencies_contribute_their_output() {
        let mut graph = TaskGraph::new();
        let done = graph.add(completed_task("inspect", "the code uses axum"));
        let pending = graph.add(Task::new(TaskSpec::new("unrelated", TaskType::Research)));

        let task = Task::new(
            TaskSpec::new("implement", TaskType::Implementation).depends_on(done.clone()),
        );
        let _ = pending;

        let prompt = MinimalContextManager::new()
            .assemble(&task, &graph, None, 100_000)
            .await
            .unwrap();

        assert!(prompt.contains("the code uses axum"));
        assert!(
            !prompt.contains("unrelated"),
            "a non-dependency must not leak in"
        );
    }

    #[tokio::test]
    async fn a_dependency_that_has_not_finished_contributes_nothing() {
        let mut graph = TaskGraph::new();
        let unfinished = graph.add(Task::new(TaskSpec::new("inspect", TaskType::Inspection)));

        let task =
            Task::new(TaskSpec::new("implement", TaskType::Implementation).depends_on(unfinished));

        let prompt = MinimalContextManager::new()
            .assemble(&task, &graph, None, 100_000)
            .await
            .unwrap();

        assert!(!prompt.contains("prerequisite work"));
    }

    #[tokio::test]
    async fn long_dependency_output_is_truncated_with_a_marker() {
        let mut graph = TaskGraph::new();
        let huge = "x".repeat(50_000);
        let done = graph.add(completed_task("inspect", &huge));

        let task = Task::new(TaskSpec::new("implement", TaskType::Implementation).depends_on(done));

        let manager = MinimalContextManager {
            max_dependency_chars: 500,
            ..MinimalContextManager::new()
        };
        let prompt = manager
            .assemble(&task, &graph, None, 100_000)
            .await
            .unwrap();

        assert!(prompt.len() < 2_000, "got {} chars", prompt.len());
        assert!(prompt.contains("truncated"), "truncation must be visible");
    }

    #[tokio::test]
    async fn a_handoff_leads_the_prompt() {
        let graph = TaskGraph::new();
        let task = Task::new(TaskSpec::new("finish it", TaskType::Implementation));

        let handoff = AgentHandoff::new(
            task.id.clone(),
            "finish it",
            AgentId::new("codex"),
            "rate limited",
        )
        .completed("wrote the endpoint");

        let prompt = MinimalContextManager::new()
            .assemble(&task, &graph, Some(&handoff), 100_000)
            .await
            .unwrap();

        assert!(prompt.starts_with("## Handoff"));
        assert!(prompt.contains("wrote the endpoint"));
    }

    #[tokio::test]
    async fn previous_failures_on_this_task_are_carried_forward() {
        let graph = TaskGraph::new();
        let mut task = Task::new(TaskSpec::new("fix it", TaskType::BugFix));
        task.attempts.push(ExecutionOutcome {
            attempt_id: AttemptId::generate(),
            agent_id: AgentId::new("a"),
            model_id: None,
            success: false,
            output: String::new(),
            changed_files: vec![],
            tokens: TokenUsage::default(),
            latency_ms: 1,
            failure_class: Some(cuma_core::ErrorClass::TaskFailure),
            failure_reason: Some("the mock server rejects PKCE".into()),
        });

        let prompt = MinimalContextManager::new()
            .assemble(&task, &graph, None, 100_000)
            .await
            .unwrap();

        assert!(prompt.contains("Previous attempts"));
        assert!(
            prompt.contains("PKCE"),
            "so the next agent does not repeat it"
        );
    }

    #[tokio::test]
    async fn the_prompt_is_trimmed_to_the_token_budget() {
        let mut graph = TaskGraph::new();
        let done = graph.add(completed_task("inspect", &"y".repeat(100_000)));
        let task = Task::new(TaskSpec::new("implement", TaskType::Implementation).depends_on(done));

        let manager = MinimalContextManager {
            max_dependency_chars: 100_000,
            ..MinimalContextManager::new()
        };

        // 1000 tokens ~ 4000 characters.
        let prompt = manager.assemble(&task, &graph, None, 1_000).await.unwrap();
        assert!(
            MinimalContextManager::estimate_tokens(&prompt) <= 1_100,
            "prompt was {} tokens",
            MinimalContextManager::estimate_tokens(&prompt)
        );
    }

    #[tokio::test]
    async fn memories_appear_when_supplied_and_are_absent_otherwise() {
        let graph = TaskGraph::new();
        let task = Task::new(TaskSpec::new("do it", TaskType::General));

        let bare = MinimalContextManager::new()
            .assemble(&task, &graph, None, 100_000)
            .await
            .unwrap();
        assert!(!bare.contains("Prior knowledge"));

        let with_memory = MinimalContextManager::new()
            .with_memories(vec![MemoryEntry {
                id: "1".into(),
                content: "this project uses tabs".into(),
                kind: "convention".into(),
                relevance: Some(0.9),
                created_at: chrono::Utc::now(),
            }])
            .assemble(&task, &graph, None, 100_000)
            .await
            .unwrap();

        assert!(with_memory.contains("this project uses tabs"));
    }
}
