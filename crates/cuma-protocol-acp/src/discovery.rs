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
use std::sync::Arc;

/// Finds ACP agents declared in configuration.
pub struct AcpConfigDiscovery {
    config: Config,
    /// Whether to drop agents whose command is not on `PATH`.
    ///
    /// On by default: an agent the router can select but the machine cannot
    /// launch wins routing decisions and then fails every one of them.
    require_launchable: bool,
    /// MCP servers every discovered agent is offered.
    mcp_servers: Vec<crate::SharedMcpServer>,
    /// A sandbox launcher every discovered agent is started under.
    launcher: Option<Arc<dyn crate::AgentLauncher>>,
    /// Launchers for particular agents, in place of `launcher`.
    agent_launchers: std::collections::BTreeMap<String, Arc<dyn crate::AgentLauncher>>,
    kept_env: Vec<String>,
}

impl AcpConfigDiscovery {
    /// Discover from `config`.
    pub fn new(config: Config) -> Self {
        Self {
            config,
            require_launchable: true,
            mcp_servers: Vec::new(),
            launcher: None,
            agent_launchers: std::collections::BTreeMap::new(),
            kept_env: Vec::new(),
        }
    }

    /// Start every discovered agent through `launcher`.
    #[must_use]
    pub fn with_launcher(mut self, launcher: Arc<dyn crate::AgentLauncher>) -> Self {
        self.launcher = Some(launcher);
        self
    }

    /// Start agent `id` through `launcher`, whatever the others use.
    #[must_use]
    pub fn with_launcher_for(mut self, id: &str, launcher: Arc<dyn crate::AgentLauncher>) -> Self {
        self.agent_launchers.insert(id.to_owned(), launcher);
        self
    }

    /// Environment variables sandboxed agents keep beyond the baseline.
    #[must_use]
    pub fn with_kept_env(mut self, names: Vec<String>) -> Self {
        self.kept_env = names;
        self
    }

    /// Offer `servers` to every agent discovered.
    #[must_use]
    pub fn with_mcp_servers(mut self, servers: Vec<crate::SharedMcpServer>) -> Self {
        self.mcp_servers = servers;
        self
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

            // What the operator stated about the agent, kept through
            // negotiation and in its place when negotiation fails.
            let mut descriptor = AgentDescriptor::new(id.as_str(), id.as_str(), AgentProtocol::Acp);
            agent_config.apply_to(&mut descriptor);

            let adapter = AcpAdapter::new(id.as_str(), command)
                .with_descriptor(descriptor)
                .with_mcp_servers(self.mcp_servers.clone())
                .with_kept_env(self.kept_env.clone());
            let adapter = match self.agent_launchers.get(id).or(self.launcher.as_ref()) {
                Some(launcher) => adapter.with_launcher(Arc::clone(launcher)),
                None => adapter,
            };

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
    use cuma_core::Capability;

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
        let discovery =
            AcpConfigDiscovery::new(config("[agents.mystery-agent]\nprotocol = \"acp\"\n"))
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

    /// The minimal ACP agent fixture, configured with `documentation` and a
    /// model, or `None` without python3.
    fn minimal_agent() -> Option<Config> {
        which::which("python3").ok()?;
        let script = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/minimal_agent.py"
        );
        let mut config = Config::default();
        config.agents.insert(
            "minimal".into(),
            cuma_config::AgentConfig {
                command: Some(shell_words::join(["python3", script])),
                capabilities: vec!["documentation".into()],
                models: vec!["house-model".into()],
                ..cuma_config::AgentConfig::default()
            },
        );
        Some(config)
    }

    #[tokio::test]
    async fn negotiation_adds_the_acp_baseline_to_what_the_operator_configured() {
        let Some(config) = minimal_agent() else {
            return;
        };
        let adapter = AcpConfigDiscovery::new(config).adapters().remove(0);

        let descriptor = adapter.refresh_capabilities().await.unwrap();

        assert_eq!(descriptor.name, "minimal", "named by the agent itself");
        assert!(
            descriptor.capabilities.contains(&Capability::CodeEditing),
            "negotiated"
        );
        assert!(
            descriptor.capabilities.contains(&Capability::Documentation),
            "configured, and beyond what ACP can say"
        );
        assert_eq!(descriptor.models.len(), 1, "ACP enumerates no models");
    }

    #[tokio::test]
    async fn an_agent_that_fails_to_start_keeps_what_the_operator_configured() {
        let discovery = AcpConfigDiscovery::new(config(
            "[agents.broken]\nprotocol = \"acp\"\ncommand = \"false\"\ncapabilities = [\"documentation\"]\n",
        ));

        let descriptors = discovery.discover().await.unwrap();

        assert_eq!(descriptors.len(), 1);
        assert!(!descriptors[0].is_routable());
        assert!(
            descriptors[0]
                .capabilities
                .contains(&Capability::Documentation)
        );
    }
}
