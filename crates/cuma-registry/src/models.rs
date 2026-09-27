//! The model registry.
//!
//! Agents and models are separate concepts: one agent may expose several
//! models with different prices, context windows and strengths. Routing picks
//! an *(agent, model)* pair, so both need first-class representation.

use cuma_core::{AgentId, Known, ModelDescriptor, ModelId};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Models, keyed by the agent that exposes them.
///
/// The same model *name* may appear under two agents with different pricing
/// and different observed reliability, so keys are `(AgentId, ModelId)` pairs
/// rather than bare model names.
#[derive(Clone, Default)]
pub struct ModelRegistry {
    models: Arc<RwLock<BTreeMap<(AgentId, ModelId), ModelDescriptor>>>,
}

impl ModelRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace a model.
    pub async fn register(&self, model: ModelDescriptor) {
        let key = (model.agent_id.clone(), model.id.clone());
        self.models.write().await.insert(key, model);
    }

    /// Register every model an agent exposes.
    pub async fn register_agent_models(&self, agent: &cuma_core::AgentDescriptor) {
        for model in &agent.models {
            self.register(model.clone()).await;
        }
    }

    /// Fetch one model.
    pub async fn get(&self, agent: &AgentId, model: &ModelId) -> Option<ModelDescriptor> {
        self.models
            .read()
            .await
            .get(&(agent.clone(), model.clone()))
            .cloned()
    }

    /// Every model an agent exposes.
    pub async fn for_agent(&self, agent: &AgentId) -> Vec<ModelDescriptor> {
        self.models
            .read()
            .await
            .iter()
            .filter(|((a, _), _)| a == agent)
            .map(|(_, m)| m.clone())
            .collect()
    }

    /// Every registered model.
    pub async fn all(&self) -> Vec<ModelDescriptor> {
        self.models.read().await.values().cloned().collect()
    }

    /// Mark a model available or not.
    pub async fn set_available(&self, agent: &AgentId, model: &ModelId, available: bool) {
        if let Some(m) = self
            .models
            .write()
            .await
            .get_mut(&(agent.clone(), model.clone()))
        {
            m.available = available;
        }
    }

    /// Fold an observed outcome into a model's historical success rate.
    ///
    /// An exponential moving average, rather than a running mean, so a model
    /// that has recovered is not held down by failures from last month. Alpha
    /// is fixed low enough that a single result cannot swing routing.
    pub async fn record_outcome(&self, agent: &AgentId, model: &ModelId, success: bool) {
        const ALPHA: f64 = 0.1;

        let mut guard = self.models.write().await;
        let Some(m) = guard.get_mut(&(agent.clone(), model.clone())) else {
            return;
        };

        let observation = if success { 1.0 } else { 0.0 };
        let updated = match m.performance.historical_success_rate {
            Known::Reported(prev) | Known::Estimated(prev) => {
                prev * (1.0 - ALPHA) + observation * ALPHA
            }
            // The first observation is the whole history we have.
            Known::Unknown => observation,
        };

        m.performance.historical_success_rate = Known::Estimated(updated.clamp(0.0, 1.0));
    }

    /// How many models are registered.
    pub async fn len(&self) -> usize {
        self.models.read().await.len()
    }

    /// Whether nothing is registered.
    pub async fn is_empty(&self) -> bool {
        self.models.read().await.is_empty()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn model(agent: &str, id: &str) -> ModelDescriptor {
        ModelDescriptor::minimal(AgentId::new(agent), id, id)
    }

    #[tokio::test]
    async fn the_same_model_name_under_two_agents_stays_distinct() {
        let registry = ModelRegistry::new();

        let mut a = model("agent-a", "shared-name");
        a.cost.input_per_mtok = Known::Reported(1.0);
        let mut b = model("agent-b", "shared-name");
        b.cost.input_per_mtok = Known::Reported(99.0);

        registry.register(a).await;
        registry.register(b).await;

        assert_eq!(registry.len().await, 2);
        let fetched = registry
            .get(&AgentId::new("agent-b"), &ModelId::new("shared-name"))
            .await
            .unwrap();
        assert_eq!(fetched.cost.input_per_mtok.value(), Some(99.0));
    }

    #[tokio::test]
    async fn the_first_outcome_establishes_the_success_rate() {
        let registry = ModelRegistry::new();
        registry.register(model("a", "m")).await;

        let (agent, id) = (AgentId::new("a"), ModelId::new("m"));
        registry.record_outcome(&agent, &id, true).await;

        let m = registry.get(&agent, &id).await.unwrap();
        assert_eq!(m.performance.historical_success_rate.value(), Some(1.0));
    }

    #[tokio::test]
    async fn one_failure_does_not_collapse_an_established_success_rate() {
        let registry = ModelRegistry::new();
        registry.register(model("a", "m")).await;
        let (agent, id) = (AgentId::new("a"), ModelId::new("m"));

        for _ in 0..30 {
            registry.record_outcome(&agent, &id, true).await;
        }
        registry.record_outcome(&agent, &id, false).await;

        let rate = registry
            .get(&agent, &id)
            .await
            .unwrap()
            .performance
            .historical_success_rate
            .value()
            .unwrap();

        assert!(rate > 0.85, "one blip should not dominate, got {rate}");
    }

    #[tokio::test]
    async fn sustained_failure_does_drive_the_success_rate_down() {
        let registry = ModelRegistry::new();
        registry.register(model("a", "m")).await;
        let (agent, id) = (AgentId::new("a"), ModelId::new("m"));

        registry.record_outcome(&agent, &id, true).await;
        for _ in 0..40 {
            registry.record_outcome(&agent, &id, false).await;
        }

        let rate = registry
            .get(&agent, &id)
            .await
            .unwrap()
            .performance
            .historical_success_rate
            .value()
            .unwrap();

        assert!(rate < 0.05, "sustained failure must register, got {rate}");
    }

    #[tokio::test]
    async fn a_learned_success_rate_is_marked_estimated_not_reported() {
        let registry = ModelRegistry::new();
        registry.register(model("a", "m")).await;
        let (agent, id) = (AgentId::new("a"), ModelId::new("m"));
        registry.record_outcome(&agent, &id, true).await;

        let m = registry.get(&agent, &id).await.unwrap();
        assert!(
            !m.performance.historical_success_rate.is_reported(),
            "we derived this; it is not ground truth from the provider"
        );
    }

    #[tokio::test]
    async fn recording_an_outcome_for_an_unknown_model_is_a_no_op() {
        let registry = ModelRegistry::new();
        registry
            .record_outcome(&AgentId::new("ghost"), &ModelId::new("m"), true)
            .await;
        assert!(registry.is_empty().await);
    }

    #[tokio::test]
    async fn models_can_be_listed_per_agent() {
        let registry = ModelRegistry::new();
        registry.register(model("a", "m1")).await;
        registry.register(model("a", "m2")).await;
        registry.register(model("b", "m3")).await;

        assert_eq!(registry.for_agent(&AgentId::new("a")).await.len(), 2);
        assert_eq!(registry.all().await.len(), 3);
    }
}
