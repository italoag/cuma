//! The CLI subcommands.

use crate::harness;
use crate::output::{Table, USAGE_HEADERS, render_tokens, usage_row};
use clap::Subcommand;
use cuma_config::Config;
use cuma_core::error::{MetaAgentError, Result};
use cuma_core::ports::SkillRegistry;
use cuma_core::{AgentId, EventKind, SkillId};
use cuma_skills::{LocalSkillRegistry, SkillManager};
use std::path::PathBuf;
use std::sync::Arc;

/// Agent subcommands.
#[derive(Subcommand)]
pub enum AgentAction {
    /// List registered agents and their health.
    List,
    /// Re-run discovery and report what was found.
    Discover,
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
    /// Search for skills.
    Search {
        /// What to search for.
        query: Vec<String>,
    },
    /// Show what a skill declares, and what validation makes of it.
    Inspect {
        /// The skill's id.
        id: String,
    },
    /// Install a skill after validating it.
    Install {
        /// The skill's id.
        id: String,
    },
    /// List installed skills.
    List,
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
    printer.abort();

    let store_result = persist_session(&config, &workspace, &orchestrator, &result).await;
    if let Err(err) = store_result {
        eprintln!("warning: could not persist this session: {err}");
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

/// Write a finished session to the runtime database.
async fn persist_session(
    config: &Config,
    workspace: &std::path::Path,
    orchestrator: &cuma_orchestrator::Orchestrator,
    result: &cuma_orchestrator::SessionResult,
) -> Result<()> {
    let store = cuma_persistence::RuntimeStore::open(&harness::database_path(config, workspace))?;

    store.begin_session(&result.session_id, &result.summary)?;

    for task in result.graph.iter() {
        store.save_task(&result.session_id, task)?;
    }

    for record in orchestrator.usage_snapshot().await.records() {
        store.record_attempt(record)?;
    }

    store.finish_session(&result.session_id, result.success, &result.summary)?;
    Ok(())
}

/// Launch the TUI.
pub async fn chat(config: Config, workspace: PathBuf) -> Result<()> {
    let (orchestrator, warnings) = harness::build_orchestrator(config, workspace).await?;
    for warning in &warnings {
        eprintln!("warning: {warning}");
    }

    cuma_tui::run(orchestrator).await
}

/// Serve CUMA itself as an agent.
pub async fn serve(config: Config, workspace: PathBuf, protocol: &str, bind: &str) -> Result<()> {
    let (orchestrator, warnings) = harness::build_orchestrator(config, workspace).await?;

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
            cuma_server_acp::serve_stdio(orchestrator).await
        }
        "a2a" => {
            let address: std::net::SocketAddr = bind.parse().map_err(|err| {
                MetaAgentError::Configuration(format!("cannot parse --bind {bind:?}: {err}"))
            })?;

            eprintln!(
                "serving {} agents over A2A on http://{address}",
                orchestrator.agents().len().await
            );
            cuma_protocol_a2a::serve(orchestrator, address, &format!("http://{address}")).await
        }
        other => Err(MetaAgentError::Configuration(format!(
            "cannot serve protocol {other:?}; expected \"acp\" or \"a2a\""
        ))),
    }
}

/// Agent subcommands.
pub async fn agents(config: Config, action: AgentAction, json: bool) -> Result<()> {
    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let (orchestrator, warnings) = harness::build_orchestrator(config, workspace).await?;

    match action {
        AgentAction::Discover => {
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
pub async fn skills(config: Config, action: SkillAction, json: bool) -> Result<()> {
    let registry = Arc::new(LocalSkillRegistry::new());
    let manager = SkillManager::new(config.skills.clone(), vec![registry.clone()]);

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

            let mut table = Table::new(&["Skill", "Trust", "Capabilities", "Description"]);
            for skill in found {
                let capabilities: Vec<String> =
                    skill.capabilities.iter().map(ToString::to_string).collect();
                table.row(vec![
                    skill.id.to_string(),
                    format!("{:?}", skill.trust),
                    capabilities.join(", "),
                    skill.description,
                ]);
            }

            println!("{}", table.render());
            Ok(())
        }

        SkillAction::Inspect { id } => {
            let manifest = registry.inspect(&SkillId::new(id)).await?;
            let report = cuma_skills::validate(&manifest);

            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "manifest": manifest,
                        "validation": report,
                    }))
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
            println!("installed {} ({:?} trust)", installed.id, installed.trust);
            Ok(())
        }

        SkillAction::List => {
            let installed = manager.installed().await;
            if installed.is_empty() {
                println!("No skills are installed.");
            }
            for skill in installed {
                println!("  {} ({:?})", skill.id, skill.trust);
            }
            Ok(())
        }
    }
}

/// Memory subcommands.
pub async fn memory(config: Config, action: MemoryAction, json: bool) -> Result<()> {
    let store = cuma_memory::from_config(&config.memory);

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
            }))
            .unwrap_or_default()
        );
        return Ok(());
    }

    println!("Sessions: {sessions}   Attempts: {attempts}   Recorded spend: >=${spend:.4}\n");

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
    let memory = cuma_memory::from_config(&config.memory);
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

    let rtk = orchestrator.rtk_status();
    if rtk.is_fatal() {
        problems.push(rtk.describe());
    } else {
        notes.push(rtk.describe());
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
