//! The runtime database as a [`SessionRecorder`].

use cuma_core::{AgentId, HealthState, SessionId, TaskId};
use cuma_orchestrator::{SessionRecorder, SessionResult};
use cuma_persistence::RuntimeStore;
use cuma_router::RoutingDecision;
use cuma_usage::UsageRecord;

/// Writes every session to the runtime database as it happens.
pub struct StoreRecorder {
    store: RuntimeStore,
}

impl StoreRecorder {
    /// Record into `store`.
    pub fn new(store: RuntimeStore) -> Self {
        Self { store }
    }
}

/// Log a write that failed. The session carries on regardless.
fn logged(what: &str, result: cuma_core::Result<()>) {
    if let Err(err) = result {
        tracing::warn!(error = %err, "could not record {what} in the runtime database");
    }
}

impl SessionRecorder for StoreRecorder {
    fn session_started(&self, session: &SessionId, goal: &str) {
        logged("a session", self.store.begin_session(session, goal));
    }

    fn routing_decided(&self, session: &SessionId, task: &TaskId, decision: &RoutingDecision) {
        logged(
            "a routing decision",
            self.store.record_routing_decision(
                session,
                task,
                &decision.selected.agent_id,
                decision.selected.model_id.as_ref(),
                decision.selected.breakdown.total,
                &decision.explain(),
            ),
        );
    }

    fn attempt_recorded(&self, record: &UsageRecord) {
        logged("an attempt", self.store.record_attempt(record));
    }

    fn agent_health_changed(&self, agent: &AgentId, state: HealthState, error: Option<&str>) {
        logged(
            "agent health",
            self.store
                .record_agent_health(agent, &format!("{state:?}"), error),
        );
    }

    fn session_finished(&self, result: &SessionResult) {
        for task in result.graph.iter() {
            logged("a task", self.store.save_task(&result.session_id, task));
        }
        logged(
            "a session's end",
            self.store
                .finish_session(&result.session_id, result.success, &result.summary),
        );
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use cuma_core::{AgentDescriptor, AgentProtocol, Capability, Known, ModelDescriptor};
    use cuma_orchestrator::Orchestrator;
    use cuma_testkit::{Behaviour, MockAgent};
    use std::sync::Arc;

    fn agent(id: &str, behaviour: Behaviour, price: f64) -> MockAgent {
        let mut descriptor = AgentDescriptor::new(id, id, AgentProtocol::Native).with_capabilities(
            [
                Capability::CodeComprehension,
                Capability::CodeGeneration,
                Capability::CodeEditing,
                Capability::Testing,
                Capability::Research,
                Capability::Documentation,
                Capability::Planning,
                Capability::Debugging,
                Capability::Refactoring,
                Capability::ShellExecution,
                Capability::FileSystem,
                Capability::VersionControl,
                Capability::Architecture,
                Capability::CodeReview,
                Capability::ToolUse,
            ]
            .into_iter()
            .collect(),
        );
        let mut model = ModelDescriptor::minimal(descriptor.id.clone(), format!("{id}-m"), id);
        model.context_window = Known::Reported(200_000);
        model.cost.input_per_mtok = Known::Reported(price);
        model.cost.output_per_mtok = Known::Reported(price);
        descriptor.models.push(model);
        MockAgent::always(id, behaviour).with_descriptor(descriptor)
    }

    #[tokio::test]
    async fn every_step_of_a_session_reaches_the_database() {
        let store = RuntimeStore::in_memory().unwrap();

        // Cost-first, so the cheap, broken agent is tried before the other.
        let mut config = cuma_config::Config::default();
        config.router.strategy = cuma_config::RoutingStrategy::CostFirst;
        config.router.weights = cuma_config::RoutingStrategy::CostFirst.default_weights();

        let mut orchestrator = Orchestrator::new(
            config,
            Arc::new(cuma_planner::HeuristicPlanner::new()),
            std::env::temp_dir(),
        )
        .with_recorder(Arc::new(StoreRecorder::new(store.clone())));
        orchestrator
            .add_agent(Arc::new(agent(
                "broken",
                Behaviour::Crash {
                    message: "exit 139".into(),
                },
                0.1,
            )))
            .await
            .unwrap();
        orchestrator
            .add_agent(Arc::new(agent("working", Behaviour::ok("done"), 5.0)))
            .await
            .unwrap();

        let result = orchestrator.run("write docs for the API").await.unwrap();
        assert!(result.success);

        assert_eq!(store.session_count().unwrap(), 1);
        assert!(
            store.routing_decision_count().unwrap() >= 2,
            "each routing pass is kept"
        );
        assert!(
            store.attempt_count().unwrap() >= 2,
            "the failure is kept as well as the success"
        );

        let health = store.agent_health().unwrap();
        let broken = health.iter().find(|h| h.agent_id == "broken").unwrap();
        assert!(broken.consecutive_failures >= 1);
        assert!(broken.last_error.as_deref().unwrap().contains("exit 139"));

        let history = store.load_routing_history().unwrap();
        assert!(!history.is_empty(), "the next process learns from this one");
    }
}
