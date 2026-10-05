//! Wiring: configuration overrides, logging and harness assembly.

use cuma_config::{Config, RoutingStrategy};
use cuma_core::error::{MetaAgentError, Result};
use cuma_core::ports::AgentAdapter;
use cuma_orchestrator::Orchestrator;
use cuma_planner::HeuristicPlanner;
use cuma_protocol_a2a::A2aDiscovery;
use cuma_protocol_acp::AcpConfigDiscovery;
use std::collections::{BTreeMap, BTreeSet};
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

/// The subscriber every output layer sits on: the registry, behind the level
/// filter.
type Filtered =
    tracing_subscriber::layer::Layered<tracing_subscriber::EnvFilter, tracing_subscriber::Registry>;

/// One output layer: log lines, or exported spans.
type OutputLayer = Box<dyn tracing_subscriber::Layer<Filtered> + Send + Sync>;

/// Set up logging.
///
/// `--json` switches to structured output, which is what CI and other agents
/// want; a human at a terminal gets the readable formatter.
pub fn init_tracing(config: &Config, verbosity: u8, json: bool) -> Telemetry {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::util::SubscriberInitExt as _;

    let level = match verbosity {
        0 => config.telemetry.log_level.clone(),
        1 => "debug".to_owned(),
        _ => "trace".to_owned(),
    };

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("cuma={level},warn")));

    // Logs go to stderr so that `--json` output on stdout stays parseable when
    // both are enabled.
    #[cfg_attr(not(feature = "otel"), allow(unused_mut))]
    let mut layers = vec![log_layer(
        json || config.telemetry.json_logs,
        std::io::stderr,
    )];
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

    if subscriber(filter, layers).try_init().is_err() {
        // A second init in the same process is not an error worth failing on.
        tracing::debug!("a tracing subscriber was already installed");
    }
    if let Some(warning) = deferred_warning {
        tracing::warn!("{warning}");
    }
    telemetry
}

/// The registry with `filter` beneath every layer in `layers`.
///
/// The filter must not be one of `layers`: a `Vec` of layers takes the
/// highest interest any member expresses, so an unfiltered formatter beside
/// the filter would switch every callsite on — debug and trace from every
/// dependency, raw JSON-RPC traffic among them.
fn subscriber(
    filter: tracing_subscriber::EnvFilter,
    layers: Vec<OutputLayer>,
) -> impl tracing::Subscriber + Send + Sync {
    use tracing_subscriber::layer::SubscriberExt as _;
    tracing_subscriber::registry().with(filter).with(layers)
}

