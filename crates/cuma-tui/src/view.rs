//! Rendering.
//!
//! Rendering is a pure function of [`AppState`]: nothing here reads the
//! orchestrator, the clock or the filesystem. That is what makes the layout
//! testable and what will let a different front end reuse the same state.
//!
//! The plain-text helpers ([`status_line`], [`plan_lines`], [`execution_lines`])
//! are the tested core; the ratatui widgets below are a thin dressing over
//! them, so a layout change cannot silently change what is displayed.

use crate::state::{AppState, InputMode, Screen};
use cuma_core::TaskStatus;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Row, Table, Tabs, Wrap};

// ---------------------------------------------------------------------------
// Plain-text projections — the tested core
// ---------------------------------------------------------------------------

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

    if let Some(task) = state.tasks.iter().find(|t| t.status == TaskStatus::Running) {
        lines.push(format!("Task:  {}", task.description));
    }

    lines
}

/// The key hints shown at the bottom, which depend on the input mode.
pub fn help_line(state: &AppState) -> String {
    match state.input_mode {
        InputMode::Editing => "Enter submit  ·  Esc cancel".to_owned(),
        InputMode::Navigating => {
            "Tab/Shift-Tab screens  ·  1-9 jump  ·  i new goal  ·  ↑↓ scroll  ·  q quit".to_owned()
        }
    }
}

// ---------------------------------------------------------------------------
// Widgets
// ---------------------------------------------------------------------------

/// Colour for a task's status marker.
fn status_style(status: TaskStatus) -> Style {
    match status {
        TaskStatus::Completed => Style::default().fg(Color::Green),
        TaskStatus::Running => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        TaskStatus::Failed => Style::default().fg(Color::Red),
        TaskStatus::Skipped | TaskStatus::Cancelled => Style::default().fg(Color::DarkGray),
        TaskStatus::Pending | TaskStatus::Ready => Style::default().fg(Color::Gray),
    }
}

/// Draw the whole interface.
pub fn draw(frame: &mut Frame<'_>, state: &AppState) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // tabs
            Constraint::Length(1), // status
            Constraint::Min(3),    // body
            Constraint::Length(3), // input
            Constraint::Length(1), // help
        ])
        .split(frame.area());

    draw_tabs(frame, areas[0], state);
    draw_status(frame, areas[1], state);
    draw_body(frame, areas[2], state);
    draw_input(frame, areas[3], state);
    draw_help(frame, areas[4], state);
}

