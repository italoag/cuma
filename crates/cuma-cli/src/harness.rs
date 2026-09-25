//! Wiring: configuration overrides, logging and harness assembly.

use cuma_config::{Config, RoutingStrategy};
use cuma_core::error::{MetaAgentError, Result};
use cuma_core::ports::AgentAdapter;
use cuma_orchestrator::Orchestrator;
use cuma_planner::HeuristicPlanner;
use cuma_protocol_a2a::A2aDiscovery;
use cuma_protocol_acp::AcpConfigDiscovery;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The command-line flags that override configuration, kept so they can be
/// applied again to the configuration of another workspace.
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    /// `--strategy`.
    pub strategy: Option<String>,
    /// `--agent`.
    pub agent: Option<String>,
    /// `--model`.
    pub model: Option<String>,
    /// `--max-cost`.
    pub max_cost: Option<f64>,
}

impl CliOverrides {
    /// Apply these flags to `config`.
    pub fn apply(&self, config: &mut Config) -> Result<()> {
        apply_cli_overrides(
            config,
            self.strategy.as_deref(),
            self.agent.as_deref(),
            self.model.as_deref(),
            self.max_cost,
        )
    }
}

/// Whether a workspace's own `.cuma/config.toml` may be applied.
///
/// The directory CUMA was started in is trusted — someone chose to run it
/// there — and so is anything under `security.trusted_workspaces`. Paths are
/// compared canonically, so a symlink cannot smuggle a directory into trust.
pub fn is_trusted_workspace(config: &Config, started_in: &Path, workspace: &Path) -> bool {
    let canonical =
        |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let workspace = canonical(workspace);
    if workspace == canonical(started_in) {
        return true;
    }
    config
        .security
        .trusted_workspaces
        .iter()
        .map(|entry| canonical(&cuma_config::expand_home(entry)))
        .any(|root| workspace.starts_with(root))
}

/// The configuration a session in `workspace` is served with, and a warning
/// when the workspace's own configuration was set aside.
///
/// `server` is the configuration CUMA started with, flags included. A trusted
/// workspace is loaded through the usual layers, its own file among them; an
/// untrusted one is served with `server`, because its file could name any
/// command as an agent and opening a folder in an editor must not run it.
pub fn workspace_config(
    server: &Config,
    started_in: &Path,
    workspace: &Path,
    overrides: &CliOverrides,
) -> Result<(Config, Option<String>)> {
    if is_trusted_workspace(server, started_in, workspace) {
        let mut config = Config::load(workspace)?.config;
        overrides.apply(&mut config)?;
        return Ok((config, None));
    }

    let project = workspace.join(".cuma").join("config.toml");
    let warning = project.exists().then(|| {
        format!(
            "{} was not applied: {} is not a trusted workspace. Add it to \
             security.trusted_workspaces in your own configuration to use it.",
            project.display(),
            workspace.display()
        )
    });
    Ok((server.clone(), warning))
}

/// Apply CLI flags over the loaded configuration.
///
/// This is the top layer of the precedence chain documented in
/// `docs/CONFIGURATION.md`: defaults, then global file, then project file,
/// then environment, then these.
pub fn apply_cli_overrides(
    config: &mut Config,
    strategy: Option<&str>,
    agent: Option<&str>,
    model: Option<&str>,
    max_cost: Option<f64>,
) -> Result<()> {
    if let Some(strategy) = strategy {
        let parsed = match strategy.to_ascii_lowercase().replace('_', "-").as_str() {
            "balanced" => RoutingStrategy::Balanced,
            "quality-first" | "quality" => RoutingStrategy::QualityFirst,
            "cost-first" | "cost" => RoutingStrategy::CostFirst,
            "latency-first" | "latency" => RoutingStrategy::LatencyFirst,
            "local-first" | "local" => RoutingStrategy::LocalFirst,
            "privacy-first" | "privacy" => RoutingStrategy::PrivacyFirst,
            "manual" => RoutingStrategy::Manual,
            other => {
                return Err(MetaAgentError::Configuration(format!(
                    "unknown routing strategy {other:?}; expected one of balanced, \
                     quality-first, cost-first, latency-first, local-first, privacy-first, manual"
                )));
            }
        };

        config.router.strategy = parsed;
        config.router.weights = parsed.default_weights();
    }

    if let Some(agent) = agent {
        config.router.pin_agent = Some(agent.to_owned());
    }

    if let Some(model) = model {
        config.router.pin_model = Some(model.to_owned());
    }

    if let Some(max_cost) = max_cost {
        if max_cost <= 0.0 {
            return Err(MetaAgentError::Configuration(
                "--max-cost must be positive".to_owned(),
            ));
        }
        config.limits.max_cost_usd = Some(max_cost);
    }

    config.validate()
}

