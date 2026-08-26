//! Goal decomposition.
//!
//! The planner turns "implement OAuth and fix the tests" into a dependency
//! graph the orchestrator can execute and the router can route task by task.
//!
//! Two implementations ship:
//!
//! - [`HeuristicPlanner`] — rule-based, deterministic, no model call. It is
//!   the default because a planner that needs a network round trip before the
//!   harness can do anything is a planner that fails when the network does.
//! - [`LlmPlanner`] — delegates decomposition to an [`LlmProvider`], for goals
//!   the heuristics do not recognize. It falls back to the heuristic planner
//!   rather than failing the session.
//!
//! [`LlmProvider`]: cuma_core::ports::LlmProvider

mod heuristic;
mod llm;

pub use heuristic::HeuristicPlanner;
pub use llm::{LlmPlanner, unsatisfiable_capabilities};
