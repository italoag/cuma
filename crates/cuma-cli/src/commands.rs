//! The CLI subcommands.

use crate::harness;
use crate::output::{Table, USAGE_HEADERS, render_tokens, usage_row};
use clap::Subcommand;
use cuma_config::Config;
use cuma_core::error::{MetaAgentError, Result};
use cuma_core::{AgentId, EventKind, SkillId};
use std::path::PathBuf;
use std::sync::Arc;

/// Agent subcommands.
#[derive(Subcommand)]
pub enum AgentAction {
    /// List registered agents and their health.
    List,
    /// Re-run discovery and report what was found. With `--registry`, list
    /// the agents the ACP registry publishes instead.
    Discover {
        /// List the ACP registry's catalogue.
        #[arg(long)]
        registry: bool,
        /// A registry other than the official one.
        #[arg(long, value_name = "URL")]
        url: Option<String>,
    },
    /// Configure an agent from the ACP registry in `.cuma/config.toml`.
    Add {
        /// The agent's registry id.
        id: String,
        /// Use the preview channel.
        #[arg(long)]
        preview: bool,
        /// A registry other than the official one.
        #[arg(long, value_name = "URL")]
        url: Option<String>,
    },
    /// Show one agent in detail.
    Show {
        /// The agent's id.
        id: String,
    },
}

/// Model subcommands.
#[derive(Subcommand)]
pub enum ModelAction {
    /// List every model every agent exposes.
    List,
}

/// Skill subcommands.
#[derive(Subcommand)]
pub enum SkillAction {
    /// Search every configured registry.
    Search {
        /// What to search for.
        query: Vec<String>,
    },
    /// Fetch and verify a skill without installing it, and show the result.
    Inspect {
        /// The skill's id.
        id: String,
    },
    /// Install a skill after verifying it. The skill is enabled.
    Install {
        /// The skill's id.
        id: String,
    },
    /// List installed skills.
    List,
    /// Uninstall a skill and delete its files.
    Remove {
        /// The skill's id.
        id: String,
    },
    /// Let a skill's instructions guide agents again.
    Enable {
        /// The skill's id.
        id: String,
    },
    /// Stop a skill's instructions reaching agents, without uninstalling it.
    Disable {
        /// The skill's id.
        id: String,
    },
    /// Re-fetch installed skills and install what changed, verified again.
    Update {
        /// Only this skill.
        id: Option<String>,
    },
    /// Generate a skill for a capability (needs `skills.allow_creation` and a
    /// provider). It is installed disabled and untrusted.
    Create {
        /// The capability it should provide.
        capability: String,
    },
    /// Compute a skill directory's digest and check its signature.
    Verify {
        /// The skill package directory.
        dir: PathBuf,
    },
    /// Sign a skill directory (writes `skill.sig`) — for publishers.
    Sign {
        /// The skill package directory.
        dir: PathBuf,
        /// The key id to put in the signature.
        #[arg(long)]
        key_id: String,
        /// File holding the base64 secret key, as `keygen` prints it.
        #[arg(long)]
        key_file: PathBuf,
    },
    /// Generate a signing key pair — for publishers.
    Keygen,
}

/// MCP subcommands.
#[derive(Subcommand)]
pub enum McpAction {
    /// List configured MCP servers.
    List,
    /// List the tools the configured servers expose, allowlists applied.
    Tools {
        /// Only this server.
        #[arg(long)]
        server: Option<String>,
    },
    /// Call a tool.
    Call {
        /// The tool's name.
        tool: String,
        /// Arguments as a JSON object.
        #[arg(long, default_value = "{}")]
        args: String,
    },
    /// Serve one configured server on stdio with its allowlist enforced.
    ///
    /// This is what ACP agents launch for a server shared with
    /// `share_with_agents = true`.
    Proxy {
        /// The server's name under `[mcp.*]`.
        name: String,
    },
}

/// Memory subcommands.
#[derive(Subcommand)]
pub enum MemoryAction {
    /// Report whether the memory backend is reachable.
    Status,
    /// Search long-term memory.
    Search {
        /// What to search for.
        query: Vec<String>,
    },
}

