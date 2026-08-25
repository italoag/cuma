//! The TUI's view model.
//!
//! Deliberately separate from rendering, and deliberately driven only by
//! [`Event`]s. That means the whole interface can be tested by feeding it an
//! event sequence and asserting on what it would show — no terminal, no
//! orchestrator, no timing.

use cuma_core::{AgentId, Event, EventKind, ModelId, TaskId, TaskStatus, TokenUsage};
use std::collections::BTreeMap;

/// Which screen is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Screen {
    /// The conversation and live plan.
    #[default]
    Chat,
    /// The task DAG.
    Tasks,
    /// Registered agents and their health.
    Agents,
    /// Models and pricing.
    Models,
    /// Installed skills.
    Skills,
    /// Recalled memories.
    Memory,
    /// Token and cost statistics.
    Usage,
    /// The raw event log.
    Logs,
    /// Effective configuration.
    Configuration,
}

impl Screen {
    /// Every screen, in tab order.
    pub const ALL: [Screen; 9] = [
        Screen::Chat,
        Screen::Tasks,
        Screen::Agents,
        Screen::Models,
        Screen::Skills,
        Screen::Memory,
        Screen::Usage,
        Screen::Logs,
        Screen::Configuration,
    ];

    /// The tab label.
    pub fn title(self) -> &'static str {
        match self {
            Self::Chat => "Chat",
            Self::Tasks => "Tasks",
            Self::Agents => "Agents",
            Self::Models => "Models",
            Self::Skills => "Skills",
            Self::Memory => "Memory",
            Self::Usage => "Usage",
            Self::Logs => "Logs",
            Self::Configuration => "Config",
        }
    }

    /// The next screen, wrapping.
    pub fn next(self) -> Self {
        let index = Self::ALL.iter().position(|s| *s == self).unwrap_or(0);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }

    /// The previous screen, wrapping.
    pub fn previous(self) -> Self {
        let index = Self::ALL.iter().position(|s| *s == self).unwrap_or(0);
        Self::ALL[(index + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

/// One task as the interface sees it.
#[derive(Debug, Clone)]
pub struct TaskRow {
    /// The task.
    pub id: TaskId,
    /// What it is.
    pub description: String,
    /// Where it is.
    pub status: TaskStatus,
    /// Who is running it.
    pub agent: Option<AgentId>,
    /// On which model.
    pub model: Option<ModelId>,
}

impl TaskRow {
    /// The status marker shown in the plan list.
    pub fn marker(&self) -> &'static str {
        match self.status {
            TaskStatus::Completed => "✓",
            TaskStatus::Running => "●",
            TaskStatus::Failed => "✗",
            TaskStatus::Skipped => "⊘",
            TaskStatus::Cancelled => "⊗",
            TaskStatus::Pending | TaskStatus::Ready => "○",
        }
    }
}

/// How many log lines are retained.
///
/// Bounded so a long session cannot grow the interface's memory without limit;
/// the full record lives in the runtime database.
const MAX_LOG_LINES: usize = 500;

/// Everything the interface displays.
#[derive(Debug, Clone, Default)]
pub struct AppState {
    /// The active screen.
    pub screen: Screen,
    /// The user's goal.
    pub goal: String,
    /// Whether a session is running.
    pub running: bool,
    /// The plan.
    pub tasks: Vec<TaskRow>,
    /// The agent currently executing.
    pub current_agent: Option<AgentId>,
    /// The model currently executing.
    pub current_model: Option<ModelId>,
    /// The most recent routing explanation.
    pub last_explanation: Option<String>,
    /// Streamed assistant output.
    pub transcript: String,
    /// Tokens consumed this session.
    pub tokens: TokenUsage,
    /// USD spent, counting only priced attempts.
    pub spent_usd: f64,
    /// Attempts whose pricing was unknown.
    pub unpriced_attempts: u32,
    /// Circuit-breaker state per agent.
    pub agent_health: BTreeMap<AgentId, String>,
    /// Recent events, newest last.
    pub logs: Vec<String>,
    /// Whether the session finished successfully.
    pub finished: Option<bool>,
}

impl AppState {
    /// A fresh state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one event into the view model.
    ///
    /// Every branch is total: an event the interface does not display still
    /// reaches the log, so a user debugging a session sees everything that
    /// happened rather than only what has a widget.
    pub fn apply(&mut self, event: &Event) {
        match &event.kind {
            EventKind::SessionStarted { goal } => {
                self.goal = goal.clone();
                self.running = true;
                self.finished = None;
                self.tasks.clear();
                self.transcript.clear();
                self.log(format!("session started: {goal}"));
            }

            EventKind::SessionCompleted { success } => {
                self.running = false;
                self.finished = Some(*success);
                self.current_agent = None;
                self.current_model = None;
                self.log(format!(
                    "session {}",
                    if *success { "completed" } else { "failed" }
                ));
            }

            EventKind::TaskPlanned { task_count } => {
                self.log(format!("planned {task_count} tasks"));
            }

            EventKind::TaskCreated { description } => {
                if let Some(id) = &event.task_id {
                    self.tasks.push(TaskRow {
                        id: id.clone(),
                        description: description.clone(),
                        status: TaskStatus::Pending,
                        agent: None,
                        model: None,
                    });
                }
            }

            EventKind::TaskStatusChanged { status } => {
                self.update_task(event.task_id.as_ref(), |task| task.status = *status);
            }

            EventKind::TaskCompleted { tokens } => {
                self.tokens = self.tokens.merge(*tokens);
                self.update_task(event.task_id.as_ref(), |task| {
                    task.status = TaskStatus::Completed;
                });
            }

            EventKind::TaskFailed { reason } => {
                self.update_task(event.task_id.as_ref(), |task| {
                    task.status = TaskStatus::Failed;
                });
                self.log(format!("task failed: {reason}"));
            }

            EventKind::TaskSkipped { blocked_by } => {
                self.update_task(event.task_id.as_ref(), |task| {
                    task.status = TaskStatus::Skipped;
                });
                self.log(format!("task skipped, blocked by {blocked_by}"));
            }

            EventKind::AgentSelected {
                agent,
                model,
                score,
                explanation,
            } => {
                self.current_agent = Some(agent.clone());
                self.current_model = model.clone();
                self.last_explanation = Some(explanation.clone());

                let agent = agent.clone();
                let model = model.clone();
                self.update_task(event.task_id.as_ref(), move |task| {
                    task.agent = Some(agent.clone());
                    task.model = model.clone();
                });

                self.log(format!(
                    "routed to {} (score {score:.3})",
                    self.current_agent
                        .as_ref()
                        .map_or_else(String::new, ToString::to_string)
                ));
            }

            EventKind::RoutingFailed { reason } => {
                self.log(format!("routing failed: {reason}"));
            }

            EventKind::AgentStarted { agent } => {
                self.update_task(event.task_id.as_ref(), |task| {
                    task.status = TaskStatus::Running;
                });
                self.log(format!("{agent} started"));
            }

            EventKind::AgentOutputReceived { chunk } => {
                self.transcript.push_str(chunk);
            }

            EventKind::AgentFailed {
                agent,
                class,
                message,
            } => {
                self.log(format!("{agent} failed ({class:?}): {message}"));
            }

            EventKind::RetryScheduled {
                attempt, delay_ms, ..
            } => {
                self.log(format!("retry {attempt} scheduled in {delay_ms}ms"));
            }

            EventKind::FallbackSelected { from, reason, .. } => {
                self.log(format!("falling back from {from}: {reason}"));
            }

            EventKind::CircuitBreakerChanged { agent, state } => {
                self.agent_health.insert(agent.clone(), state.clone());
                self.log(format!("{agent} circuit breaker is {state}"));
            }

            EventKind::HandoffPerformed { from, to } => {
                self.log(format!("handed off from {from} to {to}"));
            }

            EventKind::SkillInstalled { skill, trust } => {
                self.log(format!("installed skill {skill} ({trust})"));
            }

            EventKind::SkillRejected { skill, reason } => {
                self.log(format!("refused skill {skill}: {reason}"));
            }

            EventKind::UsageRecorded {
                tokens,
                estimated_cost_usd,
            } => {
                self.tokens = self.tokens.merge(*tokens);
                match estimated_cost_usd {
                    Some(cost) => self.spent_usd += cost,
                    // Counting an unpriced attempt as $0 would make the header
                    // read as though the session were cheaper than it was.
                    None => self.unpriced_attempts += 1,
                }
            }
        }
    }

    fn update_task(&mut self, id: Option<&TaskId>, update: impl Fn(&mut TaskRow)) {
        let Some(id) = id else {
            return;
        };
        if let Some(task) = self.tasks.iter_mut().find(|t| &t.id == id) {
            update(task);
        }
    }

    fn log(&mut self, line: String) {
        self.logs.push(line);
        if self.logs.len() > MAX_LOG_LINES {
            // Drop the oldest lines; the database keeps the full record.
            let excess = self.logs.len() - MAX_LOG_LINES;
            self.logs.drain(0..excess);
        }
    }

    /// The cost, formatted so an incomplete total cannot be read as a complete one.
    pub fn render_cost(&self) -> String {
        if self.unpriced_attempts > 0 {
            format!(
                "≥${:.4} ({} unpriced)",
                self.spent_usd, self.unpriced_attempts
            )
        } else {
            format!("~${:.4}", self.spent_usd)
        }
    }

    /// How many tasks have completed.
    pub fn completed_count(&self) -> usize {
        self.tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Completed)
            .count()
    }

    /// Move to the next screen.
    pub fn next_screen(&mut self) {
        self.screen = self.screen.next();
    }

    /// Move to the previous screen.
    pub fn previous_screen(&mut self) {
        self.screen = self.screen.previous();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use cuma_core::{ErrorClass, SessionId};

    fn session() -> SessionId {
        SessionId::new("s1")
    }

    fn apply(state: &mut AppState, kind: EventKind) {
        state.apply(&Event::session(session(), kind));
    }

    fn apply_to_task(state: &mut AppState, task: &TaskId, kind: EventKind) {
        state.apply(&Event::task(session(), task.clone(), kind));
    }

    #[test]
    fn screens_cycle_in_both_directions() {
        let mut state = AppState::new();
        assert_eq!(state.screen, Screen::Chat);

        state.next_screen();
        assert_eq!(state.screen, Screen::Tasks);

        state.previous_screen();
        assert_eq!(state.screen, Screen::Chat);

        state.previous_screen();
        assert_eq!(state.screen, Screen::Configuration, "wraps around");
    }

    #[test]
    fn every_screen_has_a_title() {
        for screen in Screen::ALL {
            assert!(!screen.title().is_empty());
        }
    }

    #[test]
    fn a_session_start_resets_the_previous_session() {
        let mut state = AppState::new();
        state.transcript.push_str("old output");
        state.finished = Some(false);

        apply(
            &mut state,
            EventKind::SessionStarted {
                goal: "new goal".into(),
            },
        );

        assert_eq!(state.goal, "new goal");
        assert!(state.running);
        assert!(state.transcript.is_empty());
        assert_eq!(state.finished, None);
    }

    #[test]
    fn a_task_moves_through_its_lifecycle() {
        let mut state = AppState::new();
        let task = TaskId::new("t1");

        apply_to_task(
            &mut state,
            &task,
            EventKind::TaskCreated {
                description: "do it".into(),
            },
        );
        assert_eq!(state.tasks[0].status, TaskStatus::Pending);
        assert_eq!(state.tasks[0].marker(), "○");

        apply_to_task(
            &mut state,
            &task,
            EventKind::AgentStarted {
                agent: AgentId::new("claude"),
            },
        );
        assert_eq!(state.tasks[0].status, TaskStatus::Running);
        assert_eq!(state.tasks[0].marker(), "●");

        apply_to_task(
            &mut state,
            &task,
            EventKind::TaskCompleted {
                tokens: TokenUsage::reported(100, 50),
            },
        );
        assert_eq!(state.tasks[0].status, TaskStatus::Completed);
        assert_eq!(state.tasks[0].marker(), "✓");
        assert_eq!(state.completed_count(), 1);
    }

    #[test]
    fn routing_records_the_agent_the_model_and_the_explanation() {
        let mut state = AppState::new();
        let task = TaskId::new("t1");

        apply_to_task(
            &mut state,
            &task,
            EventKind::TaskCreated {
                description: "do it".into(),
            },
        );
        apply_to_task(
            &mut state,
            &task,
            EventKind::AgentSelected {
                agent: AgentId::new("claude"),
                model: Some(ModelId::new("sonnet")),
                score: 0.87,
                explanation: "Selected: claude\nReasons: ...".into(),
            },
        );

        assert_eq!(state.current_agent, Some(AgentId::new("claude")));
        assert_eq!(state.tasks[0].agent, Some(AgentId::new("claude")));
        assert_eq!(state.tasks[0].model, Some(ModelId::new("sonnet")));
        assert!(state.last_explanation.unwrap().contains("Selected"));
    }

    #[test]
    fn streamed_output_accumulates_into_the_transcript() {
        let mut state = AppState::new();
        for chunk in ["Hello", ", ", "world"] {
            apply(
                &mut state,
                EventKind::AgentOutputReceived {
                    chunk: chunk.into(),
                },
            );
        }
        assert_eq!(state.transcript, "Hello, world");
    }

    #[test]
    fn an_unpriced_attempt_is_counted_rather_than_treated_as_free() {
        let mut state = AppState::new();

        apply(
            &mut state,
            EventKind::UsageRecorded {
                tokens: TokenUsage::reported(100, 50),
                estimated_cost_usd: Some(0.25),
            },
        );
        assert_eq!(state.render_cost(), "~$0.2500");

        apply(
            &mut state,
            EventKind::UsageRecorded {
                tokens: TokenUsage::reported(100, 50),
                estimated_cost_usd: None,
            },
        );

        let rendered = state.render_cost();
        assert!(rendered.starts_with('≥'), "got {rendered}");
        assert!(rendered.contains("1 unpriced"));
    }

    #[test]
    fn failures_and_fallbacks_reach_the_log() {
        let mut state = AppState::new();

        apply(
            &mut state,
            EventKind::AgentFailed {
                agent: AgentId::new("codex"),
                class: ErrorClass::RateLimit,
                message: "429".into(),
            },
        );
        apply(
            &mut state,
            EventKind::FallbackSelected {
                from: AgentId::new("codex"),
                to: AgentId::new("claude"),
                reason: "rate limited".into(),
            },
        );

        assert!(state.logs.iter().any(|l| l.contains("RateLimit")));
        assert!(state.logs.iter().any(|l| l.contains("falling back")));
    }

    #[test]
    fn breaker_state_is_tracked_per_agent() {
        let mut state = AppState::new();
        apply(
            &mut state,
            EventKind::CircuitBreakerChanged {
                agent: AgentId::new("flaky"),
                state: "Open".into(),
            },
        );

        assert_eq!(
            state
                .agent_health
                .get(&AgentId::new("flaky"))
                .map(String::as_str),
            Some("Open")
        );
    }

    #[test]
    fn the_log_is_bounded_so_a_long_session_cannot_grow_without_limit() {
        let mut state = AppState::new();

        for i in 0..(MAX_LOG_LINES + 250) {
            apply(
                &mut state,
                EventKind::TaskFailed {
                    reason: format!("failure {i}"),
                },
            );
        }

        assert_eq!(state.logs.len(), MAX_LOG_LINES);
        assert!(
            state
                .logs
                .last()
                .unwrap()
                .contains(&(MAX_LOG_LINES + 249).to_string()),
            "the newest lines must be the ones kept"
        );
    }

    #[test]
    fn an_event_for_an_unknown_task_is_ignored_rather_than_panicking() {
        let mut state = AppState::new();
        apply_to_task(
            &mut state,
            &TaskId::new("never-created"),
            EventKind::TaskCompleted {
                tokens: TokenUsage::default(),
            },
        );
        assert!(state.tasks.is_empty());
    }

    #[test]
    fn a_completed_session_stops_showing_a_current_agent() {
        let mut state = AppState::new();
        state.current_agent = Some(AgentId::new("claude"));

        apply(&mut state, EventKind::SessionCompleted { success: true });

        assert!(!state.running);
        assert_eq!(state.finished, Some(true));
        assert!(state.current_agent.is_none());
    }
}
