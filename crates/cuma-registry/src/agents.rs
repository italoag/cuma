//! The agent registry.

use cuma_core::error::Result;
use cuma_core::ports::AgentDiscovery;
use cuma_core::{AgentDescriptor, AgentId, AgentProtocol, HealthState};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// A point-in-time view of the registry, cheap to clone and pass around.
///
/// The router takes a snapshot rather than holding the lock: routing is pure
/// scoring over a fixed candidate set, and holding a read lock across an
/// `await` is how deadlocks get written.
#[derive(Debug, Clone, Default)]
pub struct RegistrySnapshot {
    agents: Vec<AgentDescriptor>,
}

impl RegistrySnapshot {
    /// Build a snapshot from descriptors.
    pub fn new(agents: Vec<AgentDescriptor>) -> Self {
        Self { agents }
    }

    /// Every agent, enabled or not.
    pub fn all(&self) -> &[AgentDescriptor] {
        &self.agents
    }

    /// Agents the router may currently consider.
    pub fn routable(&self) -> impl Iterator<Item = &AgentDescriptor> {
        self.agents.iter().filter(|a| a.is_routable())
    }

    /// Look up one agent.
    pub fn get(&self, id: &AgentId) -> Option<&AgentDescriptor> {
        self.agents.iter().find(|a| &a.id == id)
    }

    /// How many agents are registered.
    pub fn len(&self) -> usize {
        self.agents.len()
    }

    /// Whether nothing is registered.
    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }

    /// The union of every routable agent's capabilities.
    ///
    /// The planner uses this to avoid producing tasks that nothing can run.
    pub fn available_capabilities(&self) -> cuma_core::CapabilitySet {
        let mut set = cuma_core::CapabilitySet::new();
        for agent in self.routable() {
            for capability in agent.capabilities.iter() {
                set.insert(capability.clone());
            }
            for model in &agent.models {
                for capability in model.capabilities.iter() {
                    set.insert(capability.clone());
                }
            }
        }
        set
    }
}

/// The registry of known agents.
#[derive(Clone, Default)]
pub struct AgentRegistry {
    agents: Arc<RwLock<BTreeMap<AgentId, AgentDescriptor>>>,
}

impl AgentRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace an agent.
    ///
    /// Re-registering preserves the existing health record: discovery re-runs
    /// should not wipe out the circuit-breaker history that tells the router
    /// an agent is flaky.
    pub async fn register(&self, mut agent: AgentDescriptor) {
        let mut guard = self.agents.write().await;
        if let Some(existing) = guard.get(&agent.id) {
            agent.health = existing.health.clone();
        }
        guard.insert(agent.id.clone(), agent);
    }

    /// Register many agents.
    pub async fn register_all(&self, agents: impl IntoIterator<Item = AgentDescriptor>) {
        for agent in agents {
            self.register(agent).await;
        }
    }

    /// Remove an agent.
    pub async fn deregister(&self, id: &AgentId) -> Option<AgentDescriptor> {
        self.agents.write().await.remove(id)
    }

    /// Fetch one agent.
    pub async fn get(&self, id: &AgentId) -> Option<AgentDescriptor> {
        self.agents.read().await.get(id).cloned()
    }

    /// Take a snapshot for routing.
    pub async fn snapshot(&self) -> RegistrySnapshot {
        RegistrySnapshot::new(self.agents.read().await.values().cloned().collect())
    }

    /// Update an agent's health.
    pub async fn set_health(&self, id: &AgentId, state: HealthState, error: Option<String>) {
        let mut guard = self.agents.write().await;
        if let Some(agent) = guard.get_mut(id) {
            agent.health.state = state;
            if state == HealthState::Healthy {
                agent.health.consecutive_failures = 0;
                agent.health.last_success = Some(chrono::Utc::now());
                agent.health.last_error = None;
            } else if error.is_some() {
                agent.health.consecutive_failures += 1;
                agent.health.last_error = error;
            }
        }
    }

    /// Record an observed latency.
    pub async fn record_latency(&self, id: &AgentId, latency_ms: u64) {
        let mut guard = self.agents.write().await;
        if let Some(agent) = guard.get_mut(id) {
            agent.health.last_latency_ms = cuma_core::Known::Reported(latency_ms);
        }
    }

    /// Enable or disable an agent.
    pub async fn set_enabled(&self, id: &AgentId, enabled: bool) {
        let mut guard = self.agents.write().await;
        if let Some(agent) = guard.get_mut(id) {
            agent.enabled = enabled;
        }
    }

    /// Run every discovery source and register what they find.
    ///
    /// One failing source does not abort discovery: an unreachable A2A
    /// endpoint should not stop locally configured ACP agents from loading.
    /// Failures are returned alongside the count so the caller can report them.
    pub async fn discover(
        &self,
        sources: &[Arc<dyn AgentDiscovery>],
    ) -> (usize, Vec<(String, String)>) {
        let mut discovered = 0usize;
        let mut failures = Vec::new();

        for source in sources {
            match source.discover().await {
                Ok(agents) => {
                    discovered += agents.len();
                    self.register_all(agents).await;
                }
                Err(err) => {
                    tracing::warn!(
                        source = source.source_name(),
                        error = %err,
                        "agent discovery source failed"
                    );
                    failures.push((source.source_name().to_owned(), err.to_string()));
                }
            }
        }

        (discovered, failures)
    }

    /// Agents reachable over a given protocol. For diagnostics only — routing
    /// must never filter on protocol.
    pub async fn by_protocol(&self, protocol: AgentProtocol) -> Vec<AgentDescriptor> {
        self.agents
            .read()
            .await
            .values()
            .filter(|a| a.protocol == protocol)
            .cloned()
            .collect()
    }

    /// Number of registered agents.
    pub async fn len(&self) -> usize {
        self.agents.read().await.len()
    }

    /// Whether nothing is registered.
    pub async fn is_empty(&self) -> bool {
        self.agents.read().await.is_empty()
    }
}