/// Run a goal, or explain how it would run.
pub async fn run_goal(
    config: Config,
    workspace: PathBuf,
    goal: &str,
    dry_run: bool,
    json: bool,
) -> Result<()> {
    if goal.trim().is_empty() {
        return Err(MetaAgentError::Configuration(
            "no goal was given; try: cuma run \"add a health endpoint\"".to_owned(),
        ));
    }

    let (orchestrator, warnings) =
        harness::build_orchestrator(config.clone(), workspace.clone()).await?;

    for warning in &warnings {
        eprintln!("warning: {warning}");
    }

    if orchestrator.agents().is_empty().await {
        return Err(MetaAgentError::Configuration(
            "no agents are available. Configure one under [agents.*] in .cuma/config.toml, \
             then run `cuma doctor` to check it."
                .to_owned(),
        ));
    }

    if dry_run {
        return explain_plan(&orchestrator, goal, json).await;
    }

    // Stream events to the terminal while the session runs, so a long task
    // shows progress rather than a frozen prompt.
    let mut events = orchestrator.events().subscribe();
    let printer = tokio::spawn(async move {
        while let Ok(event) = events.recv().await {
            match &event.kind {
                EventKind::TaskPlanned { task_count } => {
                    eprintln!("planned {task_count} tasks");
                }
                EventKind::AgentSelected {
                    agent,
                    model,
                    score,
                    ..
                } => {
                    let model = model.as_ref().map_or(String::new(), |m| format!("/{m}"));
                    eprintln!("  -> {agent}{model} (score {score:.3})");
                }
                EventKind::AgentFailed {
                    agent,
                    class,
                    message,
                } => {
                    eprintln!("  !! {agent} failed ({class:?}): {message}");
                }
                EventKind::RetryScheduled {
                    attempt, delay_ms, ..
                } => {
                    eprintln!("  .. retrying (attempt {attempt}) in {delay_ms}ms");
                }
                EventKind::FallbackSelected { from, reason, .. } => {
                    eprintln!("  ~~ falling back from {from}: {reason}");
                }
                EventKind::TaskFailed { reason } => eprintln!("  xx task failed: {reason}"),
                EventKind::SessionCompleted { .. } => break,
                _ => {}
            }
        }
    });

    let result = orchestrator.run(goal).await?;

    // The printer stops on its own at `SessionCompleted`; aborting it here
    // would drop whatever the bus still had queued. The session was recorded
    // as it ran, so there is nothing left to persist.
    if tokio::time::timeout(std::time::Duration::from_secs(2), printer)
        .await
        .is_err()
    {
        tracing::debug!("progress output did not drain before the deadline");
    }

    if json {
        let payload = serde_json::json!({
            "session_id": result.session_id.as_str(),
            "success": result.success,
            "summary": result.summary,
            "tasks": {
                "total": result.graph.len(),
                "completed": result.completed_tasks(),
                "failed": result.failed_tasks(),
                "skipped": result.skipped_tasks(),
            },
            "usage": {
                "attempts": result.usage.attempts,
                "input_tokens": result.usage.input_tokens,
                "output_tokens": result.usage.output_tokens,
                "estimated_cost_usd": result.usage.estimated_cost_usd,
                "cost_is_complete": result.usage.is_complete(),
            },
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );
    } else {
        println!("\n{}", result.summary);

        for task in result.graph.iter() {
            let marker = match task.status {
                cuma_core::TaskStatus::Completed => "[x]",
                cuma_core::TaskStatus::Failed => "[!]",
                cuma_core::TaskStatus::Skipped => "[-]",
                _ => "[ ]",
            };
            let agent = task
                .assigned_agent
                .as_ref()
                .map_or(String::new(), |a| format!("  ({a})"));
            println!("  {marker} {}{agent}", task.spec.description);
        }
    }

    // A failed session is a failed command: `cuma run ... && deploy` must not
    // deploy when the work did not happen.
    if result.success {
        Ok(())
    } else {
        Err(MetaAgentError::Other(result.summary))
    }
}

/// Plan and route without executing.
async fn explain_plan(
    orchestrator: &cuma_orchestrator::Orchestrator,
    goal: &str,
    json: bool,
) -> Result<()> {
    let graph = orchestrator.plan_only(goal).await?;

    if json {
        let tasks: Vec<serde_json::Value> = graph
            .iter()
            .map(|task| {
                serde_json::json!({
                    "id": task.id.as_str(),
                    "description": task.spec.description,
                    "type": format!("{:?}", task.spec.task_type),
                    "risk": format!("{:?}", task.spec.risk),
                    "depends_on": task.spec.dependencies.iter().map(|d| d.as_str()).collect::<Vec<_>>(),
                })
            })
            .collect();

        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "tasks": tasks }))
                .unwrap_or_default()
        );
        return Ok(());
    }

    println!("Plan for: {goal}\n");

    let mut table = Table::new(&["#", "Type", "Risk", "Depends on", "Task"]);
    let ids: Vec<_> = graph.iter().map(|t| t.id.clone()).collect();

    for (index, task) in graph.iter().enumerate() {
        let depends: Vec<String> = task
            .spec
            .dependencies
            .iter()
            .filter_map(|dep| ids.iter().position(|id| id == dep))
            .map(|position| (position + 1).to_string())
            .collect();

        table.row(vec![
            (index + 1).to_string(),
            format!("{:?}", task.spec.task_type),
            format!("{:?}", task.spec.risk),
            if depends.is_empty() {
                "-".to_owned()
            } else {
                depends.join(",")
            },
            task.spec.description.clone(),
        ]);
    }

    println!("{}", table.render());

    // Show how the first task would route, so the explanation covers routing
    // and not only planning.
    if let Some(first) = graph.iter().next() {
        match orchestrator.explain_routing(first).await {
            Ok(decision) => {
                println!("Routing for task 1:\n");
                println!("{}", decision.explain());
            }
            Err(err) => println!("Task 1 could not be routed: {err}"),
        }
    }

    Ok(())
}

/// Launch the TUI.
pub async fn chat(config: Config, workspace: PathBuf) -> Result<()> {
    let (orchestrator, warnings) =
        harness::build_orchestrator(config.clone(), workspace.clone()).await?;
    for warning in &warnings {
        eprintln!("warning: {warning}");
    }

    let skills = if config.skills.enabled {
        cuma_skills::from_config(&config.skills, &workspace)
            .ok()
            .map(|(manager, _)| Arc::new(TuiSkills(manager)) as Arc<dyn cuma_tui::SkillSource>)
    } else {
        None
    };
    let memory = config
        .memory
        .enabled
        .then(|| harness::memory_store(&config, &workspace));

    cuma_tui::run_with(orchestrator, cuma_tui::Sources { skills, memory }).await
}

/// The skill manager, as the TUI's Skills screen sees it.
struct TuiSkills(cuma_skills::SkillManager);

#[async_trait::async_trait]
impl cuma_tui::SkillSource for TuiSkills {
    async fn skills(&self) -> Result<Vec<cuma_tui::SkillRow>> {
        Ok(self
            .0
            .installed()
            .await
            .into_iter()
            .map(|skill| cuma_tui::SkillRow {
                id: skill.manifest.id.to_string(),
                name: skill.manifest.name,
                trust: format!("{:?}", skill.manifest.trust),
                enabled: skill.enabled,
                capabilities: skill
                    .manifest
                    .capabilities
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
                registry: skill.registry,
            })
            .collect())
    }

    async fn set_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        if self.0.set_enabled(&SkillId::new(id), enabled).await? {
            Ok(())
        } else {
            Err(MetaAgentError::Skill(format!("{id} is not installed")))
        }
    }
}