fn draw_tabs(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let titles: Vec<Line<'_>> = Screen::ALL
        .iter()
        .enumerate()
        .map(|(index, screen)| {
            Line::from(vec![
                Span::styled(
                    format!("{}", index + 1),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(":"),
                Span::raw(screen.title()),
            ])
        })
        .collect();

    let selected = Screen::ALL
        .iter()
        .position(|s| *s == state.screen)
        .unwrap_or(0);

    frame.render_widget(
        Tabs::new(titles)
            .select(selected)
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .divider(" "),
        area,
    );
}

fn draw_status(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let colour = match (state.running, state.finished) {
        (true, _) => Color::Cyan,
        (false, Some(true)) => Color::Green,
        (false, Some(false)) => Color::Red,
        (false, None) => Color::Gray,
    };

    let mut spans = vec![Span::styled(
        status_line(state),
        Style::default().fg(colour),
    )];

    if let Some(notice) = &state.notice {
        spans.push(Span::raw("  |  "));
        spans.push(Span::styled(
            notice.clone(),
            Style::default().fg(Color::Yellow),
        ));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_body(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    match state.screen {
        Screen::Chat => draw_chat(frame, area, state),
        Screen::Tasks => draw_tasks(frame, area, state),
        Screen::Agents => draw_agents(frame, area, state),
        Screen::Models => draw_models(frame, area, state),
        Screen::Skills => {
            draw_placeholder(frame, area, state, "Skills", "cuma skills search <query>")
        }
        Screen::Memory => {
            draw_placeholder(frame, area, state, "Memory", "cuma memory search <query>")
        }
        Screen::Usage => draw_usage(frame, area, state),
        Screen::Logs => draw_logs(frame, area, state),
        Screen::Configuration => draw_configuration(frame, area, state),
    }
}

fn draw_chat(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(area);

    // Left: the goal and the agent's streamed output.
    let mut lines: Vec<Line<'_>> = Vec::new();
    if !state.goal.is_empty() {
        lines.push(Line::from(Span::styled(
            "You",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(format!("> {}", state.goal)));
        lines.push(Line::from(""));
    }

    if !state.transcript.is_empty() {
        lines.push(Line::from(Span::styled(
            "Agent",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        for line in state.transcript.lines() {
            lines.push(Line::from(line.to_owned()));
        }
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "Press i to enter a goal.",
            Style::default().fg(Color::DarkGray),
        )));
    }

    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Conversation "),
            )
            .wrap(Wrap { trim: false })
            .scroll((state.scroll, 0)),
        columns[0],
    );

    // Right: the plan above, the current execution below.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(7)])
        .split(columns[1]);

    let plan: Vec<Line<'_>> = state
        .tasks
        .iter()
        .map(|task| {
            Line::from(vec![
                Span::styled(task.marker().to_owned(), status_style(task.status)),
                Span::raw(" "),
                Span::raw(task.description.clone()),
            ])
        })
        .collect();

    frame.render_widget(
        Paragraph::new(if plan.is_empty() {
            vec![Line::from(Span::styled(
                "No plan yet.",
                Style::default().fg(Color::DarkGray),
            ))]
        } else {
            plan
        })
        .block(Block::default().borders(Borders::ALL).title(" Plan "))
        .wrap(Wrap { trim: true }),
        rows[0],
    );

    let execution: Vec<Line<'_>> = execution_lines(state).into_iter().map(Line::from).collect();

    frame.render_widget(
        Paragraph::new(execution)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Current execution "),
            )
            .wrap(Wrap { trim: true }),
        rows[1],
    );
}

fn draw_tasks(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let rows: Vec<Row<'_>> = state
        .tasks
        .iter()
        .map(|task| {
            Row::new(vec![
                Span::styled(task.marker().to_owned(), status_style(task.status)),
                Span::raw(format!("{:?}", task.status)),
                Span::raw(
                    task.agent
                        .as_ref()
                        .map_or_else(|| "-".to_owned(), ToString::to_string),
                ),
                Span::raw(
                    task.model
                        .as_ref()
                        .map_or_else(|| "-".to_owned(), ToString::to_string),
                ),
                Span::raw(task.description.clone()),
            ])
        })
        .collect();

    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(2),
                Constraint::Length(10),
                Constraint::Length(16),
                Constraint::Length(16),
                Constraint::Min(20),
            ],
        )
        .header(
            Row::new(vec!["", "Status", "Agent", "Model", "Task"])
                .style(Style::default().add_modifier(Modifier::BOLD)),
        )
        .block(Block::default().borders(Borders::ALL).title(" Tasks ")),
        area,
    );
}

fn draw_agents(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let rows: Vec<Row<'_>> = state
        .agents
        .iter()
        .map(|agent| {
            let health = state
                .agent_health
                .get(&agent.id)
                .cloned()
                .unwrap_or_else(|| format!("{:?}", agent.health.state));

            let colour = if agent.is_routable() {
                Color::Green
            } else {
                Color::Red
            };

            Row::new(vec![
                Span::styled(agent.id.to_string(), Style::default().fg(colour)),
                Span::raw(format!("{:?}", agent.protocol)),
                Span::raw(health),
                Span::raw(agent.models.len().to_string()),
                Span::raw(agent.capabilities.len().to_string()),
            ])
        })
        .collect();

    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(20),
                Constraint::Length(10),
                Constraint::Length(14),
                Constraint::Length(8),
                Constraint::Min(6),
            ],
        )
        .header(
            Row::new(vec![
                "Agent",
                "Protocol",
                "Health",
                "Models",
                "Capabilities",
            ])
            .style(Style::default().add_modifier(Modifier::BOLD)),
        )
        .block(Block::default().borders(Borders::ALL).title(" Agents ")),
        area,
    );
}

