//! The `cuma` command line interface.
//!
//! Everything the TUI can do is reachable here, because automation, CI and
//! other agents need the harness without a terminal UI. The TUI is one front
//! end over the same core, not the product.

mod commands;
mod harness;
mod output;

use clap::{Parser, Subcommand};
use cuma_core::error::Result;
use std::path::PathBuf;

/// A universal control plane for coding agents.
#[derive(Parser)]
#[command(name = "cuma", version, about, long_about = None)]
struct Cli {
    /// Project root. Defaults to the current directory.
    #[arg(long, global = true, value_name = "DIR")]
    workspace: Option<PathBuf>,

    /// Override the routing strategy for this invocation.
    #[arg(long, global = true, value_name = "STRATEGY")]
    strategy: Option<String>,

    /// Force a specific agent.
    #[arg(long, global = true, value_name = "AGENT")]
    agent: Option<String>,

    /// Force a specific model.
    #[arg(long, global = true, value_name = "MODEL")]
    model: Option<String>,

    /// Cap what this invocation may spend, in USD.
    #[arg(long, global = true, value_name = "USD")]
    max_cost: Option<f64>,

    /// Emit JSON instead of human-readable output.
    #[arg(long, global = true)]
    json: bool,

    /// Increase log verbosity. Repeat for more.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run a goal to completion and print the result.
    Run {
        /// What you want done.
        goal: Vec<String>,

        /// Plan and route without executing anything.
        #[arg(long)]
        dry_run: bool,
    },

    /// Start the interactive TUI.
    Chat,

    /// Serve CUMA itself as an agent, so an editor sees one agent.
    ///
    /// Logs go to stderr; stdout is the protocol channel.
    Serve {
        /// Which protocol to serve.
        #[arg(long, default_value = "acp")]
        protocol: String,
    },

    /// Inspect and manage agents.
    Agents {
        #[command(subcommand)]
        action: commands::AgentAction,
    },

    /// Inspect models.
    Models {
        #[command(subcommand)]
        action: commands::ModelAction,
    },

    /// Search, install and manage skills.
    Skills {
        #[command(subcommand)]
        action: commands::SkillAction,
    },

    /// Inspect long-term memory.
    Memory {
        #[command(subcommand)]
        action: commands::MemoryAction,
    },

    /// Show token, cost and outcome statistics.
    Usage {
        /// Break the report down by model rather than by agent.
        #[arg(long)]
        by_model: bool,
    },

    /// Explain how a goal would be planned and routed, without running it.
    Explain {
        /// The goal to explain.
        goal: Vec<String>,
    },

    /// Check the installation and report anything wrong with it.
    Doctor,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");

            // Print the cause chain: the top-level message names what failed,
            // the chain says why.
            let mut source = std::error::Error::source(&err);
            while let Some(cause) = source {
                eprintln!("  caused by: {cause}");
                source = cause.source();
            }

            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();

    let workspace = match cli.workspace.clone() {
        Some(path) => path,
        None => std::env::current_dir().map_err(|err| {
            cuma_core::MetaAgentError::Configuration(format!(
                "cannot determine the current directory: {err}"
            ))
        })?,
    };

    let loaded = cuma_config::Config::load(&workspace)?;
    let mut config = loaded.config;

    // CLI flags are the highest-precedence layer, applied after every file and
    // environment variable.
    harness::apply_cli_overrides(
        &mut config,
        cli.strategy.as_deref(),
        cli.agent.as_deref(),
        cli.model.as_deref(),
        cli.max_cost,
    )?;

    harness::init_tracing(&config, cli.verbose, cli.json);

    match cli.command {
        Some(Command::Run { goal, dry_run }) => {
            let goal = goal.join(" ");
            commands::run_goal(config, workspace, &goal, dry_run, cli.json).await
        }
        Some(Command::Explain { goal }) => {
            let goal = goal.join(" ");
            commands::run_goal(config, workspace, &goal, true, cli.json).await
        }
        Some(Command::Chat) | None => commands::chat(config, workspace).await,
        Some(Command::Serve { protocol }) => commands::serve(config, workspace, &protocol).await,
        Some(Command::Agents { action }) => commands::agents(config, action, cli.json).await,
        Some(Command::Models { action }) => commands::models(config, action, cli.json).await,
        Some(Command::Skills { action }) => commands::skills(config, action, cli.json).await,
        Some(Command::Memory { action }) => commands::memory(config, action, cli.json).await,
        Some(Command::Usage { by_model }) => {
            commands::usage(config, workspace, by_model, cli.json).await
        }
        Some(Command::Doctor) => {
            commands::doctor(config, workspace, loaded.sources, cli.json).await
        }
    }
}