/// Serve CUMA itself as an agent.
pub async fn serve(
    config: Config,
    workspace: PathBuf,
    protocol: &str,
    bind: &str,
    overrides: &harness::CliOverrides,
) -> Result<()> {
    let database = harness::database_path(&config, &workspace);
    let (orchestrator, warnings) =
        harness::build_orchestrator(config.clone(), workspace.clone()).await?;

    // Warnings go to stderr: stdout carries the protocol.
    for warning in &warnings {
        eprintln!("warning: {warning}");
    }

    if orchestrator.agents().is_empty().await {
        return Err(MetaAgentError::Configuration(
            "refusing to serve with no agents registered; there would be nothing to route to"
                .to_owned(),
        ));
    }

    match protocol.to_ascii_lowercase().as_str() {
        "acp" => {
            eprintln!(
                "serving {} agents over ACP on stdio",
                orchestrator.agents().len().await
            );
            // Sessions live beside the runtime database, so `session/load`
            // can restore a conversation after the editor restarts CUMA.
            let sessions_dir = database
                .parent()
                .map_or_else(|| workspace.join(".cuma"), std::path::Path::to_path_buf)
                .join("acp-sessions");
            // Each session works in the directory its editor names, with an
            // orchestrator of its own, built the first time it is needed.
            let workspaces = cuma_server_acp::Workspaces::per_directory(
                workspace.clone(),
                orchestrator,
                workspace_builder(config, workspace, overrides.clone()),
            );
            cuma_server_acp::serve_stdio_workspaces(
                workspaces,
                cuma_server_acp::SessionRegistry::persistent(sessions_dir),
            )
            .await
        }
        "a2a" => {
            let address: std::net::SocketAddr = bind.parse().map_err(|err| {
                MetaAgentError::Configuration(format!("cannot parse --bind {bind:?}: {err}"))
            })?;

            eprintln!(
                "serving {} agents over A2A on http://{address}",
                orchestrator.agents().len().await
            );
            // Tasks are kept in the runtime database, so a caller can still
            // read a result after CUMA restarts.
            let store = match cuma_persistence::RuntimeStore::open(&database) {
                Ok(store) => {
                    Some(Arc::new(A2aTasks(store)) as Arc<dyn cuma_protocol_a2a::TaskStore>)
                }
                Err(err) => {
                    eprintln!("warning: A2A tasks will not survive a restart: {err}");
                    None
                }
            };
            cuma_protocol_a2a::serve_with(
                orchestrator,
                store,
                address,
                &format!("http://{address}"),
            )
            .await
        }
        "mcp" => {
            eprintln!(
                "serving CUMA's tools over MCP on stdio ({} agents behind them)",
                orchestrator.agents().len().await
            );
            let tools = crate::mcp_tools::OrchestratorTools::new(std::sync::Arc::new(orchestrator));
            cuma_protocol_mcp::ToolServer::new("cuma", std::sync::Arc::new(tools))
                .with_instructions(
                    "CUMA routes software-engineering goals across coding agents. \
                     Use cuma_explain to preview a plan and cuma_run to carry it out.",
                )
                .serve_stdio()
                .await
        }
        other => Err(MetaAgentError::Configuration(format!(
            "cannot serve protocol {other:?}; expected \"acp\", \"a2a\" or \"mcp\""
        ))),
    }
}

/// Builds the orchestrator for a directory an ACP client opens a session in.
fn workspace_builder(
    server: Config,
    started_in: PathBuf,
    overrides: harness::CliOverrides,
) -> cuma_server_acp::Builder {
    Arc::new(move |workspace: PathBuf| {
        let server = server.clone();
        let started_in = started_in.clone();
        let overrides = overrides.clone();
        Box::pin(async move {
            let (config, set_aside) =
                harness::workspace_config(&server, &started_in, &workspace, &overrides)?;
            if let Some(warning) = set_aside {
                eprintln!("warning: {warning}");
            }

            let (orchestrator, warnings) =
                harness::build_orchestrator(config, workspace.clone()).await?;
            for warning in &warnings {
                eprintln!("warning ({}): {warning}", workspace.display());
            }
            if orchestrator.agents().is_empty().await {
                return Err(MetaAgentError::Configuration(format!(
                    "no agents are available for {}; there would be nothing to route to",
                    workspace.display()
                )));
            }
            Ok(orchestrator)
        }) as cuma_server_acp::BuildFuture
    })
}

/// The runtime database, as the A2A server's task store.
struct A2aTasks(cuma_persistence::RuntimeStore);

impl cuma_protocol_a2a::TaskStore for A2aTasks {
    fn save(&self, id: &str, context_id: &str, state: &str, body: &str) -> Result<()> {
        self.0.save_a2a_task(id, context_id, state, body)
    }

    fn load(&self, limit: usize) -> Result<Vec<String>> {
        self.0.load_a2a_tasks(limit)
    }

    fn remove(&self, id: &str) -> Result<()> {
        self.0.delete_a2a_task(id)
    }
}

/// Agent subcommands.
pub async fn agents(
    config: Config,
    workspace: PathBuf,
    action: AgentAction,
    json: bool,
) -> Result<()> {
    // The registry commands read a catalogue; they need no agents running.
    match &action {
        AgentAction::Discover {
            registry: true,
            url,
        } => {
            return registry_discover(&config, &workspace, url.as_deref(), json).await;
        }
        AgentAction::Add { id, preview, url } => {
            return registry_add(&config, &workspace, id, *preview, url.as_deref()).await;
        }
        _ => {}
    }

    let (orchestrator, warnings) = harness::build_orchestrator(config, workspace).await?;

    match action {
        AgentAction::Add { .. } => Ok(()),
        AgentAction::Discover { .. } => {
            for warning in &warnings {
                println!("  ! {warning}");
            }
            println!("discovered {} agents", orchestrator.agents().len().await);
            print_agents(&orchestrator, json).await
        }
        AgentAction::List => print_agents(&orchestrator, json).await,
        AgentAction::Show { id } => {
            let Some(agent) = orchestrator.agents().get(&AgentId::new(id.clone())).await else {
                return Err(MetaAgentError::Configuration(format!(
                    "no agent named {id:?} is registered"
                )));
            };

            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&agent).unwrap_or_default()
                );
                return Ok(());
            }

            println!("{} ({:?})", agent.name, agent.protocol);
            println!("  id:           {}", agent.id);
            println!("  enabled:      {}", agent.enabled);
            println!("  health:       {:?}", agent.health.state);
            println!("  auth:         {:?}", agent.auth);

            let capabilities: Vec<String> =
                agent.capabilities.iter().map(ToString::to_string).collect();
            println!("  capabilities: {}", capabilities.join(", "));

            if agent.models.is_empty() {
                println!("  models:       (the agent does not enumerate them)");
            } else {
                println!("  models:");
                for model in &agent.models {
                    println!("    - {} ({})", model.id, model.name);
                }
            }

            if let Some(error) = &agent.health.last_error {
                println!("  last error:   {error}");
            }

            Ok(())
        }
    }
}