/// Log lines written to `writer`, as JSON or for a human.
fn log_layer<W>(json: bool, writer: W) -> OutputLayer
where
    W: for<'w> tracing_subscriber::fmt::MakeWriter<'w> + Send + Sync + 'static,
{
    use tracing_subscriber::Layer as _;
    if json {
        tracing_subscriber::fmt::layer()
            .json()
            .with_writer(writer)
            .boxed()
    } else {
        tracing_subscriber::fmt::layer()
            .with_target(false)
            .with_writer(writer)
            .boxed()
    }
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
    use tracing_subscriber::Layer as _;

    /// A tracing layer exporting spans to an OTLP/HTTP collector.
    pub(super) fn layer(
        endpoint: &str,
    ) -> Result<
        (
            super::OutputLayer,
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

/// The shortest bearer token the A2A server accepts.
const MIN_TOKEN_LEN: usize = 16;

/// Resolve the A2A server's bearer tokens from their handles.
///
/// A handle that resolves to nothing, or to something short enough to guess,
/// stops the server: starting open because a variable was unset is exactly
/// the failure authentication exists to prevent.
pub async fn a2a_server_tokens(config: &Config) -> Result<Vec<String>> {
    let secrets = cuma_providers::EnvSecretStore::new();
    let mut tokens = Vec::new();
    for handle in &config.security.a2a_server_token_refs {
        let token = cuma_core::ports::SecretStore::get(&secrets, handle)
            .await?
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty())
            .ok_or_else(|| {
                MetaAgentError::Configuration(format!(
                    "security.a2a_server_token_refs names {handle}, which is not set; \
                     refusing to serve A2A without the authentication it asks for"
                ))
            })?;
        if token.chars().count() < MIN_TOKEN_LEN {
            return Err(MetaAgentError::Configuration(format!(
                "the token in {handle} is shorter than {MIN_TOKEN_LEN} characters; \
                 generate one with e.g. `openssl rand -hex 32`"
            )));
        }
        tokens.push(token);
    }
    Ok(tokens)
}

/// Refuse to serve A2A beyond this machine without authentication, unless
/// the operator said a proxy in front takes care of it.
pub fn check_a2a_exposure(
    address: std::net::SocketAddr,
    authenticated: bool,
    allow_unauthenticated: bool,
) -> Result<()> {
    if authenticated || allow_unauthenticated || address.ip().is_loopback() {
        return Ok(());
    }
    Err(MetaAgentError::Configuration(format!(
        "refusing to serve A2A on {address} without authentication: anyone who can reach it \
         could run goals on this machine. Set security.a2a_server_token_refs, bind to \
         127.0.0.1, or pass --allow-unauthenticated if a proxy in front authenticates callers."
    )))
}

/// Why no agent is available, in words that point at the actual cause.
///
/// "Configure an agent" is wrong advice when agents are configured and were
/// refused because `security.require_agent_sandbox` cannot be met.
pub fn no_agents_reason(config: &Config) -> String {
    let sandboxes = sandbox_registry(config);
    no_agents_reason_under(config, &refusals(config, |id| sandboxes.for_agent(id)))
}

/// The enabled ACP agents of `config`.
fn local_agents(config: &Config) -> impl Iterator<Item = (&String, &cuma_config::AgentConfig)> {
    config
        .agents
        .iter()
        .filter(|(_, agent)| agent.enabled && agent.protocol.eq_ignore_ascii_case("acp"))
}

/// The local agents `security.require_agent_sandbox` refuses, each with what
/// its sandbox falls short of.
fn refusals(
    config: &Config,
    sandbox_for: impl Fn(&str) -> Option<Arc<dyn cuma_sandbox::SandboxProvider>>,
) -> BTreeMap<String, String> {
    if !config.security.require_agent_sandbox {
        return BTreeMap::new();
    }
    // With sandboxing off there is no sandbox to fall short: the operator
    // chose to run agents unconfined.
    local_agents(config)
        .filter_map(|(id, _)| {
            let shortfall = sandbox_for(id)?.shortfall(&config.security.network_allowlist)?;
            Some((id.clone(), shortfall))
        })
        .collect()
}

fn no_agents_reason_under(config: &Config, refused: &BTreeMap<String, String>) -> String {
    let local: Vec<&String> = local_agents(config).map(|(id, _)| id).collect();

    if !local.is_empty() && local.iter().all(|id| refused.contains_key(*id)) {
        let mut reasons: Vec<&str> = refused.values().map(String::as_str).collect();
        reasons.sort_unstable();
        reasons.dedup();
        format!(
            "no agents can run: security.require_agent_sandbox is set, and their sandboxes fall \
             short — {}. Choose a sandbox that can confine them (`cuma sandbox list`; on this \
             machine, ai-jail or bubblewrap, firejail, or sandbox-exec on macOS), empty \
             security.network_allowlist, or unset require_agent_sandbox.",
            reasons.join("; ")
        )
    } else if !local.is_empty() {
        "no agents are available: those configured under [agents.*] could not be \
         registered (see the warnings above). Run `cuma doctor` for details."
            .to_owned()
    } else {
        "no agents are available. Configure one under [agents.*] in .cuma/config.toml, \
         then run `cuma doctor` to check it."
            .to_owned()
    }
}

/// The sandboxes `config` declares, with CUMA's own executable for those
/// reached through `cuma sandbox exec`.
pub fn sandbox_registry(config: &Config) -> cuma_sandbox::Registry {
    cuma_sandbox::Registry::from_config(config, std::env::current_exe().ok())
}

/// Launches one ACP agent under its sandbox, confined to the directory each
/// launch works in.
struct ProviderLauncher {
    provider: Arc<dyn cuma_sandbox::SandboxProvider>,
    /// The agent's own state directories, expanded.
    state: Vec<PathBuf>,
    /// Variables it needs beyond what its command asks for: the agent's own
    /// `env` and `security.agent_env`.
    env: Vec<String>,
    allowed_hosts: Vec<String>,
}

impl ProviderLauncher {
    fn new(
        provider: Arc<dyn cuma_sandbox::SandboxProvider>,
        agent: &cuma_config::AgentConfig,
        security: &cuma_config::SecurityConfig,
    ) -> Self {
        let mut env = agent.env.clone();
        env.extend(security.agent_env.iter().cloned());
        Self {
            provider,
            state: agent
                .state
                .iter()
                .map(|p| cuma_config::expand_home(p))
                .collect(),
            env,
            allowed_hosts: security.network_allowlist.clone(),
        }
    }

    fn request(
        &self,
        workspace: &Path,
        purpose: cuma_core::ports::LaunchPurpose,
        keep_env: &[String],
        readable: &[PathBuf],
    ) -> cuma_sandbox::LaunchRequest {
        let mut keep = keep_env.to_vec();
        keep.extend(self.env.iter().cloned());
        cuma_sandbox::LaunchRequest {
            workspace: workspace.to_path_buf(),
            purpose,
            keep_env: keep,
            readable: readable.to_vec(),
            state: self.state.clone(),
            allowed_hosts: self.allowed_hosts.clone(),
        }
    }
}

#[async_trait::async_trait]
impl cuma_protocol_acp::AgentLauncher for ProviderLauncher {
    fn prefix(&self, workspace: &Path, keep_env: &[String], readable: &[PathBuf]) -> Vec<String> {
        self.provider.prefix_hint(&self.request(
            workspace,
            cuma_core::ports::LaunchPurpose::Execute,
            keep_env,
            readable,
        ))
    }

    fn runs_elsewhere(&self) -> bool {
        !matches!(
            self.provider.capabilities().isolation,
            cuma_sandbox::Isolation::Process
        )
    }

    async fn open(
        &self,
        workspace: &Path,
        purpose: cuma_core::ports::LaunchPurpose,
        keep_env: &[String],
        readable: &[PathBuf],
    ) -> Result<cuma_core::ports::SandboxLaunch> {
        self.provider
            .open(&self.request(workspace, purpose, keep_env, readable))
            .await
    }
}

/// The variables shared MCP servers take their secrets from.
///
/// An agent launches `cuma mcp proxy`, which resolves `$VAR` references from
/// its own environment — inherited from the agent — so a sandboxed agent
/// must keep them, as an unsandboxed one always did.
pub fn shared_mcp_secret_names(config: &Config) -> Vec<String> {
    let mut names: Vec<String> = config
        .mcp
        .values()
        .filter(|server| server.enabled && server.share_with_agents)
        .flat_map(|server| server.env.values())
        .filter_map(|value| value.strip_prefix('$'))
        .map(|name| {
            name.trim_start_matches('{')
                .trim_end_matches('}')
                .to_owned()
        })
        .filter(|name| !name.is_empty())
        .collect();
    names.sort();
    names.dedup();
    names
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
    // Each agent runs under its own sandbox, else the default one.
    let sandboxes = sandbox_registry(&config);
    let mut acp = AcpConfigDiscovery::new(config.clone())
        .with_mcp_servers(shared)
        .with_kept_env(shared_mcp_secret_names(&config));
    let mut reported = BTreeSet::new();
    for (id, agent) in local_agents(&config) {
        let Some(provider) = sandboxes.for_agent(id) else {
            continue;
        };
        if let Some(shortfall) = provider.shortfall(&config.security.network_allowlist)
            && reported.insert(provider.name().to_owned())
        {
            warnings.push(shortfall);
        }
        acp = acp.with_launcher_for(
            id,
            Arc::new(ProviderLauncher::new(provider, agent, &config.security)),
        );
    }
    // Required confinement that falls short: that agent does not run, rather
    // than running less confined than asked.
    let refused = refusals(&config, |id| sandboxes.for_agent(id));
    if !refused.is_empty() {
        let names: Vec<&str> = refused.keys().map(String::as_str).collect();
        warnings.push(format!(
            "security.require_agent_sandbox is set and {} cannot be fully confined here; \
             not registered",
            names.join(", ")
        ));
    }
    let acp_adapters: Vec<_> = acp
        .adapters()
        .into_iter()
        .filter(|adapter| !refused.contains_key(adapter.agent_id().as_str()))
        .collect();
    for adapter in acp_adapters {
        // Negotiate capabilities where possible; register with configured
        // capabilities where not.
        let failure = adapter
            .refresh_capabilities()
            .await
            .err()
            .map(|err| format!("ACP negotiation failed ({err})"));
        register(&mut orchestrator, Arc::new(adapter), failure, &mut warnings).await;
    }

    // --- A2A agents -------------------------------------------------------
    let a2a = A2aDiscovery::new(config.clone());
    for adapter in a2a.adapters() {
        let failure = adapter
            .refresh_from_card()
            .await
            .err()
            .map(|err| format!("could not fetch the Agent Card ({err})"));
        register(&mut orchestrator, Arc::new(adapter), failure, &mut warnings).await;
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
            if let Err(err) = cuma_workspace::git::ensure_state_ignored(&workspace) {
                tracing::debug!(error = %err, "could not write .cuma/.gitignore");
            }
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

/// Register `adapter`, as unavailable when discovering it failed.
///
/// An agent that could not be interrogated stays visible — `cuma agents list`
/// and `cuma doctor` show it, and why — but is not routed to: what failed was
/// the same launch or request every task sent to it would make.
async fn register(
    orchestrator: &mut Orchestrator,
    adapter: Arc<dyn AgentAdapter>,
    failure: Option<String>,
    warnings: &mut Vec<String>,
) {
    let id = adapter.agent_id().clone();
    if let Err(err) = orchestrator.add_agent(adapter).await {
        warnings.push(format!("{id}: could not register ({err})"));
        return;
    }
    if let Some(reason) = failure {
        orchestrator
            .agents()
            .set_health(
                &id,
                cuma_core::HealthState::Unavailable,
                Some(reason.clone()),
            )
            .await;
        warnings.push(format!("{id}: {reason}"));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    /// Log output captured in memory.
    #[derive(Clone, Default)]
    struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn the_log_level_holds_for_every_crate_not_only_cumas_own() {
        let captured = Captured::default();
        let sink = captured.clone();
        let subscriber = subscriber(
            tracing_subscriber::EnvFilter::new("cuma=info,warn"),
            vec![log_layer(false, move || sink.clone())],
        );

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "cuma_router", "kept: cuma at info");
            tracing::warn!(target: "hyper", "kept: a dependency's warning");
            tracing::debug!(target: "cuma_router", "dropped: cuma at debug");
            tracing::trace!(target: "agent_client_protocol::jsonrpc", "dropped: raw JSON-RPC");
        });

        let logged = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        assert!(logged.contains("kept: cuma at info"), "{logged}");
        assert!(logged.contains("kept: a dependency's warning"), "{logged}");
        assert!(!logged.contains("dropped"), "{logged}");
    }

    #[tokio::test]
    async fn an_agent_that_cannot_negotiate_is_unavailable_and_keeps_what_was_configured() {
        let workspace = tempfile::tempdir().unwrap();
        let mut config = Config::from_toml(
            r#"
            [agents.broken]
            protocol = "acp"
            command = "false"
            capabilities = ["documentation"]
            models = ["house-model"]
            "#,
        )
        .unwrap();
        config.security.sandbox = false;

        let (orchestrator, warnings) = build_orchestrator(config, workspace.path().to_path_buf())
            .await
            .unwrap();

        let agent = orchestrator
            .agents()
            .get(&cuma_core::AgentId::new("broken"))
            .await
            .unwrap();
        assert!(!agent.is_routable(), "{:?}", agent.health.state);
        let error = agent.health.last_error.unwrap_or_default();
        assert!(error.contains("negotiation failed"), "{error}");
        assert!(
            agent
                .capabilities
                .contains(&cuma_core::Capability::Documentation)
        );
        assert_eq!(agent.models.len(), 1);
        assert!(
            warnings.iter().any(|w| w.starts_with("broken:")),
            "{warnings:?}"
        );
    }

    #[test]
    fn a2a_is_served_openly_only_on_loopback_or_when_explicitly_allowed() {
        let local: std::net::SocketAddr = "127.0.0.1:8420".parse().unwrap();
        let local6: std::net::SocketAddr = "[::1]:8420".parse().unwrap();
        let exposed: std::net::SocketAddr = "0.0.0.0:8420".parse().unwrap();

        assert!(check_a2a_exposure(local, false, false).is_ok());
        assert!(check_a2a_exposure(local6, false, false).is_ok());
        let refused = check_a2a_exposure(exposed, false, false)
            .unwrap_err()
            .to_string();
        assert!(refused.contains("a2a_server_token_refs"), "{refused}");
        assert!(check_a2a_exposure(exposed, true, false).is_ok());
        assert!(check_a2a_exposure(exposed, false, true).is_ok());
    }

    #[tokio::test]
    async fn a_token_handle_that_is_unset_or_weak_stops_the_server() {
        let mut config = Config::default();
        config.security.a2a_server_token_refs = vec!["CUMA_TEST_SURELY_UNSET_7c1d".into()];
        let err = a2a_server_tokens(&config).await.unwrap_err().to_string();
        assert!(err.contains("not set"), "{err}");

        // Cargo sets these for every test binary: one short enough to be
        // refused, one long enough to pass.
        config.security.a2a_server_token_refs = vec!["CARGO_PKG_VERSION_MAJOR".into()];
        let err = a2a_server_tokens(&config).await.unwrap_err().to_string();
        assert!(err.contains("shorter"), "{err}");

        config.security.a2a_server_token_refs = vec!["CARGO_MANIFEST_DIR".into()];
        assert_eq!(a2a_server_tokens(&config).await.unwrap().len(), 1);
    }

    /// A native provider with `runtime` fixed, whatever this machine has.
    fn native(
        config: &Config,
        runtime: Option<cuma_workspace::AgentRuntime>,
    ) -> Arc<dyn cuma_sandbox::SandboxProvider> {
        Arc::new(cuma_sandbox::native::NativeProvider::with_sandbox(
            "auto",
            cuma_workspace::AgentSandbox::with_runtime(&config.security, runtime),
        ))
    }

    #[test]
    fn refused_agents_are_explained_as_refused_not_as_missing() {
        let mut config =
            Config::from_toml("[agents.probe]\nprotocol = \"acp\"\ncommand = \"probe\"\n").unwrap();
        config.security.require_agent_sandbox = true;
        // No sandbox runtime on this imagined machine.
        let nothing = native(&config, None);
        let refused = refusals(&config, |_| Some(Arc::clone(&nothing)));
        let reason = no_agents_reason_under(&config, &refused);
        assert!(reason.contains("require_agent_sandbox"), "{reason}");
        assert!(reason.contains("works here"), "{reason}");

        // An allowlist only ai-jail can enforce is its own reason.
        let mut filtered = config.clone();
        filtered.security.network_allowlist = vec!["api.anthropic.com".into()];
        let bwrap = native(&filtered, Some(cuma_workspace::AgentRuntime::Bubblewrap));
        let refused = refusals(&filtered, |_| Some(Arc::clone(&bwrap)));
        let reason = no_agents_reason_under(&filtered, &refused);
        assert!(reason.contains("network_allowlist"), "{reason}");
        assert!(!reason.contains("Configure one"), "{reason}");

        let empty = Config::default();
        assert!(no_agents_reason_under(&empty, &BTreeMap::new()).contains("Configure one"));

        config.security.require_agent_sandbox = false;
        let unrequired = refusals(&config, |_| Some(Arc::clone(&nothing)));
        assert!(unrequired.is_empty());
        assert!(no_agents_reason_under(&config, &unrequired).contains("could not be registered"));

        // With sandboxing off there is nothing to fall short.
        config.security.require_agent_sandbox = true;
        assert!(refusals(&config, |_| None).is_empty());
    }

    #[test]
    fn only_the_agent_whose_sandbox_falls_short_is_refused() {
        let config = Config::from_toml(
            r#"
            [security]
            require_agent_sandbox = true
            network_allowlist = ["api.anthropic.com"]
            [sandboxes.vm]
            kind = "microsandbox"
            image = "node:22"
            [sandboxes.box]
            kind = "docker"
            image = "node:22"
            [agents.filtered]
            command = "a"
            sandbox = "vm"
            [agents.open]
            command = "b"
            sandbox = "box"
            "#,
        )
        .unwrap();
        config.validate().unwrap();
        let sandboxes = sandbox_registry(&config);

        let refused = refusals(&config, |id| sandboxes.for_agent(id));

        // microsandbox enforces the allowlist; a container engine cannot.
        assert_eq!(refused.keys().collect::<Vec<_>>(), ["open"]);
        assert!(
            refused["open"].contains("NOT enforced"),
            "{}",
            refused["open"]
        );
    }

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
