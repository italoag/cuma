//! Rendering.
//!
//! Rendering is a pure function of [`AppState`]: nothing here reads the
//! orchestrator, the clock or the filesystem. That is what makes the layout
//! testable and what will let a different front end reuse the same state.

use crate::state::AppState;

/// Render the status line shown above every screen.
pub fn status_line(state: &AppState) -> String {
    let connection = if state.running {
        "running"
    } else if state.finished.is_some() {
        "finished"
    } else {
        "idle"
    };

    format!(
        "Meta Agent  |  {}/{} tasks  |  {} tokens  |  {}  |  {connection}",
        state.completed_count(),
        state.tasks.len(),
        state.tokens.total(),
        state.render_cost(),
    )
}

/// Render the plan as the chat screen shows it.
pub fn plan_lines(state: &AppState) -> Vec<String> {
    state
        .tasks
        .iter()
        .map(|task| format!("{} {}", task.marker(), task.description))
        .collect()
}

/// Render the current-execution panel.
pub fn execution_lines(state: &AppState) -> Vec<String> {
    let mut lines = Vec::new();

    match &state.current_agent {
        Some(agent) => lines.push(format!("Agent: {agent}")),
        None => lines.push("Agent: -".to_owned()),
    }

    match &state.current_model {
        Some(model) => lines.push(format!("Model: {model}")),
        None => lines.push("Model: -".to_owned()),
    }

    if let Some(task) = state
        .tasks
        .iter()
        .find(|t| t.status == cuma_core::TaskStatus::Running)
    {
        lines.push(format!("Task:  {}", task.description));
    }

    lines
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::state::TaskRow;
    use cuma_core::{AgentId, TaskId, TaskStatus};

    fn state_with_tasks() -> AppState {
        let mut state = AppState::new();
        state.tasks = vec![
            TaskRow {
                id: TaskId::new("t1"),
                description: "inspect repository".into(),
                status: TaskStatus::Completed,
                agent: Some(AgentId::new("claude")),
                model: None,
            },
            TaskRow {
                id: TaskId::new("t2"),
                description: "design solution".into(),
                status: TaskStatus::Running,
                agent: Some(AgentId::new("claude")),
                model: None,
            },
            TaskRow {
                id: TaskId::new("t3"),
                description: "implementation".into(),
                status: TaskStatus::Pending,
                agent: None,
                model: None,
            },
        ];
        state
    }

    #[test]
    fn the_status_line_reports_progress_and_spend() {
        let mut state = state_with_tasks();
        state.running = true;

        let line = status_line(&state);
        assert!(line.contains("1/3 tasks"));
        assert!(line.contains("running"));
        assert!(line.contains('$'));
    }

    #[test]
    fn the_status_line_distinguishes_idle_from_finished() {
        let mut state = AppState::new();
        assert!(status_line(&state).contains("idle"));

        state.finished = Some(true);
        assert!(status_line(&state).contains("finished"));
    }

    #[test]
    fn the_plan_shows_a_marker_per_task_state() {
        let lines = plan_lines(&state_with_tasks());

        assert!(lines[0].starts_with('✓'));
        assert!(lines[1].starts_with('●'));
        assert!(lines[2].starts_with('○'));
    }

    #[test]
    fn the_execution_panel_names_the_running_task() {
        let mut state = state_with_tasks();
        state.current_agent = Some(AgentId::new("claude"));

        let lines = execution_lines(&state);
        assert!(lines.iter().any(|l| l.contains("claude")));
        assert!(lines.iter().any(|l| l.contains("design solution")));
    }

    #[test]
    fn an_idle_execution_panel_renders_placeholders_rather_than_nothing() {
        let lines = execution_lines(&AppState::new());
        assert!(lines.iter().any(|l| l == "Agent: -"));
        assert!(lines.iter().any(|l| l == "Model: -"));
    }
}
