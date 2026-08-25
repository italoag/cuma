//! The terminal interface.
//!
//! The TUI is a *subscriber*, not a participant. It holds no orchestration
//! state of its own: it reads the event bus, folds each event into a view
//! model, and draws. That is what keeps the runtime independent of the
//! interface, so a web or IDE front end can attach the same way later.
//!
//! The state machine lives in [`state`] and is tested without a terminal.
//! Rendering is a pure function of state.

mod state;
mod view;

pub use state::{AppState, Screen, TaskRow};
pub use view::{execution_lines, plan_lines, status_line};

use cuma_core::error::Result;
use cuma_orchestrator::Orchestrator;

/// Run the interactive interface.
///
/// Not yet implemented: the event loop and rendering are the remaining work
/// for Milestone 10 (see `docs/ROADMAP.md`). The state machine underneath is
/// complete and tested, so this is wiring rather than design.
pub async fn run(orchestrator: Orchestrator) -> Result<()> {
    let snapshot = orchestrator.agents().snapshot().await;

    eprintln!("The interactive TUI is not wired up yet (Milestone 10).");
    eprintln!(
        "Everything it will show is available now through the CLI:\n\
         \n\
         \x20 cuma run \"<goal>\"      run a goal\n\
         \x20 cuma explain \"<goal>\"  plan and route without executing\n\
         \x20 cuma agents list       {} agents registered\n\
         \x20 cuma usage             token, cost and outcome statistics\n\
         \x20 cuma doctor            check the installation",
        snapshot.len()
    );

    Ok(())
}
