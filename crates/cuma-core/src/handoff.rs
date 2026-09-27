//! Structured handoff between agents.
//!
//! When agent A fails halfway through a task and agent B picks it up, B must
//! not have to re-read A's entire transcript. A handoff is a *compressed*,
//! structured summary: what was done, what is left, what was decided, what to
//! watch out for. It is the difference between a fallback that costs one
//! prompt and one that costs a whole conversation replay.

use crate::ids::{AgentId, TaskId};
use serde::{Deserialize, Serialize};

/// Everything the next agent needs, and nothing it does not.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentHandoff {
    /// The task being handed over.
    pub task_id: TaskId,
    /// What the task is, restated so the receiver needs no other context.
    pub task_description: String,
    /// The agent handing over.
    pub from_agent: AgentId,
    /// The agent receiving, once routing has chosen one.
    pub to_agent: Option<AgentId>,
    /// What the previous agent finished.
    pub completed_work: Vec<String>,
    /// What is still outstanding.
    pub remaining_work: Vec<String>,
    /// Decisions already made that the receiver must not relitigate.
    pub decisions: Vec<String>,
    /// Files the previous agent modified.
    pub changed_files: Vec<String>,
    /// Context worth carrying forward, already filtered.
    pub relevant_context: Vec<String>,
    /// Traps, dead ends and known-bad approaches.
    pub warnings: Vec<String>,
    /// Validation output (test results, build errors).
    pub validation_results: Vec<String>,
    /// Why the handoff is happening.
    pub reason: String,
}

impl AgentHandoff {
    /// An empty handoff for `task_id`, from `from_agent`, because of `reason`.
    pub fn new(
        task_id: TaskId,
        task_description: impl Into<String>,
        from_agent: AgentId,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            task_id,
            task_description: task_description.into(),
            from_agent,
            to_agent: None,
            completed_work: Vec::new(),
            remaining_work: Vec::new(),
            decisions: Vec::new(),
            changed_files: Vec::new(),
            relevant_context: Vec::new(),
            warnings: Vec::new(),
            validation_results: Vec::new(),
            reason: reason.into(),
        }
    }

    /// Name the receiving agent.
    #[must_use]
    pub fn to(mut self, agent: AgentId) -> Self {
        self.to_agent = Some(agent);
        self
    }

    /// Record completed work.
    #[must_use]
    pub fn completed(mut self, item: impl Into<String>) -> Self {
        self.completed_work.push(item.into());
        self
    }

    /// Record outstanding work.
    #[must_use]
    pub fn remaining(mut self, item: impl Into<String>) -> Self {
        self.remaining_work.push(item.into());
        self
    }

    /// Record a warning.
    #[must_use]
    pub fn warning(mut self, item: impl Into<String>) -> Self {
        self.warnings.push(item.into());
        self
    }

    /// Record a changed file.
    #[must_use]
    pub fn changed_file(mut self, path: impl Into<String>) -> Self {
        self.changed_files.push(path.into());
        self
    }

    /// Render the handoff as a prompt preamble for the receiving agent.
    ///
    /// Empty sections are omitted rather than emitted as headings with nothing
    /// under them: the whole point is to spend as few tokens as possible.
    pub fn to_prompt(&self) -> String {
        let mut out = String::new();
        out.push_str("## Handoff\n\n");
        out.push_str(&format!("Task: {}\n", self.task_description));
        out.push_str(&format!(
            "Previous agent: {} (handed over because: {})\n",
            self.from_agent, self.reason
        ));

        let sections: [(&str, &Vec<String>); 6] = [
            ("Already done", &self.completed_work),
            ("Still to do", &self.remaining_work),
            ("Decisions made (do not revisit)", &self.decisions),
            ("Files changed", &self.changed_files),
            ("Warnings", &self.warnings),
            ("Validation results", &self.validation_results),
        ];

        for (heading, items) in sections {
            if items.is_empty() {
                continue;
            }
            out.push_str(&format!("\n### {heading}\n"));
            for item in items {
                out.push_str(&format!("- {item}\n"));
            }
        }

        if !self.relevant_context.is_empty() {
            out.push_str("\n### Context\n");
            for item in &self.relevant_context {
                out.push_str(item);
                out.push('\n');
            }
        }

        out
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn handoff() -> AgentHandoff {
        AgentHandoff::new(
            TaskId::new("t1"),
            "implement OAuth",
            AgentId::new("codex"),
            "rate limited",
        )
    }

    #[test]
    fn the_prompt_states_the_task_and_why_the_handoff_happened() {
        let prompt = handoff().to_prompt();
        assert!(prompt.contains("implement OAuth"));
        assert!(prompt.contains("codex"));
        assert!(prompt.contains("rate limited"));
    }

    #[test]
    fn empty_sections_are_omitted_to_save_tokens() {
        let prompt = handoff().to_prompt();
        assert!(!prompt.contains("Already done"));
        assert!(!prompt.contains("Warnings"));
    }

    #[test]
    fn populated_sections_appear_with_their_items() {
        let prompt = handoff()
            .completed("wrote the token endpoint")
            .remaining("wire up refresh")
            .warning("the mock server rejects PKCE")
            .changed_file("src/auth.rs")
            .to_prompt();

        assert!(prompt.contains("### Already done"));
        assert!(prompt.contains("- wrote the token endpoint"));
        assert!(prompt.contains("### Still to do"));
        assert!(prompt.contains("- wire up refresh"));
        assert!(prompt.contains("### Warnings"));
        assert!(prompt.contains("### Files changed"));
        assert!(prompt.contains("- src/auth.rs"));
    }

    #[test]
    fn a_handoff_prompt_is_far_smaller_than_a_transcript_replay() {
        let prompt = handoff()
            .completed("wrote the token endpoint")
            .remaining("wire up refresh")
            .to_prompt();
        assert!(
            prompt.len() < 600,
            "handoffs must stay compact, got {} bytes",
            prompt.len()
        );
    }
}