/// Build descriptors from the `[agents.*]` config block.
///
/// This is the "configuração manual" discovery source: it needs no network and
/// no running agent, so the harness is usable before anything is reachable.
pub fn descriptors_from_config(config: &cuma_config::Config) -> Result<Vec<AgentDescriptor>> {
    let mut agents = Vec::new();

    for (id, agent_config) in &config.agents {
        let protocol = match agent_config.protocol.to_ascii_lowercase().as_str() {
            "acp" => AgentProtocol::Acp,
            "a2a" => AgentProtocol::A2A,
            "native" => AgentProtocol::Native,
            other => {
                return Err(cuma_core::MetaAgentError::Configuration(format!(
                    "agent {id}: unknown protocol {other:?} (expected acp, a2a or native)"
                )));
            }
        };

        let capabilities: cuma_core::CapabilitySet = agent_config
            .capabilities
            .iter()
            .map(|c| cuma_core::Capability::parse(c))
            .collect();

        let mut descriptor = AgentDescriptor::new(id.as_str(), id.as_str(), protocol)
            .with_capabilities(capabilities);
        descriptor.enabled = agent_config.enabled;
        descriptor.metadata = agent_config.metadata.clone();

        if let Some(command) = &agent_config.command {
            descriptor
                .metadata
                .insert("command".to_owned(), command.clone());
        }
        if let Some(endpoint) = &agent_config.endpoint {
            descriptor
                .metadata
                .insert("endpoint".to_owned(), endpoint.clone());
        }

        descriptor.auth = match &agent_config.auth_secret_ref {
            Some(handle) => cuma_core::AgentAuth::SecretRef {
                handle: handle.clone(),
            },
            // Defaulting to agent-managed auth is what lets the harness reuse
            // an already-logged-in CLI without ever seeing a credential.
            None => cuma_core::AgentAuth::AgentManaged,
        };

        for model_name in &agent_config.models {
            descriptor.models.push(cuma_core::ModelDescriptor::minimal(
                descriptor.id.clone(),
                model_name.as_str(),
                model_name.as_str(),
            ));
        }

        agents.push(descriptor);
    }

    Ok(agents)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use cuma_core::{Capability, CapabilitySet, ModelDescriptor};

    fn agent(id: &str) -> AgentDescriptor {
        AgentDescriptor::new(id, id, AgentProtocol::Native)
            .with_capabilities(CapabilitySet::new().with(Capability::CodeEditing))
    }

    #[tokio::test]
    async fn registering_makes_an_agent_visible() {
        let registry = AgentRegistry::new();
        assert!(registry.is_empty().await);

        registry.register(agent("codex")).await;
        assert_eq!(registry.len().await, 1);
        assert!(registry.get(&AgentId::new("codex")).await.is_some());
    }

    #[tokio::test]
    async fn re_registering_preserves_health_history() {
        let registry = AgentRegistry::new();
        registry.register(agent("flaky")).await;

        let id = AgentId::new("flaky");
        registry
            .set_health(&id, HealthState::Unavailable, Some("crashed".into()))
            .await;

        // Discovery re-runs and re-registers a fresh descriptor.
        registry.register(agent("flaky")).await;

        let stored = registry.get(&id).await.unwrap();
        assert_eq!(
            stored.health.state,
            HealthState::Unavailable,
            "a rediscovery must not erase what we learned about this agent"
        );
    }

    #[tokio::test]
    async fn an_unhealthy_agent_is_excluded_from_the_routable_set() {
        let registry = AgentRegistry::new();
        registry.register(agent("good")).await;
        registry.register(agent("bad")).await;

        registry
            .set_health(&AgentId::new("bad"), HealthState::Unavailable, Some("x".into()))
            .await;

        let snapshot = registry.snapshot().await;
        assert_eq!(snapshot.len(), 2, "still registered");
        assert_eq!(snapshot.routable().count(), 1, "but not routable");
    }

    #[tokio::test]
    async fn recovering_clears_the_failure_counter() {
        let registry = AgentRegistry::new();
        registry.register(agent("a")).await;
        let id = AgentId::new("a");

        registry
            .set_health(&id, HealthState::Unavailable, Some("boom".into()))
            .await;
        registry.set_health(&id, HealthState::Healthy, None).await;

        let stored = registry.get(&id).await.unwrap();
        assert_eq!(stored.health.consecutive_failures, 0);
        assert!(stored.health.last_error.is_none());
        assert!(stored.health.last_success.is_some());
    }

    #[tokio::test]
    async fn available_capabilities_span_agents_and_their_models() {
        let registry = AgentRegistry::new();

        let mut with_vision = agent("multimodal");
        let mut model = ModelDescriptor::minimal(with_vision.id.clone(), "m", "M");
        model.capabilities.insert(Capability::Vision);
        with_vision.models.push(model);

        registry.register(with_vision).await;
        registry
            .register(
                AgentDescriptor::new("researcher", "researcher", AgentProtocol::A2A)
                    .with_capabilities(CapabilitySet::new().with(Capability::Research)),
            )
            .await;

        let caps = registry.snapshot().await.available_capabilities();
        assert!(caps.contains(&Capability::CodeEditing));
        assert!(caps.contains(&Capability::Vision));
        assert!(caps.contains(&Capability::Research));
    }

    #[tokio::test]
    async fn a_disabled_agent_contributes_no_capabilities() {
        let registry = AgentRegistry::new();
        registry.register(agent("off")).await;
        registry.set_enabled(&AgentId::new("off"), false).await;

        assert!(registry.snapshot().await.available_capabilities().is_empty());
    }

    #[test]
    fn config_without_an_explicit_secret_defaults_to_agent_managed_auth() {
        let config = cuma_config::Config::from_toml(
            r#"
            [agents.codex]
            protocol = "acp"
            command = "npx -y @agentclientprotocol/codex-acp@latest"
            capabilities = ["code_editing", "debugging"]
            "#,
        )
        .unwrap();

        let agents = descriptors_from_config(&config).unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].auth, cuma_core::AgentAuth::AgentManaged);
        assert_eq!(agents[0].protocol, AgentProtocol::Acp);
        assert!(agents[0].capabilities.contains(&Capability::Debugging));
        assert!(agents[0].metadata.contains_key("command"));
    }

    #[test]
    fn a_secret_ref_is_stored_as_a_handle_never_a_value() {
        let config = cuma_config::Config::from_toml(
            r#"
            [agents.remote]
            protocol = "a2a"
            endpoint = "https://example.invalid/a2a"
            auth_secret_ref = "cuma/remote/token"
            "#,
        )
        .unwrap();

        let agents = descriptors_from_config(&config).unwrap();
        match &agents[0].auth {
            cuma_core::AgentAuth::SecretRef { handle } => {
                assert_eq!(handle, "cuma/remote/token");
            }
            other => panic!("expected a secret handle, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_protocol_is_rejected_at_load_time() {
        let config =
            cuma_config::Config::from_toml("[agents.weird]\nprotocol = \"carrier-pigeon\"\n")
                .unwrap();
        assert!(descriptors_from_config(&config).is_err());
    }
}
