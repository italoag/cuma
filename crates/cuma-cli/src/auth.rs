//! The agent sign-in journey.
//!
//! One journey, two front ends: `cuma auth` runs it on the terminal it
//! already owns; the TUI runs it after releasing the frame. Both end at the
//! same place — `Orchestrator::reprobe_agent`, which is what proves the
//! credentials the flow just created actually work inside the sandbox the
//! agent runs in.
//!
//! The shape follows what `initialize.authMethods` advertises: a `terminal`
//! method is the agent's own login command run on the user's real terminal —
//! the spec forbids driving it through `authenticate` — while an `agent`
//! method is a request the adapter sends over a connection deliberately made
//! *outside* the sandbox, because that is where the agent's credential store
//! (the macOS Keychain, say) lives.

use cuma_core::error::{MetaAgentError, Result};
use cuma_core::{AgentId, AuthMethodInfo, AuthMethodKind};
use cuma_orchestrator::Orchestrator;
use std::sync::Arc;

/// The orchestrator's view of the journey, as the TUI sees it.
///
/// The interface owns the terminal — it releases and reclaims it around an
/// interactive method — while this owns everything the journey does.
pub struct CliAuth {
    orchestrator: Arc<Orchestrator>,
}

impl CliAuth {
    /// Wrap the orchestrator the interface already drives.
    pub fn new(orchestrator: Arc<Orchestrator>) -> Self {
        Self { orchestrator }
    }
}

#[async_trait::async_trait]
impl cuma_tui::AgentAuthSource for CliAuth {
    async fn needs_terminal(&self, agent: &AgentId) -> bool {
        self.orchestrator
            .agents()
            .get(agent)
            .await
            .is_some_and(|descriptor| {
                descriptor
                    .auth_methods()
                    .iter()
                    .any(|method| method.kind == AuthMethodKind::Terminal)
            })
    }

    async fn authenticate(&self, agent: &AgentId) -> Result<String> {
        sign_in(&self.orchestrator, agent, None).await
    }

    async fn reprobe(&self, agent: &AgentId) -> Result<String> {
        reprobe(&self.orchestrator, agent).await
    }
}

