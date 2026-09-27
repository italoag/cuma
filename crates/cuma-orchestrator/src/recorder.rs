//! Where a session's record goes as it happens.
//!
//! The orchestrator calls a [`SessionRecorder`] at each step that is worth
//! keeping — a session starting, a routing decision, an attempt, a change in
//! an agent's health, the session's end. Recording as it happens, rather than
//! after the fact, is what makes the record complete whichever front end
//! started the session: the CLI, the TUI, an editor over ACP, a peer over
//! A2A, a host over MCP.
//!
//! Methods are synchronous and must be quick; an implementation that can
//! fail logs and carries on. A session never fails because it could not be
//! written down.

use crate::SessionResult;
use cuma_core::{AgentId, HealthState, SessionId, TaskId};
use cuma_router::RoutingDecision;
use cuma_usage::UsageRecord;

/// Receives a session's record as it is made. Every method defaults to doing
/// nothing.
pub trait SessionRecorder: Send + Sync {
    /// A session began.
    fn session_started(&self, session: &SessionId, goal: &str) {
        let _ = (session, goal);
    }

    /// A task was routed.
    fn routing_decided(&self, session: &SessionId, task: &TaskId, decision: &RoutingDecision) {
        let _ = (session, task, decision);
    }

    /// An attempt finished, successfully or not.
    fn attempt_recorded(&self, record: &UsageRecord) {
        let _ = record;
    }

    /// An agent's health changed.
    fn agent_health_changed(&self, agent: &AgentId, state: HealthState, error: Option<&str>) {
        let _ = (agent, state, error);
    }

    /// A session ended.
    fn session_finished(&self, result: &SessionResult) {
        let _ = result;
    }
}