async fn print_agents(orchestrator: &cuma_orchestrator::Orchestrator, json: bool) -> Result<()> {
    let snapshot = orchestrator.agents().snapshot().await;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&snapshot.all()).unwrap_or_default()
        );
        return Ok(());
    }

    if snapshot.is_empty() {
        println!("No agents are registered. Configure one under [agents.*] in .cuma/config.toml.");
        return Ok(());
    }

    let mut table = Table::new(&["Agent", "Protocol", "Health", "Models", "Capabilities"]);

    for agent in snapshot.all() {
        table.row(vec![
            agent.id.to_string(),
            format!("{:?}", agent.protocol),
            if agent.enabled {
                format!("{:?}", agent.health.state)
            } else {
                "Disabled".to_owned()
            },
            agent.models.len().to_string(),
            agent.capabilities.len().to_string(),
        ]);
    }

    println!("{}", table.render());
    Ok(())
}

/// Model subcommands.
pub async fn models(config: Config, action: ModelAction, json: bool) -> Result<()> {
    let ModelAction::List = action;

    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let (orchestrator, _) = harness::build_orchestrator(config, workspace).await?;
    let snapshot = orchestrator.agents().snapshot().await;

    let all: Vec<_> = snapshot
        .all()
        .iter()
        .flat_map(|agent| agent.models.iter().cloned())
        .collect();

    if json {
        println!("{}", serde_json::to_string_pretty(&all).unwrap_or_default());
        return Ok(());
    }

    if all.is_empty() {
        println!("No models are registered. Agents that hide their models report none.");
        return Ok(());
    }

    let mut table = Table::new(&["Model", "Agent", "Context", "Input $/Mtok", "Output $/Mtok"]);

    for model in all {
        let render_price = |price: cuma_core::Known<f64>| {
            price
                .value()
                .map_or_else(|| "-".to_owned(), |p| format!("{p:.2}"))
        };

        table.row(vec![
            model.id.to_string(),
            model.agent_id.to_string(),
            model
                .context_window
                .value()
                .map_or_else(|| "-".to_owned(), render_tokens),
            render_price(model.cost.input_per_mtok),
            render_price(model.cost.output_per_mtok),
        ]);
    }

    println!("{}", table.render());
    Ok(())
}

