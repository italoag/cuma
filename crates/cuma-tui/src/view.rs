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
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Row, Tabs, Wrap};
use ratatui_bubbletea_components::{
    Progress, SpinnerFrames, TextInput, TextInputState, ThemedTable,
};
use ratatui_bubbletea_theme::Theme;

/// The interface's single source of colours, borders and symbols.
///
/// Every widget draws from this Charm palette rather than picking its own
/// `Color`, which is what gives the app the Bubble Tea look: pink accent,
/// rounded borders, muted hints.
fn theme() -> Theme {
    Theme::default()
}

/// The current spinner frame — the only animated glyph in the interface.
fn spinner_frame(state: &AppState) -> &'static str {
    let frames = SpinnerFrames::DOTS.frames();
    frames[state.spinner.frame_index() % frames.len()]
}

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

/// The bindings behind the help footer, in display order.
fn help_bindings(state: &AppState) -> &'static [(&'static str, &'static str)] {
    match state.input_mode {
        InputMode::Editing => &[("Enter", "submit"), ("Esc", "cancel")],
        InputMode::Navigating => &[
            ("Tab/Shift-Tab", "screens"),
            ("1-9", "jump"),
            ("i", "new goal"),
            ("↑↓", "scroll"),
            ("q", "quit"),
        ],
    }
}

/// The key hints shown at the bottom, which depend on the input mode.
pub fn help_line(state: &AppState) -> String {
    help_bindings(state)
        .iter()
        .map(|(key, description)| format!("{key} {description}"))
        .collect::<Vec<_>>()
        .join("  ·  ")
}

// ---------------------------------------------------------------------------
// Widgets
// ---------------------------------------------------------------------------

