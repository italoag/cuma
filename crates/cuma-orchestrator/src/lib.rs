//! The orchestrator.
//!
//! This is where planning, routing, execution, resilience, handoff and
//! accounting meet. It owns the loop that turns a user goal into a result:
//!
//! ```text
//! goal -> plan -> for each ready task:
//!           route -> execute -> on failure: classify -> retry | reroute | replan
//!                            -> on success: record usage, unblock dependents
//! ```
//!
//! What it deliberately does *not* own: how any agent is reached (adapters),
//! how candidates are scored (the router), how failures are classified (the
//! core), or how anything is displayed (subscribers to the event bus).

mod context;
mod executor;

pub use context::MinimalContextManager;
pub use executor::{Orchestrator, SessionResult};