fn draw_models(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let render_price = |price: cuma_core::Known<f64>| {
        price
            .value()
            .map_or_else(|| "-".to_owned(), |p| format!("{p:.2}"))
    };

    let rows: Vec<Row<'_>> = state
        .agents
        .iter()
        .flat_map(|agent| agent.models.iter())
        .map(|model| {
            Row::new(vec![
                Span::raw(model.id.to_string()),
                Span::raw(model.agent_id.to_string()),
                Span::raw(
                    model
                        .context_window
                        .value()
                        .map_or_else(|| "-".to_owned(), |w| format!("{}K", w / 1000)),
                ),
                Span::raw(render_price(model.cost.input_per_mtok)),
                Span::raw(render_price(model.cost.output_per_mtok)),
            ])
        })
        .collect();

    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(24),
                Constraint::Length(18),
                Constraint::Length(10),
                Constraint::Length(14),
                Constraint::Min(14),
            ],
        )
        .header(
            Row::new(vec!["Model", "Agent", "Context", "In $/Mtok", "Out $/Mtok"])
                .style(Style::default().add_modifier(Modifier::BOLD)),
        )
        .block(Block::default().borders(Borders::ALL).title(" Models ")),
        area,
    );
}

fn draw_usage(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let mut lines = vec![
        Line::from(format!("Tokens in:     {}", state.tokens.input)),
        Line::from(format!("Tokens out:    {}", state.tokens.output)),
        Line::from(format!("Tokens cached: {}", state.tokens.cached)),
        Line::from(""),
        Line::from(format!("Cost:          {}", state.render_cost())),
        Line::from(""),
        Line::from(format!(
            "Tasks:         {} of {} complete",
            state.completed_count(),
            state.tasks.len()
        )),
    ];

    if !state.tokens.reported {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Some token counts were estimated, not reported by the agent.",
            Style::default().fg(Color::Yellow),
        )));
    }

    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(" Usage "))
            .scroll((state.scroll, 0)),
        area,
    );
}

fn draw_logs(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let lines: Vec<Line<'_>> = state
        .logs
        .iter()
        .map(|line| {
            let colour = if line.contains("failed") || line.contains("refused") {
                Color::Red
            } else if line.contains("retry") || line.contains("falling back") {
                Color::Yellow
            } else {
                Color::Gray
            };
            Line::from(Span::styled(line.clone(), Style::default().fg(colour)))
        })
        .collect();

    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(" Logs "))
            .wrap(Wrap { trim: true })
            .scroll((state.scroll, 0)),
        area,
    );
}

fn draw_configuration(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let lines: Vec<Line<'_>> = state
        .last_explanation
        .as_deref()
        .unwrap_or("No routing decision has been made yet.")
        .lines()
        .map(|line| Line::from(line.to_owned()))
        .collect();

    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Last routing decision "),
            )
            .wrap(Wrap { trim: false })
            .scroll((state.scroll, 0)),
        area,
    );
}

fn draw_placeholder(
    frame: &mut Frame<'_>,
    area: Rect,
    _state: &AppState,
    title: &str,
    command: &str,
) {
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                format!("{title} are not browsable from the TUI yet."),
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(""),
            Line::from(format!("Use: {command}")),
        ])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {title} ")),
        ),
        area,
    );
}

fn draw_input(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let (title, content, style) = match state.input_mode {
        InputMode::Editing => (
            " Goal (Enter to run, Esc to cancel) ",
            format!("> {}_", state.input),
            Style::default().fg(Color::Yellow),
        ),
        InputMode::Navigating => (
            " Goal ",
            if state.running {
                "running…".to_owned()
            } else {
                "press i to enter a goal".to_owned()
            },
            Style::default().fg(Color::DarkGray),
        ),
    };

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(content, style)))
            .block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
}