/// Style for a task's status marker.
fn status_style(theme: &Theme, status: TaskStatus) -> Style {
    match status {
        TaskStatus::Completed => theme.success,
        TaskStatus::Running => theme.accent,
        TaskStatus::Failed => theme.error,
        TaskStatus::Skipped | TaskStatus::Cancelled => theme.muted,
        TaskStatus::Pending | TaskStatus::Ready => theme.text,
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
    let theme = theme();
    let titles: Vec<Line<'_>> = Screen::ALL
        .iter()
        .enumerate()
        .map(|(index, screen)| {
            Line::from(vec![
                theme.muted(format!("{}", index + 1)),
                theme.span(":"),
                theme.span(screen.title()),
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
            .highlight_style(theme.title)
            .divider(" "),
        area,
    );
}

fn draw_status(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let theme = theme();
    let style = match (state.running, state.finished) {
        (true, _) => theme.accent,
        (false, Some(true)) => theme.success,
        (false, Some(false)) => theme.error,
        (false, None) => theme.muted,
    };

    let mut spans = Vec::new();
    if state.running {
        spans.push(theme.accent(spinner_frame(state)));
        spans.push(theme.span(" "));
    }
    spans.push(Span::styled(status_line(state), style));

    if let Some(notice) = &state.notice {
        spans.push(theme.muted("  |  "));
        spans.push(theme.warning(notice.clone()));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_body(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    match state.screen {
        Screen::Chat => draw_chat(frame, area, state),
        Screen::Tasks => draw_tasks(frame, area, state),
        Screen::Agents => draw_agents(frame, area, state),
        Screen::Models => draw_models(frame, area, state),
        Screen::Skills => draw_skills(frame, area, state),
        Screen::Memory => draw_memory(frame, area, state),
        Screen::Usage => draw_usage(frame, area, state),
        Screen::Logs => draw_logs(frame, area, state),
        Screen::Configuration => draw_configuration(frame, area, state),
    }
}

fn draw_chat(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let theme = theme();
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(area);

    // Left: the goal and the agent's streamed output.
    let mut lines: Vec<Line<'_>> = Vec::new();
    if !state.goal.is_empty() {
        lines.push(theme.title("You"));
        lines.push(Line::from(format!("> {}", state.goal)));
        lines.push(Line::from(""));
    }

    if !state.transcript.is_empty() {
        lines.push(theme.title("Agent"));
        for line in state.transcript.lines() {
            lines.push(Line::from(line.to_owned()));
        }
    }

    if lines.is_empty() {
        lines.push(Line::from(theme.muted("Press i to enter a goal.")));
    }

    frame.render_widget(
        Paragraph::new(lines)
            .block(theme.titled_block(" Conversation "))
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
                Span::styled(task.marker().to_owned(), status_style(&theme, task.status)),
                theme.span(" "),
                theme.span(task.description.clone()),
            ])
        })
        .collect();

    frame.render_widget(
        Paragraph::new(if plan.is_empty() {
            vec![Line::from(theme.muted("No plan yet."))]
        } else {
            plan
        })
        .block(theme.titled_block(" Plan "))
        .wrap(Wrap { trim: true }),
        rows[0],
    );

    let execution: Vec<Line<'_>> = execution_lines(state).into_iter().map(Line::from).collect();

    frame.render_widget(
        Paragraph::new(execution)
            .block(theme.titled_block(" Current execution "))
            .wrap(Wrap { trim: true }),
        rows[1],
    );
}

fn draw_tasks(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let theme = theme();
    let rows: Vec<Row<'_>> = state
        .tasks
        .iter()
        .map(|task| {
            Row::new(vec![
                Span::styled(task.marker().to_owned(), status_style(&theme, task.status)),
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

    let table = ThemedTable::new(
        rows,
        [
            Constraint::Length(2),
            Constraint::Length(10),
            Constraint::Length(16),
            Constraint::Length(16),
            Constraint::Min(20),
        ],
    )
    .header(Row::new(vec!["", "Status", "Agent", "Model", "Task"]))
    .title(" Tasks ");

    frame.render_widget(&table, area);
}

fn draw_agents(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let theme = theme();
    let rows: Vec<Row<'_>> = state
        .agents
        .iter()
        .map(|agent| {
            let health = state
                .agent_health
                .get(&agent.id)
                .cloned()
                .unwrap_or_else(|| format!("{:?}", agent.health.state));

            let style = if agent.is_routable() {
                theme.success
            } else {
                theme.error
            };

            Row::new(vec![
                Span::styled(agent.id.to_string(), style),
                Span::raw(format!("{:?}", agent.protocol)),
                Span::raw(health),
                Span::raw(agent.models.len().to_string()),
                Span::raw(agent.capabilities.len().to_string()),
            ])
        })
        .collect();

    let table = ThemedTable::new(
        rows,
        [
            Constraint::Length(20),
            Constraint::Length(10),
            Constraint::Length(14),
            Constraint::Length(8),
            Constraint::Min(6),
        ],
    )
    .header(Row::new(vec![
        "Agent",
        "Protocol",
        "Health",
        "Models",
        "Capabilities",
    ]))
    .title(" Agents ");

    frame.render_widget(&table, area);
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

    let table = ThemedTable::new(
        rows,
        [
            Constraint::Length(24),
            Constraint::Length(18),
            Constraint::Length(10),
            Constraint::Length(14),
            Constraint::Min(14),
        ],
    )
    .header(Row::new(vec![
        "Model",
        "Agent",
        "Context",
        "In $/Mtok",
        "Out $/Mtok",
    ]))
    .title(" Models ");

    frame.render_widget(&table, area);
}

fn draw_usage(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let theme = theme();
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
        lines.push(Line::from(theme.warning(
            "Some token counts were estimated, not reported by the agent.",
        )));
    }

    frame.render_widget(
        Paragraph::new(lines)
            .block(theme.titled_block(" Usage "))
            .scroll((state.scroll, 0)),
        area,
    );
}

fn draw_logs(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let theme = theme();
    let lines: Vec<Line<'_>> = state
        .logs
        .iter()
        .map(|line| {
            let style = if line.contains("failed") || line.contains("refused") {
                theme.error
            } else if line.contains("retry") || line.contains("falling back") {
                theme.warning
            } else {
                theme.muted
            };
            Line::from(Span::styled(line.clone(), style))
        })
        .collect();

    frame.render_widget(
        Paragraph::new(lines)
            .block(theme.titled_block(" Logs "))
            .wrap(Wrap { trim: true })
            .scroll((state.scroll, 0)),
        area,
    );
}

fn draw_configuration(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let theme = theme();
    let lines: Vec<Line<'_>> = state
        .last_explanation
        .as_deref()
        .unwrap_or("No routing decision has been made yet.")
        .lines()
        .map(|line| Line::from(line.to_owned()))
        .collect();

    frame.render_widget(
        Paragraph::new(lines)
            .block(theme.titled_block(" Last routing decision "))
            .wrap(Wrap { trim: false })
            .scroll((state.scroll, 0)),
        area,
    );
}

/// Lines for the Skills screen. Pure, so it can be tested without a terminal.
pub fn skill_lines(state: &AppState) -> Vec<Line<'static>> {
    let theme = theme();
    let Some(skills) = &state.skills else {
        return vec![Line::from(
            theme.muted("No skill manager is attached to this interface."),
        )];
    };
    if skills.is_empty() {
        return vec![
            Line::from("No skills are installed."),
            Line::from(""),
            Line::from(theme.muted(
                "Find some with: cuma skills search <query>, then cuma skills install <id>",
            )),
        ];
    }

    let mut lines = Vec::with_capacity(skills.len() + 2);
    for (index, skill) in skills.iter().enumerate() {
        let selected = index == state.skill_cursor;
        let marker = if skill.enabled { "[x]" } else { "[ ]" };
        let trust_style = match skill.trust.as_str() {
            "Trusted" => theme.success,
            "Verified" => theme.accent,
            "Community" => theme.warning,
            _ => theme.error,
        };
        let mut style = Style::default();
        if selected {
            style = theme.selected;
        }
        lines.push(Line::from(vec![
            Span::styled(format!("{marker} {:<24}", skill.id), style),
            Span::styled(format!(" {:<10}", skill.trust), trust_style),
            Span::raw(format!(" {:<12}", skill.registry)),
            theme.muted(skill.capabilities.join(", ")),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(theme.muted(
        "↑/↓ select   space enable/disable — an enabled skill's instructions accompany relevant tasks",
    )));
    lines
}

fn draw_skills(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    frame.render_widget(
        Paragraph::new(skill_lines(state))
            .block(theme().titled_block(" Skills "))
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// Lines for the Memory screen. Pure, so it can be tested without a terminal.
pub fn memory_lines(state: &AppState) -> Vec<Line<'static>> {
    let theme = theme();
    if !state.memory_attached {
        return vec![
            Line::from(theme.muted("Long-term memory is off.")),
            Line::from(""),
            Line::from("Enable it with [memory] enabled = true in .cuma/config.toml."),
        ];
    }

    let mut lines = Vec::new();
    match (&state.memory_query, state.memory_searching) {
        (_, true) => lines.push(Line::from(vec![
            theme.accent(spinner_frame(state)),
            theme.span(" "),
            theme.accent("searching…"),
        ])),
        (Some(query), false) => lines.push(Line::from(format!(
            "{} result{} for {query:?}",
            state.memories.len(),
            if state.memories.len() == 1 { "" } else { "s" }
        ))),
        (None, false) => lines.push(Line::from("Press / to search long-term memory.")),
    }
    lines.push(Line::from(""));

    for memory in &state.memories {
        lines.push(Line::from(vec![
            theme.accent(format!("{} ", memory.id)),
            theme.muted(format!("({})", memory.kind)),
        ]));
        // Memory is written by agents and people; it is shown, never acted on.
        for text in memory.content.lines().take(6) {
            lines.push(Line::from(format!("  {text}")));
        }
        lines.push(Line::from(""));
    }
    lines
}

fn draw_memory(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    frame.render_widget(
        Paragraph::new(memory_lines(state))
            .block(theme().titled_block(" Memory "))
            .wrap(Wrap { trim: false })
            .scroll((state.scroll, 0)),
        area,
    );
}

fn draw_input(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let theme = theme();
    let editing = state.input_mode == InputMode::Editing;

    let title = if editing {
        " Goal (Enter to run, Esc to cancel) "
    } else {
        " Goal "
    };

    // The border brightens while the box owns the keystrokes.
    let block = theme.block_with_focus(editing).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if editing {
        let input = TextInput::from_state(TextInputState::with_value(state.input.clone()))
            .placeholder("type a goal…")
            .focused(true);
        frame.render_widget(&input, inner);
    } else if state.running {
        let total = state.tasks.len();
        let done = state.completed_count();
        let ratio = if total == 0 {
            0.0
        } else {
            done as f64 / total as f64
        };
        let progress = Progress::from_ratio(ratio).label(format!("{done}/{total} tasks"));
        frame.render_widget(&progress, inner);
    } else {
        frame.render_widget(
            Paragraph::new(Line::from(theme.muted("press i to enter a goal"))),
            inner,
        );
    }
}

fn draw_help(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let theme = theme();
    frame.render_widget(
        Paragraph::new(theme.help_line(help_bindings(state).iter().copied())),
        area,
    );
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_skills_screen_marks_enabled_skills_and_explains_itself_when_empty() {
        let mut state = AppState::new();
        assert!(text(&skill_lines(&state)).contains("No skill manager"));
        state.set_skills(Vec::new());
        assert!(text(&skill_lines(&state)).contains("cuma skills install"));

        state.set_skills(vec![crate::SkillRow {
            id: "rust-debug".into(),
            name: "Rust debugging".into(),
            trust: "Trusted".into(),
            enabled: true,
            capabilities: vec!["debugging".into()],
            registry: "local".into(),
        }]);
        let rendered = text(&skill_lines(&state));
        assert!(rendered.contains("[x] rust-debug"));
        assert!(rendered.contains("Trusted"));
    }

    #[test]
    fn the_memory_screen_shows_results_and_its_state() {
        let mut state = AppState::new();
        assert!(text(&memory_lines(&state)).contains("memory is off"));

        state.memory_attached = true;
        assert!(text(&memory_lines(&state)).contains("Press / to search"));
        state.memory_searching = true;
        assert!(text(&memory_lines(&state)).contains("searching"));

        state.set_memories(
            "auth".into(),
            vec![crate::MemoryRow {
                id: "notes/auth.md".into(),
                kind: "fact".into(),
                content: "tokens live in src/auth.rs".into(),
            }],
        );
        let rendered = text(&memory_lines(&state));
        assert!(rendered.contains("1 result for \"auth\""));
        assert!(rendered.contains("notes/auth.md"));
        assert!(rendered.contains("tokens live in src/auth.rs"));
    }

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