/// Skill subcommands.
pub async fn skills(
    config: Config,
    workspace: PathBuf,
    action: SkillAction,
    json: bool,
) -> Result<()> {
    use base64::Engine;
    use cuma_skills::integrity;

    // The publisher-side commands need no manager.
    match &action {
        SkillAction::Keygen => {
            let secret: [u8; 32] = rand::random();
            let key = ed25519_dalek::SigningKey::from_bytes(&secret);
            let secret = base64::engine::general_purpose::STANDARD.encode(secret);
            let public = integrity::public_key(&key);
            if json {
                println!(
                    "{}",
                    serde_json::json!({ "secret_key": secret, "public_key": public })
                );
            } else {
                println!("secret key (keep it private): {secret}");
                println!("public key:                   {public}");
                println!(
                    "\nConsumers trust it with:\n  [skills.trusted_keys]\n  <your-key-id> = \"{public}\""
                );
            }
            return Ok(());
        }
        SkillAction::Sign {
            dir,
            key_id,
            key_file,
        } => {
            let encoded = std::fs::read_to_string(key_file).map_err(|err| {
                MetaAgentError::Configuration(format!("cannot read {}: {err}", key_file.display()))
            })?;
            let secret: [u8; 32] = base64::engine::general_purpose::STANDARD
                .decode(encoded.trim())
                .ok()
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or_else(|| {
                    MetaAgentError::Configuration(
                        "the key file does not hold a base64 32-byte key".to_owned(),
                    )
                })?;
            let key = ed25519_dalek::SigningKey::from_bytes(&secret);
            let digest = integrity::content_digest(dir)?;
            let line = integrity::sign(&digest, key_id, &key);
            std::fs::write(dir.join(integrity::SIGNATURE_FILE), format!("{line}\n")).map_err(
                |err| MetaAgentError::Skill(format!("cannot write the signature: {err}")),
            )?;
            println!("signed {digest} as {key_id}");
            return Ok(());
        }
        SkillAction::Verify { dir } => {
            let keys = integrity::TrustedKeys::from_config(&config.skills.trusted_keys)?;
            let evidence = integrity::Evidence::gather(dir, None, &keys)?;
            let manifest = cuma_skills::package::read_package(dir)?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "id": manifest.id.as_str(),
                        "digest": evidence.digest,
                        "signature": format!("{:?}", evidence.signature),
                    })
                );
            } else {
                println!("{} ({})", manifest.name, manifest.id);
                println!("  digest:    {}", evidence.digest.as_deref().unwrap_or("-"));
                println!(
                    "  signature: {:?}",
                    evidence
                        .signature
                        .unwrap_or(integrity::SignatureCheck::Absent)
                );
            }
            return Ok(());
        }
        _ => {}
    }

    let (manager, warnings) = cuma_skills::from_config(&config.skills, &workspace)?;
    for warning in &warnings {
        eprintln!("warning: {warning}");
    }

    match action {
        SkillAction::Search { query } => {
            let query = query.join(" ");
            let found = manager.search(&query).await;

            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&found).unwrap_or_default()
                );
                return Ok(());
            }
            if found.is_empty() {
                println!("No skills matched {query:?}.");
                return Ok(());
            }

            // Search results are not verified yet; `inspect` fetches and
            // checks them.
            let mut table = Table::new(&["Skill", "Source", "Capabilities", "Description"]);
            for skill in found {
                let capabilities: Vec<String> =
                    skill.capabilities.iter().map(ToString::to_string).collect();
                table.row(vec![
                    skill.id.to_string(),
                    skill.source,
                    capabilities.join(", "),
                    skill.description,
                ]);
            }
            println!("{}", table.render());
            Ok(())
        }

        SkillAction::Inspect { id } => {
            let (manifest, report) = manager.preview(&SkillId::new(id)).await?;

            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &serde_json::json!({ "manifest": manifest, "validation": report })
                    )
                    .unwrap_or_default()
                );
                return Ok(());
            }

            println!("{} ({})", manifest.name, manifest.id);
            println!("  version:     {}", manifest.version);
            println!("  source:      {}", manifest.source);
            println!("  description: {}", manifest.description);
            println!("  permissions:");
            for permission in &manifest.requested_permissions {
                println!("    - {permission}");
            }
            println!("\n{}", report.render());
            Ok(())
        }

        SkillAction::Install { id } => {
            let installed = manager.install(&SkillId::new(id)).await?;
            println!(
                "installed {} ({:?}{}){}",
                installed.manifest.id,
                installed.manifest.trust,
                installed
                    .signed_by
                    .as_ref()
                    .map_or(String::new(), |k| format!(", signed by {k}")),
                if installed.enabled {
                    ""
                } else {
                    " — disabled"
                }
            );
            Ok(())
        }

        SkillAction::List => {
            let installed = manager.installed().await;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&installed).unwrap_or_default()
                );
                return Ok(());
            }
            if installed.is_empty() {
                println!("No skills are installed.");
                return Ok(());
            }
            let mut table = Table::new(&["Skill", "Trust", "Enabled", "Registry", "Digest"]);
            for skill in installed {
                table.row(vec![
                    skill.manifest.id.to_string(),
                    format!("{:?}", skill.manifest.trust),
                    skill.enabled.to_string(),
                    skill.registry,
                    skill
                        .digest
                        .map_or("-".to_owned(), |d| d.chars().take(19).collect()),
                ]);
            }
            println!("{}", table.render());
            Ok(())
        }

        SkillAction::Remove { id } => {
            if manager.remove(&SkillId::new(id.clone())).await? {
                println!("removed {id}");
                Ok(())
            } else {
                Err(MetaAgentError::Skill(format!("{id} is not installed")))
            }
        }

        SkillAction::Enable { id } => toggle(&manager, &id, true).await,
        SkillAction::Disable { id } => toggle(&manager, &id, false).await,

        SkillAction::Update { id } => {
            let only = id.map(SkillId::new);
            let outcomes = manager.update(only.as_ref()).await;
            if json {
                let rows: Vec<_> = outcomes
                    .iter()
                    .map(|(id, outcome)| serde_json::json!({ "id": id.as_str(), "outcome": outcome }))
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&rows).unwrap_or_default()
                );
                return Ok(());
            }
            if outcomes.is_empty() {
                println!("Nothing to update.");
            }
            for (id, outcome) in outcomes {
                match outcome {
                    cuma_skills::UpdateOutcome::Unchanged => println!("  {id}: up to date"),
                    cuma_skills::UpdateOutcome::Updated { disabled, .. } => println!(
                        "  {id}: updated{}",
                        if disabled {
                            " — disabled, its trust fell"
                        } else {
                            ""
                        }
                    ),
                    cuma_skills::UpdateOutcome::Refused { blockers } => {
                        println!(
                            "  {id}: REFUSED, kept the installed version: {}",
                            blockers.join("; ")
                        );
                    }
                    cuma_skills::UpdateOutcome::Unavailable { reason } => {
                        println!("  {id}: unavailable: {reason}")
                    }
                }
            }
            Ok(())
        }

        SkillAction::Create { capability } => {
            let secrets: Arc<dyn cuma_core::ports::SecretStore> =
                Arc::new(cuma_providers::EnvSecretStore::new());
            let provider = cuma_providers::from_config(&config, secrets)
                .into_iter()
                .next();
            let factory = match provider {
                Some(provider) => {
                    cuma_skills::SkillFactory::new(provider, config.skills.allow_creation)
                }
                None => cuma_skills::SkillFactory::disabled(),
            };
            let capability = cuma_core::Capability::parse(&capability);
            match factory.create(&capability).await? {
                Ok(generated) => {
                    let installed = manager.install_generated(&generated).await?;
                    println!(
                        "generated {} for {capability}; installed DISABLED and Untrusted — review {} before relying on it",
                        installed.manifest.id,
                        manager.install_dir().map_or_else(String::new, |d| d
                            .join(installed.manifest.id.as_str())
                            .display()
                            .to_string())
                    );
                    Ok(())
                }
                Err(refusal) => Err(MetaAgentError::Skill(refusal.explain())),
            }
        }

        SkillAction::Keygen | SkillAction::Sign { .. } | SkillAction::Verify { .. } => Ok(()),
    }
}

async fn toggle(manager: &cuma_skills::SkillManager, id: &str, enabled: bool) -> Result<()> {
    if manager.set_enabled(&SkillId::new(id), enabled).await? {
        println!("{} {id}", if enabled { "enabled" } else { "disabled" });
        Ok(())
    } else {
        Err(MetaAgentError::Skill(format!("{id} is not installed")))
    }
}