fn draw_help(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            help_line(state),
            Style::default().fg(Color::DarkGray),
        ))),
        area,
    );
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::state::TaskRow;
    use cuma_core::{AgentId, TaskId};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

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

    /// Draw into an off-screen backend and return the rendered text.
    fn render(state: &AppState) -> String {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal.draw(|frame| draw(frame, state)).expect("draw");

        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    // --- plain-text projections ------------------------------------------

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

    #[test]
    fn the_help_line_changes_with_the_input_mode() {
        let mut state = AppState::new();
        assert!(help_line(&state).contains("quit"));

        state.input_mode = InputMode::Editing;
        assert!(help_line(&state).contains("Enter submit"));
        assert!(
            !help_line(&state).contains("quit"),
            "q is a literal q while typing"
        );
    }

    // --- actual rendering -------------------------------------------------

    #[test]
    fn every_screen_renders_without_panicking() {
        // A panic in `draw` takes the terminal down with the raw mode still on,
        // which leaves the user's shell unusable. Cheap insurance.
        let mut state = state_with_tasks();
        state.goal = "implement OAuth".into();
        state.transcript = "working on it".into();
        state.logs = vec!["something failed".into(), "retry scheduled".into()];
        state.last_explanation = Some("Selected:\n  Agent: claude".into());

        for screen in Screen::ALL {
            state.go_to(screen);
            let rendered = render(&state);
            assert!(!rendered.is_empty(), "{screen:?} rendered nothing");
        }
    }

    #[test]
    fn a_default_state_renders_without_panicking() {
        assert!(!render(&AppState::new()).is_empty());
    }

    #[test]
    fn the_chat_screen_shows_the_goal_and_the_plan() {
        let mut state = state_with_tasks();
        state.goal = "implement OAuth".into();

        let rendered = render(&state);
        assert!(rendered.contains("implement OAuth"));
        assert!(rendered.contains("inspect repository"));
    }

    #[test]
    fn the_tab_bar_lists_every_screen() {
        let rendered = render(&AppState::new());
        for screen in Screen::ALL {
            assert!(
                rendered.contains(screen.title()),
                "no {} tab",
                screen.title()
            );
        }
    }

    #[test]
    fn editing_mode_shows_the_buffer_being_typed() {
        let mut state = AppState::new();
        state.begin_editing();
        for character in "fix the bug".chars() {
            state.push_char(character);
        }

        let rendered = render(&state);
        assert!(rendered.contains("fix the bug"));
        assert!(rendered.contains("Esc"));
    }

    #[test]
    fn a_notice_is_surfaced_in_the_status_bar() {
        let mut state = AppState::new();
        state.running = true;
        state.begin_editing();

        assert!(render(&state).contains("already running"));
    }

    #[test]
    fn the_agents_screen_lists_registered_agents() {
        let mut state = AppState::new();
        state.set_agents(vec![cuma_core::AgentDescriptor::new(
            "codex",
            "codex",
            cuma_core::AgentProtocol::Acp,
        )]);
        state.go_to(Screen::Agents);

        assert!(render(&state).contains("codex"));
    }

    #[test]
    fn the_usage_screen_warns_when_token_counts_were_estimated() {
        let mut state = AppState::new();
        state.tokens = cuma_core::TokenUsage::estimated(100, 50);
        state.go_to(Screen::Usage);

        assert!(render(&state).contains("estimated"));
    }

    #[test]
    fn a_very_small_terminal_still_renders() {
        // Layout constraints that assume space produce a panic when there is
        // none; a 20x10 terminal is a real thing users have.
        let backend = TestBackend::new(20, 10);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &state_with_tasks()))
            .expect("small terminals must not panic");
    }
}
