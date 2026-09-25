//! The event loop.
//!
//! Three sources feed one loop: terminal input, the orchestrator's event bus,
//! and the completion of a running session. All three are `select!`ed so that
//! none blocks another — in particular, a running agent must never stop the
//! interface from redrawing or from accepting a quit.

use crate::state::{AppState, InputMode, Screen};
use crate::view;
use cuma_core::error::{MetaAgentError, Result};
use cuma_core::{Event, EventSubscriber};
use cuma_orchestrator::Orchestrator;
use ratatui::crossterm::event::{
    Event as TerminalEvent, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use std::sync::Arc;
use std::time::Duration;
use tokio_stream::StreamExt;

/// How often the loop redraws when nothing is happening.
///
/// Not a poll for work — the bus wakes the loop for that. This only keeps
/// elapsed-time displays honest and repaints after a resize the terminal did
/// not report.
const IDLE_REDRAW: Duration = Duration::from_millis(250);

/// What a keystroke asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Nothing.
    None,
    /// Leave the interface.
    Quit,
    /// Redraw.
    Redraw,
    /// Start a session with this goal.
    Submit(String),
    /// Search long-term memory for this.
    SearchMemory(String),
    /// Turn this skill on or off.
    ToggleSkill {
        /// The skill.
        id: String,
        /// What it should become.
        enabled: bool,
    },
}

/// Installed skills, for the Skills screen.
///
/// Defined here, not taken from the skills crate, so the interface depends on
/// what it shows rather than on how skills are managed.
#[async_trait::async_trait]
pub trait SkillSource: Send + Sync {
    /// Every installed skill.
    async fn skills(&self) -> Result<Vec<crate::SkillRow>>;
    /// Turn a skill's instructions on or off.
    async fn set_enabled(&self, id: &str, enabled: bool) -> Result<()>;
}

/// What the interface can show beyond the orchestrator's own state.
#[derive(Default, Clone)]
pub struct Sources {
    /// Installed skills.
    pub skills: Option<Arc<dyn SkillSource>>,
    /// Long-term memory.
    pub memory: Option<Arc<dyn cuma_core::ports::MemoryStore>>,
}

/// Translate a keystroke into an action, given the current mode.
///
/// Pure, so the whole keymap is testable without a terminal.
pub fn handle_key(state: &mut AppState, key: KeyEvent) -> Action {
    // Windows reports both press and release; acting on both double-types.
    if key.kind == KeyEventKind::Release {
        return Action::None;
    }

    // Ctrl-C quits from any mode. A user who cannot leave an interface is
    // trapped, and Esc alone is not enough while editing.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        state.quit();
        return Action::Quit;
    }

    match state.input_mode {
        InputMode::Editing => match key.code {
            KeyCode::Enter => match state.submit() {
                Some(query) if state.screen == Screen::Memory => {
                    state.memory_searching = true;
                    Action::SearchMemory(query)
                }
                Some(goal) => Action::Submit(goal),
                None => Action::Redraw,
            },
            KeyCode::Esc => {
                state.cancel_editing();
                Action::Redraw
            }
            KeyCode::Backspace => {
                state.pop_char();
                Action::Redraw
            }
            KeyCode::Char(character) => {
                state.push_char(character);
                Action::Redraw
            }
            _ => Action::None,
        },

        InputMode::Navigating => match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                state.quit();
                Action::Quit
            }
            KeyCode::Char('i') | KeyCode::Char('/') => {
                state.begin_editing();
                Action::Redraw
            }
            KeyCode::Tab | KeyCode::Right => {
                state.next_screen();
                Action::Redraw
            }
            KeyCode::BackTab | KeyCode::Left => {
                state.previous_screen();
                Action::Redraw
            }
            KeyCode::Down | KeyCode::Char('j') if state.screen == Screen::Skills => {
                state.move_skill_cursor(1);
                Action::Redraw
            }
            KeyCode::Up | KeyCode::Char('k') if state.screen == Screen::Skills => {
                state.move_skill_cursor(-1);
                Action::Redraw
            }
            KeyCode::Char(' ') | KeyCode::Char('e') if state.screen == Screen::Skills => {
                match state.selected_skill() {
                    Some(skill) => Action::ToggleSkill {
                        id: skill.id.clone(),
                        enabled: !skill.enabled,
                    },
                    None => Action::None,
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                state.scroll_down(1);
                Action::Redraw
            }
            KeyCode::Up | KeyCode::Char('k') => {
                state.scroll_up(1);
                Action::Redraw
            }
            KeyCode::PageDown => {
                state.scroll_down(10);
                Action::Redraw
            }
            KeyCode::PageUp => {
                state.scroll_up(10);
                Action::Redraw
            }
            KeyCode::Home => {
                state.scroll = 0;
                Action::Redraw
            }
            KeyCode::Char(digit @ '1'..='9') => {
                let index = (digit as usize) - ('1' as usize);
                if let Some(screen) = Screen::ALL.get(index) {
                    state.go_to(*screen);
                }
                Action::Redraw
            }
            _ => Action::None,
        },
    }
}