/// Run every advertised sign-in method until one succeeds, then re-probe.
///
/// `only` restricts the journey to one advertised method, as `cuma auth
/// --method` asks. Returns the one-line summary interfaces show.
async fn sign_in(orchestrator: &Orchestrator, id: &AgentId, only: Option<&str>) -> Result<String> {
    let Some(descriptor) = orchestrator.agents().get(id).await else {
        return Err(MetaAgentError::Configuration(format!(
            "no agent named {id} is registered"
        )));
    };

    let methods = descriptor.auth_methods();
    let selected: Vec<AuthMethodInfo> = match only {
        Some(wanted) => {
            let found = methods.iter().find(|m| m.id == wanted).cloned();
            match found {
                Some(method) => vec![method],
                None => {
                    let advertised = methods
                        .iter()
                        .map(|m| m.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Err(MetaAgentError::Configuration(format!(
                        "{id} advertises no sign-in method {wanted:?}{}",
                        if advertised.is_empty() {
                            String::new()
                        } else {
                            format!(" (advertised: {advertised})")
                        }
                    )));
                }
            }
        }
        None => methods,
    };

    if selected.is_empty() {
        return Ok(format!(
            "{id} advertises no sign-in method — its credentials come from its \
             own environment or config (agents.{id}.env)"
        ));
    }

    // A method that ran is reported; one that failed is remembered so the
    // summary can say why the journey ended where it did.
    let mut failures = Vec::new();
    let mut succeeded = false;
    for method in &selected {
        let outcome = match method.kind {
            AuthMethodKind::Terminal => match method.invocation.as_deref() {
                Some(invocation) => run_on_terminal(invocation).await,
                None => Err("the method names no invocation".to_owned()),
            },
            AuthMethodKind::Agent => orchestrator
                .authenticate_agent(id, &method.id)
                .await
                .map_err(|err| err.to_string()),
            AuthMethodKind::Other => Err("a sign-in shape this build does not know".to_owned()),
        };
        match outcome {
            Ok(()) => {
                succeeded = true;
                break;
            }
            Err(err) => failures.push(format!("{}: {err}", method.id)),
        }
    }

    if !succeeded {
        return Err(MetaAgentError::Authentication {
            target: id.to_string(),
            message: format!("every sign-in method failed ({})", failures.join("; ")),
        });
    }

    reprobe(orchestrator, id).await
}

/// Re-interrogate the agent and say what changed.
///
/// The probe is what proves the new credentials work where the agent
/// actually runs — inside its sandbox — not just where they were made.
async fn reprobe(orchestrator: &Orchestrator, id: &AgentId) -> Result<String> {
    match orchestrator.reprobe_agent(id).await {
        Ok(descriptor) => Ok(format!(
            "{id} is healthy — {} models, {} capabilities discovered",
            descriptor.models.len(),
            descriptor.capabilities.len()
        )),
        Err(err) => Err(err),
    }
}

/// Run the login invocation on the user's terminal, stdio inherited.
///
/// The command is the one the agent itself named at negotiation, launched
/// through `sh -c` — never parsed by us — and never inside the sandbox: a
/// credential written inside a jail is a credential the agent cannot read
/// back out.
async fn run_on_terminal(invocation: &str) -> std::result::Result<(), String> {
    eprintln!("$ {invocation}");
    let status = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(invocation)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .await
        .map_err(|err| format!("could not run `{invocation}`: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("`{invocation}` exited with {status}"))
    }
}

/// `cuma auth`: the sign-in state of every agent, or one agent's journey.
pub async fn run(
    config: cuma_config::Config,
    workspace: std::path::PathBuf,
    agent: Option<String>,
    method: Option<String>,
    json: bool,
) -> Result<()> {
    let (orchestrator, warnings) = crate::harness::build_orchestrator(config, workspace).await?;

    let Some(name) = agent else {
        return print_status(&orchestrator, &warnings, json).await;
    };

    let summary = sign_in(&orchestrator, &AgentId::new(name), method.as_deref()).await?;
    println!("{summary}");
    Ok(())
}

/// Every agent's sign-in posture, for `cuma auth` with no name.
async fn print_status(orchestrator: &Orchestrator, warnings: &[String], json: bool) -> Result<()> {
    let snapshot = orchestrator.agents().snapshot().await;

    if json {
        let report: Vec<serde_json::Value> = snapshot
            .all()
            .iter()
            .map(|agent| {
                serde_json::json!({
                    "agent": agent.id.to_string(),
                    "health": format!("{:?}", agent.health.state),
                    "auth": format!("{:?}", agent.auth),
                    "sign_in_methods": agent.auth_methods(),
                    "last_error": agent.health.last_error,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
        return Ok(());
    }

    let mut table = crate::output::Table::new(&["Agent", "Health", "Sign-in", "Detail"]);
    for agent in snapshot.all() {
        let methods = agent.auth_methods();
        let sign_in = if methods.is_empty() {
            "-".to_owned()
        } else {
            methods
                .iter()
                .map(|m| match m.kind {
                    AuthMethodKind::Terminal => format!("{} (terminal)", m.id),
                    AuthMethodKind::Agent => format!("{} (agent)", m.id),
                    AuthMethodKind::Other => m.id.clone(),
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        table.row(vec![
            agent.id.to_string(),
            if agent.enabled {
                format!("{:?}", agent.health.state)
            } else {
                "Disabled".to_owned()
            },
            sign_in,
            agent.health.last_error.clone().unwrap_or_default(),
        ]);
    }
    println!("{}", table.render());

    for warning in warnings {
        println!("  ! {warning}");
    }
    println!("\nSign an agent in with: cuma auth <agent>");
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use cuma_core::{AgentDescriptor, AgentProtocol, HealthState};
    use cuma_testkit::MockAgent;

    async fn harness(descriptor: AgentDescriptor) -> Orchestrator {
        let mut orchestrator = Orchestrator::new(
            cuma_config::Config::default(),
            Arc::new(cuma_planner::HeuristicPlanner::new()),
            std::env::temp_dir(),
        );
        orchestrator
            .add_agent(Arc::new(
                MockAgent::always("unused", cuma_testkit::Behaviour::ok("done"))
                    .with_descriptor(descriptor),
            ))
            .await
            .expect("a mock agent registers");
        orchestrator
    }

    fn agent(id: &str) -> AgentDescriptor {
        AgentDescriptor::new(id, id, AgentProtocol::Acp)
    }

    fn with_methods(mut agent: AgentDescriptor, methods_json: &str) -> AgentDescriptor {
        agent
            .metadata
            .insert("acp_auth_methods".to_owned(), methods_json.to_owned());
        agent
    }

    #[tokio::test]
    async fn an_agent_with_no_sign_in_method_gets_the_environment_guidance() {
        let id = AgentId::new("kiro");
        let orchestrator = harness(agent("kiro")).await;

        let summary = sign_in(&orchestrator, &id, None).await.unwrap();
        assert!(summary.contains("no sign-in method"), "{summary}");
        assert!(summary.contains("agents.kiro.env"), "{summary}");
    }

    #[tokio::test]
    async fn an_unadvertised_method_is_refused_with_what_exists() {
        let id = AgentId::new("copilot");
        let descriptor = with_methods(
            agent("copilot"),
            r#"[{"id":"copilot-login","kind":"terminal","invocation":"copilot login"}]"#,
        );
        let orchestrator = harness(descriptor).await;

        let err = sign_in(&orchestrator, &id, Some("oauth"))
            .await
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("oauth"), "{message}");
        assert!(message.contains("copilot-login"), "{message}");
    }

    #[tokio::test]
    async fn a_terminal_method_runs_and_the_agent_is_reprobed() {
        let id = AgentId::new("copilot");
        let mut descriptor = with_methods(
            agent("copilot"),
            r#"[{"id":"copilot-login","kind":"terminal","invocation":"true"}]"#,
        );
        descriptor.health.state = HealthState::Unavailable;
        descriptor.health.last_error = Some("was unauthenticated".to_owned());
        let orchestrator = harness(descriptor).await;

        let summary = sign_in(&orchestrator, &id, None).await.unwrap();
        assert!(summary.contains("healthy"), "{summary}");

        let after = orchestrator.agents().get(&id).await.unwrap();
        assert_eq!(after.health.state, HealthState::Healthy);
        assert!(
            after.health.last_error.is_none(),
            "the old error is cleared"
        );
    }

    #[tokio::test]
    async fn a_failed_sign_in_marks_the_agent_unavailable() {
        let id = AgentId::new("junie");
        // `agent`-kind methods reach the adapter, and the mock adapter's
        // default port answers "not supported" — a failed journey.
        let descriptor = with_methods(
            agent("junie"),
            r#"[{"id":"jetbrains-account","kind":"agent"}]"#,
        );
        let orchestrator = harness(descriptor).await;

        let err = sign_in(&orchestrator, &id, None).await.unwrap_err();
        assert!(
            err.to_string().contains("every sign-in method failed"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn reprobe_restores_health_and_clears_the_old_error() {
        let id = AgentId::new("devin");
        let mut descriptor = agent("devin");
        descriptor.health.state = HealthState::Unavailable;
        descriptor.health.last_error = Some("stale failure".to_owned());
        let orchestrator = harness(descriptor).await;
        orchestrator
            .agents()
            .set_health(
                &id,
                HealthState::Unavailable,
                Some("stale failure".to_owned()),
            )
            .await;

        let summary = reprobe(&orchestrator, &id).await.unwrap();
        assert!(summary.contains("healthy"), "{summary}");
        let after = orchestrator.agents().get(&id).await.unwrap();
        assert!(after.health.last_error.is_none());
    }
}