/// Set up logging.
///
/// `--json` switches to structured output, which is what CI and other agents
/// want; a human at a terminal gets the readable formatter.
pub fn init_tracing(config: &Config, verbosity: u8, json: bool) -> Telemetry {
    use tracing_subscriber::Layer as _;
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;
    use tracing_subscriber::{EnvFilter, Registry};

    let level = match verbosity {
        0 => config.telemetry.log_level.clone(),
        1 => "debug".to_owned(),
        _ => "trace".to_owned(),
    };

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("cuma={level},warn")));

    // Logs go to stderr so that `--json` output on stdout stays parseable when
    // both are enabled.
    let fmt_layer = if json || config.telemetry.json_logs {
        tracing_subscriber::fmt::layer()
            .json()
            .with_writer(std::io::stderr)
            .boxed()
    } else {
        tracing_subscriber::fmt::layer()
            .with_target(false)
            .with_writer(std::io::stderr)
            .boxed()
    };

    #[cfg_attr(not(feature = "otel"), allow(unused_mut))]
    let mut layers: Vec<Box<dyn tracing_subscriber::Layer<Registry> + Send + Sync>> =
        vec![filter.boxed(), fmt_layer];
    #[cfg_attr(not(feature = "otel"), allow(unused_mut))]
    let mut telemetry = Telemetry::default();
    let mut deferred_warning = None;

    if let Some(endpoint) = &config.telemetry.otlp_endpoint {
        #[cfg(feature = "otel")]
        match otel::layer(endpoint) {
            Ok((layer, provider)) => {
                layers.push(layer);
                telemetry.provider = Some(provider);
            }
            Err(err) => deferred_warning = Some(format!("OTLP export is off: {err}")),
        }
        #[cfg(not(feature = "otel"))]
        {
            deferred_warning = Some(format!(
                "telemetry.otlp_endpoint = {endpoint:?} is set, but this build has no OTLP \
                 support; rebuild with `--features otel`"
            ));
        }
    }

    if tracing_subscriber::registry()
        .with(layers)
        .try_init()
        .is_err()
    {
        // A second init in the same process is not an error worth failing on.
        tracing::debug!("a tracing subscriber was already installed");
    }
    if let Some(warning) = deferred_warning {
        tracing::warn!("{warning}");
    }
    telemetry
}

/// Keeps trace export alive for the process, and flushes it on the way out.
#[derive(Default)]
pub struct Telemetry {
    #[cfg(feature = "otel")]
    provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        #[cfg(feature = "otel")]
        if let Some(provider) = self.provider.take() {
            // Spans still batched would otherwise be lost with the process.
            if let Err(err) = provider.shutdown() {
                eprintln!("warning: could not flush traces: {err}");
            }
        }
    }
}

#[cfg(feature = "otel")]
mod otel {
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_otlp::WithExportConfig as _;
    use tracing_subscriber::{Layer as _, Registry};

    /// A tracing layer exporting spans to an OTLP/HTTP collector.
    pub(super) fn layer(
        endpoint: &str,
    ) -> Result<
        (
            Box<dyn tracing_subscriber::Layer<Registry> + Send + Sync>,
            opentelemetry_sdk::trace::SdkTracerProvider,
        ),
        String,
    > {
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_endpoint(endpoint)
            .build()
            .map_err(|err| err.to_string())?;

        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_resource(
                opentelemetry_sdk::Resource::builder()
                    .with_service_name("cuma")
                    .build(),
            )
            .build();

        let layer = tracing_opentelemetry::layer()
            .with_tracer(provider.tracer("cuma"))
            .boxed();
        Ok((layer, provider))
    }
}