/// Memory subcommands.
pub async fn memory(
    config: Config,
    workspace: PathBuf,
    action: MemoryAction,
    json: bool,
) -> Result<()> {
    let store = harness::memory_store(&config, &workspace);

    match action {
        MemoryAction::Status => {
            let available = store.is_available().await;

            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "enabled": config.memory.enabled,
                        "backend": config.memory.backend,
                        "available": available,
                    })
                );
                return Ok(());
            }

            println!("Long-term memory");
            println!("  enabled:   {}", config.memory.enabled);
            println!("  backend:   {}", config.memory.backend);
            println!(
                "  status:    {}",
                if available {
                    "reachable"
                } else if config.memory.enabled {
                    "NOT reachable (running without recall)"
                } else {
                    "disabled"
                }
            );
            Ok(())
        }

        MemoryAction::Search { query } => {
            let query = query.join(" ");
            let memories = store.recall(&query, config.memory.recall_limit).await?;

            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&memories).unwrap_or_default()
                );
                return Ok(());
            }

            if memories.is_empty() {
                println!("Nothing recalled for {query:?}.");
                return Ok(());
            }

            for entry in memories {
                let relevance = entry
                    .relevance
                    .map_or(String::new(), |r| format!(" ({r:.2})"));
                println!("  [{}]{relevance} {}", entry.kind, entry.content);
            }
            Ok(())
        }
    }
}

/// Usage statistics.
pub async fn usage(config: Config, workspace: PathBuf, by_model: bool, json: bool) -> Result<()> {
    let store = cuma_persistence::RuntimeStore::open(&harness::database_path(&config, &workspace))?;

    let sessions = store.session_count()?;
    let attempts = store.attempt_count()?;
    let spend = store.total_spend_usd()?;

    let grouped = if by_model {
        store.usage_by_model()?
    } else {
        store.usage_by_agent()?
    };

    let history = store.load_routing_history()?;

    // RTK's own measurements, for this project. Not CUMA's estimates: RTK
    // counted what it filtered, including commands agents ran through their
    // own RTK hooks.
    let rtk_gain = cuma_workspace::Rtk::detect(&config.rtk).measured_gain(Some(&workspace));

    if json {
        let groups: Vec<serde_json::Value> = grouped
            .iter()
            .map(|(label, totals)| {
                serde_json::json!({
                    "label": label,
                    "attempts": totals.attempts,
                    "successes": totals.successes,
                    "success_rate": totals.success_rate(),
                    "input_tokens": totals.input_tokens,
                    "output_tokens": totals.output_tokens,
                    "estimated_cost_usd": totals.estimated_cost_usd,
                    // Without this flag a consumer cannot tell a complete
                    // total from one with unpriced attempts in it.
                    "cost_is_complete": totals.is_complete(),
                    "attempts_without_pricing": totals.attempts_without_pricing,
                    "mean_latency_ms": totals.mean_latency_ms(),
                })
            })
            .collect();

        let buckets: Vec<serde_json::Value> = history
            .buckets()
            .map(|(key, stats)| {
                serde_json::json!({
                    "bucket": key,
                    "attempts": stats.attempts,
                    "successes": stats.successes,
                    "success_rate": stats.success_rate(),
                    "mean_latency_ms": stats.mean_latency_ms,
                })
            })
            .collect();

        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "sessions": sessions,
                "attempts": attempts,
                // A lower bound: unpriced attempts contribute nothing.
                "recorded_spend_usd": spend,
                "groups": groups,
                "routing_history": buckets,
                "rtk_measured": rtk_gain,
            }))
            .unwrap_or_default()
        );
        return Ok(());
    }

    println!("Sessions: {sessions}   Attempts: {attempts}   Recorded spend: >=${spend:.4}");
    if let Some(gain) = rtk_gain.filter(|g| g.total_commands > 0) {
        println!(
            "RTK (measured): {} commands filtered, {} tokens saved ({:.0}% on average)",
            gain.total_commands, gain.total_saved, gain.avg_savings_pct
        );
    }
    println!();

    if grouped.is_empty() {
        println!("No usage has been recorded yet. Run `cuma run \"...\"` first.");
        return Ok(());
    }

    println!(
        "{}",
        if by_model {
            "MODEL USAGE"
        } else {
            "AGENT USAGE"
        }
    );

    let mut table = Table::new(USAGE_HEADERS);
    for (label, totals) in &grouped {
        table.row(usage_row(label, totals));
    }
    println!("{}", table.render());

    if !history.is_empty() {
        println!("ROUTING HISTORY  (agent / model / task type)");

        let mut table = Table::new(&["Bucket", "Attempts", "Success", "Mean latency"]);
        for (bucket, stats) in history.buckets() {
            table.row(vec![
                bucket.replace('|', " / "),
                stats.attempts.to_string(),
                crate::output::render_rate(stats.success_rate()),
                crate::output::render_latency(Some(stats.mean_latency_ms)),
            ]);
        }
        println!("{}", table.render());
    }

    Ok(())
}

