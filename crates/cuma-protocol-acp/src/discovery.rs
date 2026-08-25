//! Discovering ACP agents from configuration.
//!
//! ACP agents are launched, not found: the harness needs a command line, which
//! comes from `[agents.*]` in the config. Well-known agent names resolve to
//! their published adapters, so `protocol = "acp"` with no command still works
//! for `codex` and `claude-code`.

use crate::adapter::AcpAdapter;
use crate::capabilities::well_known_agent_command;
use async_trait::async_trait;
use cuma_config::Config;
use cuma_core::error::Result;
use cuma_core::ports::{AgentAdapter, AgentDiscovery};
use cuma_core::{AgentDescriptor, AgentProtocol};

/// Finds ACP agents declared in configuration.
pub struct AcpConfigDiscovery {
    config: Config,
    /// Whether to drop agents whose command is not on `PATH`.
    ///
    /// On by default: an agent the router can select but the machine cannot
    /// launch wins routing decisions and then fails every one of them.
    require_launchable: bool,
}

impl AcpConfigDiscovery {
    /// Discover from `config`.
    pub fn new(config: Config) -> Self {
        Self {
            config,
            require_launchable: true,
        }
    }

    /// Register agents even when their command is missing.
    #[must_use]
    pub fn allowing_unlaunchable(mut self) -> Self {
        self.require_launchable = false;
        self
    }

    /// Build adapters for every configured, launchable ACP agent.
    pub fn adapters(&self) -> Vec<AcpAdapter> {
        let mut adapters = Vec::new();

        for (id, agent_config) in &self.config.agents {
            if !agent_config.enabled || !agent_config.protocol.eq_ignore_ascii_case("acp") {
                continue;
            }

            let Some(command) = agent_config
                .command
                .clone()
                .or_else(|| well_known_agent_command(id).map(str::to_owned))
            else {
                tracing::warn!(
                    agent = id,
                    "ACP agent has no command and is not a well-known agent; skipping"
                );
                continue;
            };

            let adapter = AcpAdapter::new(id.as_str(), command);

            if self.require_launchable && !adapter.is_launchable() {
                tracing::info!(
                    agent = id,
                    "ACP agent's command is not on PATH; not registering it"
                );
                continue;
            }

            adapters.push(adapter);
        }

        adapters
    }
}

#[async_trait]
impl AgentDiscovery for AcpConfigDiscovery {
    fn source_name(&self) -> &str {
        "acp-config"
    }

    async fn discover(&self) -> Result<Vec<AgentDescriptor>> {
        let mut descriptors = Vec::new();

        for adapter in self.adapters() {
            // Interrogating a live agent is best effort. An agent that fails
            // to start is registered with its configured capabilities rather
            // than dropped, so `cuma agents list` can show it as unhealthy
            // instead of pretending it was never configured.
            match adapter.refresh_capabilities().await {
                Ok(descriptor) => descriptors.push(descriptor),
                Err(err) => {
                    tracing::warn!(
                        agent = %adapter.agent_id(),
                        error = %err,
                        "ACP capability negotiation failed; using configured capabilities"
                    );

                    let mut descriptor = adapter.describe().await?;
                    descriptor.health.state = cuma_core::HealthState::Unavailable;
                    descriptor.health.last_error = Some(err.to_string());
                    descriptor.protocol = AgentProtocol::Acp;
                    descriptors.push(descriptor);
                }
            }
        }

        Ok(descriptors)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn config(toml: &str) -> Config {
        Config::from_toml(toml).expect("test config should parse")
    }

    #[test]
    fn a_well_known_agent_needs_no_explicit_command() {
        let discovery = AcpConfigDiscovery::new(config("[agents.codex]\nprotocol = \"acp\"\n"))
            .allowing_unlaunchable();

        let adapters = discovery.adapters();
        assert_eq!(adapters.len(), 1);
        assert_eq!(adapters[0].agent_id(), &cuma_core::AgentId::new("codex"));
    }

    #[test]
    fn an_unknown_agent_without_a_command_is_skipped_rather_than_guessed_at() {
        let discovery = AcpConfigDiscovery::new(config(
            "[agents.mystery-agent]\nprotocol = \"acp\"\n",
        ))
        .allowing_unlaunchable();

        assert!(discovery.adapters().is_empty());
    }

    #[test]
    fn disabled_agents_are_not_launched() {
        let discovery = AcpConfigDiscovery::new(config(
            "[agents.codex]\nprotocol = \"acp\"\nenabled = false\n",
        ))
        .allowing_unlaunchable();

        assert!(discovery.adapters().is_empty());
    }

    #[test]
    fn agents_on_other_protocols_are_left_to_their_own_adapters() {
        let discovery = AcpConfigDiscovery::new(config(
            r#"
            [agents.remote]
            protocol = "a2a"
            endpoint = "https://example.invalid/a2a"
            [agents.codex]
            protocol = "acp"
            "#,
        ))
        .allowing_unlaunchable();

        let adapters = discovery.adapters();
        assert_eq!(adapters.len(), 1);
        assert_eq!(adapters[0].agent_id(), &cuma_core::AgentId::new("codex"));
    }

    #[test]
    fn an_agent_whose_command_is_missing_is_not_registered_by_default() {
        let discovery = AcpConfigDiscovery::new(config(
            "[agents.ghost]\nprotocol = \"acp\"\ncommand = \"not-a-real-binary-9f3a2b\"\n",
        ));

        assert!(
            discovery.adapters().is_empty(),
            "an unlaunchable agent would win routing decisions and then fail them"
        );
    }

    #[tokio::test]
    async fn discovery_reports_its_source_for_the_registry_to_log() {
        let discovery = AcpConfigDiscovery::new(Config::default());
        assert_eq!(discovery.source_name(), "acp-config");
        assert!(discovery.discover().await.unwrap().is_empty());
    }
}