/// Where the runtime database lives.
pub fn database_path(config: &Config, workspace: &Path) -> PathBuf {
    config
        .telemetry
        .database_path
        .as_ref()
        .map_or_else(|| workspace.join(".cuma").join("runtime.db"), PathBuf::from)
}

/// The MCP servers to hand every ACP agent.
///
/// Each is declared as `cuma mcp proxy <name>` rather than as the server's
/// own command: the agent then reaches it through CUMA, which enforces the
/// server's allowlist and resolves its secrets from CUMA's environment, so
/// neither the allowlist nor a token depends on the agent behaving.
pub fn shared_mcp_servers(
    config: &Config,
    workspace: &Path,
) -> Vec<cuma_protocol_acp::SharedMcpServer> {
    let registry = cuma_protocol_mcp::McpServerRegistry::from_config(config);
    if registry.shared().next().is_none() {
        return Vec::new();
    }

    let Ok(cuma) = std::env::current_exe() else {
        tracing::warn!("cannot locate the cuma executable; MCP servers will not be shared");
        return Vec::new();
    };

    registry
        .shared()
        .map(|(name, _)| {
            let (command, args) = cuma_protocol_mcp::shared_server_command(&cuma, workspace, name);
            cuma_protocol_acp::SharedMcpServer {
                name: name.clone(),
                command,
                args,
            }
        })
        .collect()
}

/// Build the long-term memory store the configuration asks for.
///
/// `ai-memory-mcp` talks to `ai-memory serve --transport stdio` (or the
/// `[mcp.<name>]` server `memory.mcp_server` names); everything else is
/// handled by [`cuma_memory::from_config`].
pub fn memory_store(config: &Config, workspace: &Path) -> Arc<dyn cuma_core::ports::MemoryStore> {
    let memory = &config.memory;
    if !memory.enabled || !matches!(memory.backend.as_str(), "ai-memory-mcp" | "mcp") {
        return cuma_memory::from_config(memory);
    }

    let registry = match &memory.mcp_server {
        Some(name) => cuma_protocol_mcp::McpServerRegistry::from_config(config)
            .only(name)
            .unwrap_or_default(),
        None => {
            let command = memory
                .command
                .clone()
                .unwrap_or_else(|| "ai-memory".to_owned());
            let mut registry = cuma_protocol_mcp::McpServerRegistry::new();
            registry.add(
                "ai-memory",
                cuma_protocol_mcp::McpServerConfig::new(command)
                    .arg("serve")
                    .arg("--transport")
                    .arg("stdio"),
            );
            registry
        }
    };

    let tools: Arc<dyn cuma_core::ports::ToolProvider> =
        Arc::new(cuma_protocol_mcp::McpToolProvider::new(registry));
    Arc::new(cuma_memory::AiMemoryMcp::new(tools).in_workspace(workspace))
}