/// Check the installation.
pub async fn doctor(
    config: Config,
    workspace: PathBuf,
    sources: Vec<cuma_config::ConfigSource>,
    json: bool,
) -> Result<()> {
    let mut problems: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    // --- configuration ----------------------------------------------------
    for source in &sources {
        notes.push(match source {
            cuma_config::ConfigSource::Default => "config: built-in defaults".to_owned(),
            cuma_config::ConfigSource::File(path) => format!("config: {}", path.display()),
            cuma_config::ConfigSource::Environment => "config: CUMA_* environment".to_owned(),
        });
    }

    // --- agents -----------------------------------------------------------
    let (orchestrator, warnings) =
        harness::build_orchestrator(config.clone(), workspace.clone()).await?;
    problems.extend(warnings);

    let snapshot = orchestrator.agents().snapshot().await;
    if snapshot.is_empty() {
        problems.push(
            "no agents are registered; configure one under [agents.*] in .cuma/config.toml"
                .to_owned(),
        );
    } else {
        notes.push(format!(
            "agents: {} registered, {} routable",
            snapshot.len(),
            snapshot.routable().count()
        ));

        for agent in snapshot.all() {
            if !agent.is_routable() {
                problems.push(format!(
                    "agent {} is not routable ({:?}){}",
                    agent.id,
                    agent.health.state,
                    agent
                        .health
                        .last_error
                        .as_ref()
                        .map_or(String::new(), |e| format!(": {e}"))
                ));
            }
        }
    }

    // --- database ---------------------------------------------------------
    let database = harness::database_path(&config, &workspace);
    match cuma_persistence::RuntimeStore::open(&database) {
        Ok(store) => notes.push(format!(
            "database: {} ({} sessions recorded)",
            database.display(),
            store.session_count().unwrap_or(0)
        )),
        Err(err) => problems.push(format!("database at {}: {err}", database.display())),
    }

    // --- memory -----------------------------------------------------------
    let memory = harness::memory_store(&config, &workspace);
    if config.memory.enabled {
        if memory.is_available().await {
            notes.push(format!("memory: {} is reachable", config.memory.backend));
        } else {
            problems.push(format!(
                "memory is enabled but {} is not reachable",
                config.memory.backend
            ));
        }
    } else {
        notes.push("memory: disabled".to_owned());
    }

    // --- workspace safety -------------------------------------------------
    if orchestrator.is_git_repository().await {
        notes.push(format!(
            "workspace: git repository{}",
            if config.security.checkpoint_before_write {
                ", checkpointing before writes"
            } else {
                ", NOT checkpointing (security.checkpoint_before_write is off)"
            }
        ));
    } else {
        problems.push(
            "workspace is not a git repository; agents' changes will not be recoverable".to_owned(),
        );
    }

    // --- sandbox and RTK --------------------------------------------------
    let sandbox = orchestrator.sandbox_status();
    if sandbox.is_active() || matches!(sandbox, cuma_workspace::SandboxStatus::Disabled) {
        notes.push(sandbox.describe());
    } else {
        // Requested but unavailable is exactly the case an operator must not
        // discover by having something escape.
        problems.push(sandbox.describe());
    }

    // A shortfall already reached `problems` through the build warnings.
    let agent_sandbox = cuma_workspace::AgentSandbox::detect(&config.security);
    if !agent_sandbox.level().is_shortfall() {
        notes.push(agent_sandbox.describe());
    }

    let rtk = orchestrator.rtk_status();
    if rtk.is_fatal() || matches!(rtk, cuma_workspace::RtkStatus::Unverified { .. }) {
        problems.push(rtk.describe());
    } else {
        notes.push(rtk.describe());
    }
    if rtk.is_active() {
        let measured = cuma_workspace::Rtk::detect(&config.rtk).measured_gain(Some(&workspace));
        match measured {
            Some(gain) if gain.total_commands > 0 => notes.push(format!(
                "RTK measured in this project: {} commands, {} tokens saved ({:.0}% on average)",
                gain.total_commands, gain.total_saved, gain.avg_savings_pct
            )),
            _ => notes.push("RTK has not filtered anything in this project yet".to_owned()),
        }
        // CUMA wraps the commands it runs itself; agents run their own shell
        // commands, which only RTK's per-agent hooks can reach.
        notes.push(
            "RTK hooks: agents filter their own shell output only once hooked — \
             `rtk init -g` for Claude Code, `rtk init -g --agent <name>` for others"
                .to_owned(),
        );
    }

    // --- security ---------------------------------------------------------
    notes.push(format!(
        "security: destructive operations {}",
        if config.security.allow_destructive_operations {
            "ALLOWED"
        } else {
            "denied"
        }
    ));

    if config.security.allow_destructive_operations {
        problems.push(
            "security.allow_destructive_operations is on; agents may run destructive commands"
                .to_owned(),
        );
    }

    // --- report -----------------------------------------------------------
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "healthy": problems.is_empty(),
                "notes": notes,
                "problems": problems,
            }))
            .unwrap_or_default()
        );
    } else {
        for note in &notes {
            println!("  ok   {note}");
        }
        for problem in &problems {
            println!("  WARN {problem}");
        }
        println!();
        println!(
            "{}",
            if problems.is_empty() {
                "Everything checks out."
            } else {
                "Some things need attention (see WARN above)."
            }
        );
    }

    Ok(())
}

/// MCP subcommands.
pub async fn mcp(config: Config, action: McpAction, json: bool) -> Result<()> {
    use cuma_core::ports::ToolProvider;

    let registry = cuma_protocol_mcp::McpServerRegistry::from_config(&config);

    match action {
        McpAction::List => {
            if json {
                let servers: Vec<serde_json::Value> = config
                    .mcp
                    .iter()
                    .map(|(name, server)| {
                        serde_json::json!({
                            "name": name,
                            "command": server.command,
                            "args": server.args,
                            "enabled": server.enabled,
                            "allowed_tools": server.allowed_tools,
                            "share_with_agents": server.share_with_agents,
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&servers).unwrap_or_default()
                );
                return Ok(());
            }

            if config.mcp.is_empty() {
                println!("No MCP servers are configured. Add one under [mcp.<name>].");
                return Ok(());
            }
            let mut table =
                Table::new(&["Server", "Enabled", "Shared", "Allowed tools", "Command"]);
            for (name, server) in &config.mcp {
                table.row(vec![
                    name.clone(),
                    server.enabled.to_string(),
                    server.share_with_agents.to_string(),
                    if server.allowed_tools.is_empty() {
                        "all".to_owned()
                    } else {
                        server.allowed_tools.join(", ")
                    },
                    std::iter::once(server.command.as_str())
                        .chain(server.args.iter().map(String::as_str))
                        .collect::<Vec<_>>()
                        .join(" "),
                ]);
            }
            println!("{}", table.render());
            Ok(())
        }
        McpAction::Tools { server } => {
            let provider = cuma_protocol_mcp::McpToolProvider::new(registry);
            let tools = match &server {
                Some(name) => provider.list_server_tools(name).await?,
                None => provider.list_tools().await?,
            };

            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&tools).unwrap_or_default()
                );
                return Ok(());
            }
            let mut table = Table::new(&["Server", "Tool", "Description"]);
            for tool in tools {
                let description: String = tool
                    .description
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(80)
                    .collect();
                table.row(vec![tool.server, tool.name, description]);
            }
            println!("{}", table.render());
            Ok(())
        }
        McpAction::Call { tool, args } => {
            let arguments: serde_json::Value = serde_json::from_str(&args).map_err(|err| {
                MetaAgentError::Configuration(format!("--args must be a JSON object: {err}"))
            })?;
            let provider = cuma_protocol_mcp::McpToolProvider::new(registry);
            // Tool output is untrusted data; it is printed, never interpreted.
            let output = provider.call_tool(&tool, arguments).await?;
            if json {
                println!("{}", serde_json::json!({ "tool": tool, "output": output }));
            } else {
                println!("{output}");
            }
            Ok(())
        }
        McpAction::Proxy { name } => {
            let Some(only) = registry.only(&name) else {
                return Err(MetaAgentError::Configuration(format!(
                    "no MCP server named {name:?} is configured"
                )));
            };
            let provider = cuma_protocol_mcp::McpToolProvider::new(only);
            cuma_protocol_mcp::ToolServer::new(
                format!("cuma-proxy-{name}"),
                std::sync::Arc::new(provider),
            )
            .serve_stdio()
            .await
        }
    }
}

