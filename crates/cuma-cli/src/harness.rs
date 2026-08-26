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
pub fn init_tracing(config: &Config, verbosity: u8, json: bool) {
    use tracing_subscriber::EnvFilter;

    let level = match verbosity {
        0 => config.telemetry.log_level.clone(),
        1 => "debug".to_owned(),
        _ => "trace".to_owned(),
    };

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("cuma={level},warn")));

    // Logs go to stderr so that `--json` output on stdout stays parseable when
    // both are enabled.
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr);

    let installed = if json || config.telemetry.json_logs {
        builder.json().try_init().is_ok()
    } else {
        builder.with_target(false).try_init().is_ok()
    };

    if !installed {
        // A second init in the same process is not an error worth failing on.
        tracing::debug!("a tracing subscriber was already installed");
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
    let acp = AcpConfigDiscovery::new(config.clone());
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
    let memory = cuma_memory::from_config(&config.memory);
    if config.memory.enabled && !memory.is_available().await {
        warnings.push(
            "long-term memory is enabled but its backend is not reachable; \
             running without recall"
                .to_owned(),
        );
    }
    let mut orchestrator = orchestrator.with_memory(memory);

    // --- routing history --------------------------------------------------
    // A fresh process should route with everything previous sessions learned.
    match cuma_persistence::RuntimeStore::open(&database_path(&config, &workspace)) {
        Ok(store) => match store.load_routing_history() {
            Ok(history) => orchestrator = orchestrator.with_history(history),
            Err(err) => warnings.push(format!("could not load routing history: {err}")),
        },
        Err(err) => warnings.push(format!("could not open the runtime database: {err}")),
    }

    Ok((orchestrator, warnings))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

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