/// Build a fully wired orchestrator: agents discovered, memory attached,
/// history restored.
///
/// Discovery failures are reported and survived. A harness that refuses to
/// start because one configured agent is unreachable would be unusable on any
/// machine with a partial setup, which is most of them.
pub async fn build_orchestrator(
    config: Config,
    workspace: PathBuf,
) -> Result<(Orchestrator, Vec<String>)> {
    let mut warnings = Vec::new();

    // --- planner ----------------------------------------------------------
    // A configured LLM provider upgrades the planner from keyword matching to
    // model-assisted decomposition. `LlmPlanner` falls back to the heuristic
    // one on any failure, so this is strictly additive.
    let secrets: Arc<dyn cuma_core::ports::SecretStore> =
        Arc::new(cuma_providers::EnvSecretStore::new());
    let providers = cuma_providers::from_config(&config, Arc::clone(&secrets));

    let planner: Arc<dyn cuma_core::ports::Planner> = match providers.into_iter().next() {
        Some(provider) => {
            tracing::info!(provider = provider.name(), "using model-assisted planning");
            Arc::new(cuma_planner::LlmPlanner::new(provider))
        }
        None => Arc::new(HeuristicPlanner::new()),
    };

    let mut orchestrator = Orchestrator::new(config.clone(), planner, workspace.clone());

    // --- ACP agents -------------------------------------------------------
    let shared = shared_mcp_servers(&config, &workspace);
    if !shared.is_empty() {
        tracing::info!(count = shared.len(), "offering MCP servers to ACP agents");
    }
    let sandbox = cuma_workspace::Sandbox::detect(&config.security);
    let confinement = cuma_workspace::AgentConfinement::from_config(&config.security);
    let launch_prefix = sandbox
        .agent_launch_prefix(&workspace, &confinement)
        .unwrap_or_default();
    if config.security.sandbox && launch_prefix.is_empty() {
        warnings.push(sandbox.describe_agent_confinement());
    }
    let acp = AcpConfigDiscovery::new(config.clone())
        .with_mcp_servers(shared)
        .with_launch_prefix(launch_prefix);
    for adapter in acp.adapters() {
        let id = adapter.agent_id().clone();

        // Negotiate capabilities where possible; register with configured
        // capabilities where not.
        if let Err(err) = adapter.refresh_capabilities().await {
            warnings.push(format!("{id}: ACP negotiation failed ({err})"));
        }

        if let Err(err) = orchestrator.add_agent(Arc::new(adapter)).await {
            warnings.push(format!("{id}: could not register ({err})"));
        }
    }

    // --- A2A agents -------------------------------------------------------
    let a2a = A2aDiscovery::new(config.clone());
    for adapter in a2a.adapters() {
        let id = adapter.agent_id().clone();

        if let Err(err) = adapter.refresh_from_card().await {
            warnings.push(format!("{id}: could not fetch the Agent Card ({err})"));
        }

        if let Err(err) = orchestrator.add_agent(Arc::new(adapter)).await {
            warnings.push(format!("{id}: could not register ({err})"));
        }
    }

    // --- memory -----------------------------------------------------------
    let memory = memory_store(&config, &workspace);
    if config.memory.enabled && !memory.is_available().await {
        warnings.push(
            "long-term memory is enabled but its backend is not reachable; \
             running without recall"
                .to_owned(),
        );
    }
    let mut orchestrator = orchestrator.with_memory(memory);

    // --- skills -----------------------------------------------------------
    if config.skills.enabled {
        match cuma_skills::from_config(&config.skills, &workspace) {
            Ok((skills, skill_warnings)) => {
                warnings.extend(skill_warnings);
                orchestrator = orchestrator.with_skill_guidance(Arc::new(skills));
            }
            Err(err) => warnings.push(format!("skills are unavailable: {err}")),
        }
    }

    // --- runtime database -------------------------------------------------
    // A fresh process should route with everything previous sessions learned,
    // and every session — whichever front end started it — is recorded as it
    // happens so the next process can learn from this one.
    match cuma_persistence::RuntimeStore::open(&database_path(&config, &workspace)) {
        Ok(store) => {
            match store.load_routing_history() {
                Ok(history) => orchestrator = orchestrator.with_history(history),
                Err(err) => warnings.push(format!("could not load routing history: {err}")),
            }
            orchestrator =
                orchestrator.with_recorder(Arc::new(crate::recorder::StoreRecorder::new(store)));
        }
        Err(err) => warnings.push(format!("could not open the runtime database: {err}")),
    }

    Ok((orchestrator, warnings))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn only_the_start_directory_and_listed_roots_are_trusted() {
        let started = tempfile::tempdir().unwrap();
        let trusted_root = tempfile::tempdir().unwrap();
        let inside = trusted_root.path().join("project");
        std::fs::create_dir_all(&inside).unwrap();
        let stranger = tempfile::tempdir().unwrap();

        let mut config = Config::default();
        config.security.trusted_workspaces = vec![trusted_root.path().display().to_string()];

        assert!(is_trusted_workspace(
            &config,
            started.path(),
            started.path()
        ));
        assert!(is_trusted_workspace(&config, started.path(), &inside));
        assert!(!is_trusted_workspace(
            &config,
            started.path(),
            stranger.path()
        ));
    }

    #[test]
    fn an_untrusted_workspace_is_served_with_cumas_own_configuration() {
        let started = tempfile::tempdir().unwrap();
        let stranger = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(stranger.path().join(".cuma")).unwrap();
        std::fs::write(
            stranger.path().join(".cuma/config.toml"),
            "[agents.evil]\nprotocol = \"acp\"\ncommand = \"touch /tmp/pwned\"\n",
        )
        .unwrap();

        let server = Config::default();
        let (config, warning) = workspace_config(
            &server,
            started.path(),
            stranger.path(),
            &CliOverrides::default(),
        )
        .unwrap();
        assert!(
            !config.agents.contains_key("evil"),
            "a stranger's agents are never loaded"
        );
        assert!(warning.unwrap().contains("trusted_workspaces"));

        // Trusting it applies the file.
        let mut server = Config::default();
        server.security.trusted_workspaces = vec![stranger.path().display().to_string()];
        let (config, warning) = workspace_config(
            &server,
            started.path(),
            stranger.path(),
            &CliOverrides::default(),
        )
        .unwrap();
        assert!(config.agents.contains_key("evil"));
        assert!(warning.is_none());
    }

    #[test]
    fn command_line_flags_still_apply_to_a_trusted_workspace() {
        let started = tempfile::tempdir().unwrap();
        let overrides = CliOverrides {
            agent: Some("codex".into()),
            ..CliOverrides::default()
        };
        let (config, _) = workspace_config(
            &Config::default(),
            started.path(),
            started.path(),
            &overrides,
        )
        .unwrap();
        assert_eq!(config.router.pin_agent.as_deref(), Some("codex"));
    }

    #[test]
    fn a_strategy_flag_overrides_the_configured_strategy_and_its_weights() {
        let mut config = Config::default();
        assert_eq!(config.router.strategy, RoutingStrategy::Balanced);

        apply_cli_overrides(&mut config, Some("cost-first"), None, None, None).unwrap();

        assert_eq!(config.router.strategy, RoutingStrategy::CostFirst);
        assert!(config.router.weights.cost > config.router.weights.quality);
    }

    #[test]
    fn strategy_aliases_are_accepted() {
        for alias in ["cost", "cost-first", "COST_FIRST"] {
            let mut config = Config::default();
            apply_cli_overrides(&mut config, Some(alias), None, None, None).unwrap();
            assert_eq!(
                config.router.strategy,
                RoutingStrategy::CostFirst,
                "{alias}"
            );
        }
    }

    #[test]
    fn an_unknown_strategy_is_rejected_with_the_valid_options() {
        let mut config = Config::default();
        let err = apply_cli_overrides(&mut config, Some("cheapest"), None, None, None).unwrap_err();

        assert!(err.to_string().contains("cost-first"), "got: {err}");
    }

    #[test]
    fn agent_and_model_flags_become_pins() {
        let mut config = Config::default();
        apply_cli_overrides(&mut config, None, Some("codex"), Some("gpt-x"), None).unwrap();

        assert_eq!(config.router.pin_agent.as_deref(), Some("codex"));
        assert_eq!(config.router.pin_model.as_deref(), Some("gpt-x"));
    }

    #[test]
    fn a_cost_cap_becomes_a_budget() {
        let mut config = Config::default();
        apply_cli_overrides(&mut config, None, None, None, Some(2.50)).unwrap();
        assert_eq!(config.limits.max_cost_usd, Some(2.50));
    }

    #[test]
    fn a_non_positive_cost_cap_is_rejected() {
        let mut config = Config::default();
        assert!(apply_cli_overrides(&mut config, None, None, None, Some(0.0)).is_err());
        assert!(apply_cli_overrides(&mut config, None, None, None, Some(-1.0)).is_err());
    }

    #[test]
    fn no_flags_leaves_the_configuration_untouched() {
        let mut config = Config::default();
        let before = format!("{config:?}");

        apply_cli_overrides(&mut config, None, None, None, None).unwrap();
        assert_eq!(format!("{config:?}"), before);
    }

    #[test]
    fn the_database_defaults_to_the_project_directory() {
        let path = database_path(&Config::default(), Path::new("/projects/app"));
        assert_eq!(path, PathBuf::from("/projects/app/.cuma/runtime.db"));
    }

    #[test]
    fn an_explicit_database_path_wins() {
        let mut config = Config::default();
        config.telemetry.database_path = Some("/var/lib/cuma.db".to_owned());

        assert_eq!(
            database_path(&config, Path::new("/projects/app")),
            PathBuf::from("/var/lib/cuma.db")
        );
    }
}