/// Where the ACP registry is cached between runs.
fn registry_cache(workspace: &std::path::Path) -> PathBuf {
    workspace
        .join(".cuma")
        .join("cache")
        .join("acp-registry.json")
}

/// `cuma agents discover --registry`.
async fn registry_discover(
    config: &Config,
    workspace: &std::path::Path,
    url: Option<&str>,
    json: bool,
) -> Result<()> {
    use cuma_protocol_acp::registry::{Launch, REGISTRY_URL, fetch_registry};

    let fetched = fetch_registry(url.unwrap_or(REGISTRY_URL), &registry_cache(workspace)).await?;
    if let Some(reason) = &fetched.stale {
        eprintln!("warning: showing the cached registry; the live one is unreachable ({reason})");
    }

    if json {
        let rows: Vec<serde_json::Value> = fetched
            .registry
            .agents
            .iter()
            .map(|agent| {
                serde_json::json!({
                    "id": agent.id,
                    "name": agent.name,
                    "version": agent.version,
                    "description": agent.description,
                    "launch": agent.launch(false),
                    "configured": config.agents.contains_key(&agent.id),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).unwrap_or_default()
        );
        return Ok(());
    }

    let mut table = Table::new(&["Agent", "Version", "Launch", "Status", "Description"]);
    for agent in &fetched.registry.agents {
        let (launch, status) = match agent.launch(false) {
            Launch::Command { launcher, .. } => {
                let ready = which::which(&launcher).is_ok();
                (
                    launcher.clone(),
                    if ready {
                        "ready".to_owned()
                    } else {
                        format!("needs {launcher}")
                    },
                )
            }
            Launch::Binary { .. } => ("binary".to_owned(), "manual install".to_owned()),
            Launch::Unsupported => ("-".to_owned(), "not for this platform".to_owned()),
        };
        let status = if config.agents.contains_key(&agent.id) {
            "configured".to_owned()
        } else {
            status
        };
        let description: String = agent.description.chars().take(60).collect();
        table.row(vec![
            agent.id.clone(),
            agent.version.clone(),
            launch,
            status,
            description,
        ]);
    }
    println!("{}", table.render());
    println!("Add one with: cuma agents add <id>");
    Ok(())
}

/// `cuma agents add <id>`.
async fn registry_add(
    config: &Config,
    workspace: &std::path::Path,
    id: &str,
    preview: bool,
    url: Option<&str>,
) -> Result<()> {
    use cuma_protocol_acp::registry::{Launch, REGISTRY_URL, config_entry, fetch_registry};

    if config.agents.contains_key(id) {
        return Err(MetaAgentError::Configuration(format!(
            "an agent named {id:?} is already configured; edit its [agents.{id}] section instead"
        )));
    }

    let fetched = fetch_registry(url.unwrap_or(REGISTRY_URL), &registry_cache(workspace)).await?;
    let Some(agent) = fetched.registry.agents.iter().find(|a| a.id == id) else {
        return Err(MetaAgentError::Configuration(format!(
            "the ACP registry lists no agent {id:?}; see `cuma agents discover --registry`"
        )));
    };

    let command = match agent.launch(preview) {
        Launch::Command { command, launcher } => {
            if which::which(&launcher).is_err() {
                eprintln!("warning: {launcher} is not on PATH; install it before using {id}");
            }
            command
        }
        Launch::Binary {
            archive,
            sha256,
            command,
        } => {
            // Downloading and unpacking an executable on someone's behalf is
            // a step they should take knowingly, with the checksum in hand.
            return Err(MetaAgentError::Configuration(format!(
                "{id} is distributed as a binary. Download {archive}{}, unpack it, and add:\n\n\
                 [agents.{id}]\nprotocol = \"acp\"\ncommand = \"/path/to/{command}\"",
                sha256.map_or_else(String::new, |s| format!(" (sha256 {s})"))
            )));
        }
        Launch::Unsupported => {
            return Err(MetaAgentError::Configuration(format!(
                "{id} publishes nothing this platform ({}) can run",
                cuma_protocol_acp::registry::current_platform()
            )));
        }
    };

    let entry = config_entry(id, &command)?;
    let path = workspace.join(".cuma").join("config.toml");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| MetaAgentError::Configuration(err.to_string()))?;
    }
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if existing.contains(&format!("[agents.{id}]")) {
        return Err(MetaAgentError::Configuration(format!(
            "{} already has [agents.{id}]",
            path.display()
        )));
    }
    std::fs::write(&path, format!("{existing}{entry}")).map_err(|err| {
        MetaAgentError::Configuration(format!("cannot write {}: {err}", path.display()))
    })?;

    println!(
        "added {id} ({} {}) to {}",
        agent.name,
        agent.version,
        path.display()
    );
    println!("  command = {command}");
    println!("Check it with: cuma agents list");
    Ok(())
}
