//! The terminal interface.
//!
//! The TUI is a *subscriber*, not a participant. It holds no orchestration
//! state of its own: it reads the event bus, folds each event into a view
//! model, and draws. That is what keeps the runtime independent of the
//! interface, so a web or IDE front end can attach the same way later.
//!
//! The state machine lives in [`state`] and is tested without a terminal.
//! Rendering is a pure function of state.

mod app;
mod state;
mod view;

pub use app::{Action, handle_key, run};
pub use state::{AppState, InputMode, Screen, TaskRow};
pub use view::{execution_lines, plan_lines, status_line};