/// Run the interface until the user quits.
///
/// The terminal is restored on every exit path, including an error, because
/// leaving raw mode on makes the user's shell unusable.
pub async fn run(orchestrator: Orchestrator) -> Result<()> {
    run_with(orchestrator, Sources::default()).await
}

/// Run the interface with skills and memory to browse.
pub async fn run_with(orchestrator: Orchestrator, sources: Sources) -> Result<()> {
    let orchestrator = Arc::new(orchestrator);

    let mut state = AppState::new();
    state.set_agents(orchestrator.agents().snapshot().await.all().to_vec());
    state.memory_attached = sources.memory.is_some();
    refresh_skills(&mut state, &sources).await;

    let events = orchestrator.events().subscribe();

    let mut terminal = ratatui::try_init()
        .map_err(|err| MetaAgentError::Other(format!("cannot initialize the terminal: {err}")))?;

    let outcome = event_loop(&mut terminal, &mut state, orchestrator, events, sources).await;

    // Restore before propagating, so an error message lands in a usable shell.
    let restored = ratatui::try_restore();

    outcome?;
    restored.map_err(|err| MetaAgentError::Other(format!("cannot restore the terminal: {err}")))
}

/// The loop proper, separated so `run` can always restore the terminal.
async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    state: &mut AppState,
    orchestrator: Arc<Orchestrator>,
    mut events: EventSubscriber,
    sources: Sources,
) -> Result<()> {
    let mut input = EventStream::new();
    let mut session: Option<tokio::task::JoinHandle<Result<cuma_orchestrator::SessionResult>>> =
        None;
    type Recall = (String, Result<Vec<cuma_core::ports::MemoryEntry>>);
    let mut recall: Option<tokio::task::JoinHandle<Recall>> = None;
    let mut redraw = true;

    loop {
        if redraw {
            terminal
                .draw(|frame| view::draw(frame, state))
                .map_err(|err| MetaAgentError::Other(format!("cannot draw: {err}")))?;
            redraw = false;
        }

        if state.should_quit {
            break;
        }

        tokio::select! {
            // Terminal input.
            Some(Ok(terminal_event)) = input.next() => {
                match terminal_event {
                    TerminalEvent::Key(key) => match handle_key(state, key) {
                        Action::Quit => break,
                        Action::Redraw => redraw = true,
                        Action::Submit(goal) => {
                            // Refuse rather than interleave two plans in one
                            // view model.
                            if session.is_some() {
                                state.notice = Some("a session is already running".to_owned());
                            } else {
                                let orchestrator = Arc::clone(&orchestrator);
                                session = Some(tokio::spawn(async move {
                                    orchestrator.run(&goal).await
                                }));
                            }
                            redraw = true;
                        }
                        Action::SearchMemory(query) => {
                            match &sources.memory {
                                Some(memory) => {
                                    if let Some(previous) = recall.take() {
                                        previous.abort();
                                    }
                                    let memory = Arc::clone(memory);
                                    recall = Some(tokio::spawn(async move {
                                        let found = tokio::time::timeout(
                                            MEMORY_TIMEOUT,
                                            memory.recall(&query, 20),
                                        )
                                        .await
                                        .unwrap_or_else(|_| {
                                            Err(MetaAgentError::Memory("the search timed out".to_owned()))
                                        });
                                        (query, found)
                                    }));
                                }
                                None => {
                                    state.memory_searching = false;
                                    state.notice = Some("no memory backend is configured".to_owned());
                                }
                            }
                            redraw = true;
                        }
                        Action::ToggleSkill { id, enabled } => {
                            if let Some(skills) = &sources.skills {
                                state.notice = Some(match skills.set_enabled(&id, enabled).await {
                                    Ok(()) => format!("{} {id}", if enabled { "enabled" } else { "disabled" }),
                                    Err(err) => err.to_string(),
                                });
                                refresh_skills(state, &sources).await;
                            }
                            redraw = true;
                        }
                        Action::None => {}
                    },
                    TerminalEvent::Resize(_, _) => redraw = true,
                    _ => {}
                }
            }

            // Orchestrator events.
            received = events.recv() => {
                match received {
                    Ok(event) => {
                        apply_and_refresh(state, &event, &orchestrator).await;
                        redraw = true;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                        // The bus is lossy by design. Say so rather than
                        // silently showing an incomplete picture.
                        state.notice = Some(format!("{missed} events dropped while redrawing"));
                        redraw = true;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }

            // Session completion.
            result = async {
                match session.as_mut() {
                    Some(handle) => handle.await,
                    // No session running: never resolve, so `select!` ignores
                    // this branch instead of spinning.
                    None => std::future::pending().await,
                }
            } => {
                session = None;

                state.notice = Some(match result {
                    Ok(Ok(outcome)) => outcome.summary,
                    Ok(Err(err)) => format!("session failed: {err}"),
                    Err(err) => format!("session task panicked: {err}"),
                });

                state.set_agents(orchestrator.agents().snapshot().await.all().to_vec());
                redraw = true;
            }

            // Memory search completion.
            finished = async {
                match recall.as_mut() {
                    Some(handle) => handle.await,
                    None => std::future::pending().await,
                }
            } => {
                recall = None;
                match finished {
                    Ok((query, Ok(entries))) => {
                        let rows = entries
                            .into_iter()
                            .map(|entry| crate::MemoryRow {
                                id: entry.id,
                                kind: entry.kind,
                                content: entry.content,
                            })
                            .collect();
                        state.set_memories(query, rows);
                    }
                    Ok((query, Err(err))) => {
                        state.set_memories(query, Vec::new());
                        state.notice = Some(format!("memory search failed: {err}"));
                    }
                    Err(err) => {
                        state.memory_searching = false;
                        state.notice = Some(format!("memory search stopped: {err}"));
                    }
                }
                redraw = true;
            }

            // Idle repaint.
            () = tokio::time::sleep(IDLE_REDRAW) => redraw = true,
        }
    }

    // A session outliving the interface would keep spending tokens with
    // nobody watching.
    if let Some(handle) = session {
        handle.abort();
    }
    if let Some(handle) = recall {
        handle.abort();
    }

    Ok(())
}

/// How long a memory search may take before the interface gives up on it.
const MEMORY_TIMEOUT: Duration = Duration::from_secs(15);

/// Re-read installed skills, when a source is attached.
async fn refresh_skills(state: &mut AppState, sources: &Sources) {
    if let Some(skills) = &sources.skills {
        match skills.skills().await {
            Ok(rows) => state.set_skills(rows),
            Err(err) => state.notice = Some(format!("cannot list skills: {err}")),
        }
    }
}

/// Fold an event into the state, refreshing anything the event cannot carry.
async fn apply_and_refresh(state: &mut AppState, event: &Event, orchestrator: &Orchestrator) {
    state.apply(event);

    // Health lives in the registry, not in the event, so a breaker transition
    // is the cue to re-read it.
    if matches!(
        event.kind,
        cuma_core::EventKind::CircuitBreakerChanged { .. }
    ) {
        state.set_agents(orchestrator.agents().snapshot().await.all().to_vec());
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn q_quits_while_navigating() {
        let mut state = AppState::new();
        assert_eq!(
            handle_key(&mut state, press(KeyCode::Char('q'))),
            Action::Quit
        );
        assert!(state.should_quit);
    }

    #[test]
    fn q_is_a_literal_q_while_editing() {
        let mut state = AppState::new();
        state.begin_editing();

        handle_key(&mut state, press(KeyCode::Char('q')));
        assert_eq!(state.input, "q");
        assert!(!state.should_quit, "typing must not quit");
    }

    #[test]
    fn ctrl_c_quits_from_any_mode() {
        for editing in [false, true] {
            let mut state = AppState::new();
            if editing {
                state.begin_editing();
            }

            let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
            assert_eq!(handle_key(&mut state, key), Action::Quit);
            assert!(state.should_quit, "editing={editing}");
        }
    }

    #[test]
    fn key_release_events_are_ignored() {
        // Windows reports press and release; acting on both double-types.
        let mut state = AppState::new();
        state.begin_editing();

        let mut key = press(KeyCode::Char('a'));
        key.kind = KeyEventKind::Release;

        assert_eq!(handle_key(&mut state, key), Action::None);
        assert!(state.input.is_empty());
    }

    #[test]
    fn tab_cycles_screens_in_both_directions() {
        let mut state = AppState::new();
        assert_eq!(state.screen, Screen::Chat);

        handle_key(&mut state, press(KeyCode::Tab));
        assert_eq!(state.screen, Screen::Tasks);

        handle_key(&mut state, press(KeyCode::BackTab));
        assert_eq!(state.screen, Screen::Chat);
    }

    #[test]
    fn digits_jump_straight_to_a_screen() {
        let mut state = AppState::new();

        handle_key(&mut state, press(KeyCode::Char('3')));
        assert_eq!(state.screen, Screen::Agents);

        handle_key(&mut state, press(KeyCode::Char('1')));
        assert_eq!(state.screen, Screen::Chat);
    }

    #[test]
    fn a_digit_beyond_the_last_screen_is_ignored() {
        let mut state = AppState::new();
        state.go_to(Screen::Tasks);

        handle_key(&mut state, press(KeyCode::Char('9')));
        assert_eq!(state.screen, Screen::Configuration, "9 is the last screen");
    }

    #[test]
    fn changing_screens_resets_the_scroll_offset() {
        let mut state = AppState::new();
        state.scroll_down(30);

        handle_key(&mut state, press(KeyCode::Tab));
        assert_eq!(state.scroll, 0, "30 lines into a shorter list is nowhere");
    }

    #[test]
    fn scrolling_never_underflows() {
        let mut state = AppState::new();
        for _ in 0..5 {
            handle_key(&mut state, press(KeyCode::Up));
        }
        assert_eq!(state.scroll, 0);
    }

    #[test]
    fn typing_a_goal_and_pressing_enter_submits_it() {
        let mut state = AppState::new();
        handle_key(&mut state, press(KeyCode::Char('i')));
        assert_eq!(state.input_mode, InputMode::Editing);

        for character in "fix the bug".chars() {
            handle_key(&mut state, press(KeyCode::Char(character)));
        }

        assert_eq!(
            handle_key(&mut state, press(KeyCode::Enter)),
            Action::Submit("fix the bug".to_owned())
        );
        assert_eq!(state.input_mode, InputMode::Navigating);
        assert!(state.input.is_empty());
    }

    #[test]
    fn an_empty_goal_is_not_submitted() {
        let mut state = AppState::new();
        state.begin_editing();

        assert_eq!(
            handle_key(&mut state, press(KeyCode::Enter)),
            Action::Redraw
        );
        assert_eq!(state.input_mode, InputMode::Navigating);
    }

    #[test]
    fn a_whitespace_only_goal_is_not_submitted() {
        let mut state = AppState::new();
        state.begin_editing();
        for _ in 0..3 {
            handle_key(&mut state, press(KeyCode::Char(' ')));
        }

        assert_eq!(
            handle_key(&mut state, press(KeyCode::Enter)),
            Action::Redraw
        );
    }

    #[test]
    fn escape_abandons_the_goal_being_typed() {
        let mut state = AppState::new();
        state.begin_editing();
        handle_key(&mut state, press(KeyCode::Char('x')));

        handle_key(&mut state, press(KeyCode::Esc));
        assert_eq!(state.input_mode, InputMode::Navigating);
        assert!(state.input.is_empty());
        assert!(!state.should_quit, "Esc leaves editing, it does not quit");
    }

    #[test]
    fn backspace_deletes_the_last_character() {
        let mut state = AppState::new();
        state.begin_editing();
        for character in "abc".chars() {
            handle_key(&mut state, press(KeyCode::Char(character)));
        }

        handle_key(&mut state, press(KeyCode::Backspace));
        assert_eq!(state.input, "ab");
    }

    #[test]
    fn a_goal_cannot_be_entered_while_a_session_runs() {
        let mut state = AppState::new();
        state.running = true;

        handle_key(&mut state, press(KeyCode::Char('i')));
        assert_eq!(state.input_mode, InputMode::Navigating);
        assert!(state.notice.unwrap().contains("already running"));
    }

    fn skills_state() -> AppState {
        let mut state = AppState::new();
        state.go_to(Screen::Skills);
        let row = |id: &str, enabled: bool| crate::SkillRow {
            id: id.into(),
            name: id.into(),
            trust: "Trusted".into(),
            enabled,
            capabilities: Vec::new(),
            registry: "builtin".into(),
        };
        state.set_skills(vec![row("a", true), row("b", false)]);
        state
    }

    #[test]
    fn on_the_skills_screen_arrows_move_the_cursor_and_space_toggles() {
        let mut state = skills_state();
        handle_key(&mut state, press(KeyCode::Down));
        handle_key(&mut state, press(KeyCode::Down));
        assert_eq!(state.skill_cursor, 1, "the cursor stops at the last skill");
        assert_eq!(
            handle_key(&mut state, press(KeyCode::Char(' '))),
            Action::ToggleSkill {
                id: "b".into(),
                enabled: true
            }
        );
        handle_key(&mut state, press(KeyCode::Up));
        assert_eq!(
            handle_key(&mut state, press(KeyCode::Char('e'))),
            Action::ToggleSkill {
                id: "a".into(),
                enabled: false
            }
        );
    }

    #[test]
    fn on_the_memory_screen_enter_searches_instead_of_running_a_goal() {
        let mut state = AppState::new();
        state.go_to(Screen::Memory);
        state.running = true;
        handle_key(&mut state, press(KeyCode::Char('/')));
        assert_eq!(
            state.input_mode,
            InputMode::Editing,
            "searching is allowed mid-session"
        );
        for c in "auth".chars() {
            handle_key(&mut state, press(KeyCode::Char(c)));
        }
        assert_eq!(
            handle_key(&mut state, press(KeyCode::Enter)),
            Action::SearchMemory("auth".into())
        );
        assert!(state.memory_searching);
    }

    #[test]
    fn an_unbound_key_does_nothing() {
        let mut state = AppState::new();
        let before = format!("{state:?}");

        assert_eq!(handle_key(&mut state, press(KeyCode::F(7))), Action::None);
        assert_eq!(format!("{state:?}"), before);
    }
}
