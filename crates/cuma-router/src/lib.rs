//! Multi-dimensional, explainable routing.
//!
//! The router answers one question: *given this task, which agent and which
//! model should run it?* It never answers "the biggest model available".
//!
//! Routing has three stages:
//!
//! 1. **Filter** — hard constraints. Pins, exclusions, health, circuit
//!    breakers, missing capabilities and budget ceilings remove candidates
//!    entirely. A filtered candidate can never be selected by a high score.
//! 2. **Score** — every surviving candidate is scored on five weighted
//!    dimensions (see [`score`]).
//! 3. **Explain** — the winner, its breakdown and the runners-up are returned
//!    together. A selection that cannot be explained cannot be tuned.
//!
//! Scores are only comparable *within one decision*. They are not a quality
//! rating of an agent in the abstract.

mod adaptive;
mod explain;
mod score;
mod router;

pub use adaptive::{AdaptiveStats, OutcomeRecord, RoutingHistory};
pub use explain::{Candidate, RoutingDecision, ScoreBreakdown};
pub use router::{RouteRequest, Router};
pub use score::DimensionScores;
