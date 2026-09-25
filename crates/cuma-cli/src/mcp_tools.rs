//! CUMA's own capabilities, as MCP tools.
//!
//! Served by `cuma serve --protocol mcp`, so any MCP host — an editor, a
//! chat client, another agent — can hand CUMA a goal the same way it would
//! call any other tool.
//!
//! **Do not share this server with the agents CUMA itself delegates to.** An
//! agent that can call `cuma_run` can ask CUMA to delegate back to it, and
//! the only thing bounding that loop would be the budget.

use async_trait::async_trait;
use cuma_core::error::{MetaAgentError, Result};
use cuma_core::ports::{ToolDescriptor, ToolProvider};
use cuma_orchestrator::Orchestrator;
use serde_json::json;
use std::sync::Arc;

/// The tools CUMA exposes about itself.
pub struct OrchestratorTools {
    orchestrator: Arc<Orchestrator>,
}

impl OrchestratorTools {
    /// Tools over `orchestrator`.
    pub fn new(orchestrator: Arc<Orchestrator>) -> Self {
        Self { orchestrator }
    }
}

fn goal_schema(what: &str) -> serde_json::Value {
    json!({
        "type": "object",
        "properties": { "goal": { "type": "string", "description": what } },
        "required": ["goal"],
    })
}

fn goal_argument(arguments: &serde_json::Value) -> Result<String> {
    arguments
        .get("goal")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|goal| !goal.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| MetaAgentError::Tool {
            tool: "cuma".to_owned(),
            message: "a non-empty \"goal\" string is required".to_owned(),
        })
}

#[async_trait]
impl ToolProvider for OrchestratorTools {
    async fn list_tools(&self) -> Result<Vec<ToolDescriptor>> {
        let tool = |name: &str, description: &str, schema: serde_json::Value| ToolDescriptor {
            name: name.to_owned(),
            description: description.to_owned(),
            input_schema: schema,
            server: "cuma".to_owned(),
        };

        Ok(vec![
            tool(
                "cuma_run",
                "Plan a software-engineering goal, route each task to the best available \
                 coding agent, and run it to completion. Returns a summary and each task's output.",
                goal_schema("What should be done, in plain language"),
            ),
            tool(
                "cuma_explain",
                "Show how a goal would be planned and routed, without running anything.",
                goal_schema("The goal to explain"),
            ),
            tool(
                "cuma_agents",
                "List the coding agents CUMA can route to, with their health and capabilities.",
                json!({ "type": "object", "properties": {} }),
            ),
        ])
    }

    async fn call_tool(&self, name: &str, arguments: serde_json::Value) -> Result<String> {
        match name {
            "cuma_run" => {
                let goal = goal_argument(&arguments)?;
                let result = self.orchestrator.run(&goal).await?;
                let outputs = result
                    .graph
                    .iter()
                    .filter_map(|task| {
                        task.successful_outcome()
                            .map(|o| format!("## {}\n{}", task.spec.description, o.output))
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n");

                let text = format!("{}\n\n{outputs}", result.summary);
                if result.success {
                    Ok(text)
                } else {
                    Err(MetaAgentError::Tool {
                        tool: name.to_owned(),
                        message: text,
                    })
                }
            }
            "cuma_explain" => {
                let goal = goal_argument(&arguments)?;
                let graph = self.orchestrator.plan_only(&goal).await?;
                let mut text = format!("Plan for: {goal}\n");
                for (index, task) in graph.iter().enumerate() {
                    text.push_str(&format!(
                        "\n{}. [{:?}, {:?}] {}",
                        index + 1,
                        task.spec.task_type,
                        task.spec.risk,
                        task.spec.description
                    ));
                }
                if let Some(first) = graph.iter().next() {
                    match self.orchestrator.explain_routing(first).await {
                        Ok(decision) => {
                            text.push_str("\n\nRouting for task 1:\n");
                            text.push_str(&decision.explain());
                        }
                        Err(err) => {
                            text.push_str(&format!("\n\nTask 1 could not be routed: {err}"))
                        }
                    }
                }
                Ok(text)
            }
            "cuma_agents" => {
                let agents: Vec<serde_json::Value> = self
                    .orchestrator
                    .agents()
                    .snapshot()
                    .await
                    .all()
                    .iter()
                    .map(|agent| {
                        json!({
                            "id": agent.id.as_str(),
                            "name": agent.name,
                            "protocol": format!("{:?}", agent.protocol),
                            "health": format!("{:?}", agent.health.state),
                            "capabilities": agent.capabilities.iter().map(ToString::to_string).collect::<Vec<_>>(),
                        })
                    })
                    .collect();
                serde_json::to_string_pretty(&agents).map_err(|err| MetaAgentError::Tool {
                    tool: name.to_owned(),
                    message: err.to_string(),
                })
            }
            other => Err(MetaAgentError::Tool {
                tool: other.to_owned(),
                message: "CUMA exposes no such tool".to_owned(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use cuma_core::{AgentDescriptor, AgentProtocol, Capability, Known, ModelDescriptor};
    use cuma_testkit::{Behaviour, MockAgent};

    async fn tools() -> OrchestratorTools {
        let mut descriptor = AgentDescriptor::new("worker", "worker", AgentProtocol::Native)
            .with_capabilities(
                [
                    Capability::CodeComprehension,
                    Capability::CodeGeneration,
                    Capability::CodeEditing,
                    Capability::Testing,
                    Capability::Research,
                    Capability::Documentation,
                    Capability::Planning,
                    Capability::Debugging,
                    Capability::Refactoring,
                    Capability::ShellExecution,
                    Capability::FileSystem,
                    Capability::VersionControl,
                    Capability::Architecture,
                    Capability::CodeReview,
                    Capability::ToolUse,
                ]
                .into_iter()
                .collect(),
            );
        let mut model = ModelDescriptor::minimal(descriptor.id.clone(), "m", "worker");
        model.context_window = Known::Reported(200_000);
        descriptor.models.push(model);

        let mut orchestrator = Orchestrator::new(
            cuma_config::Config::default(),
            Arc::new(cuma_planner::HeuristicPlanner::new()),
            std::env::temp_dir(),
        );
        orchestrator
            .add_agent(Arc::new(
                MockAgent::always("worker", Behaviour::ok("wrote the docs"))
                    .with_descriptor(descriptor),
            ))
            .await
            .unwrap();
        OrchestratorTools::new(Arc::new(orchestrator))
    }

    #[tokio::test]
    async fn three_tools_are_offered() {
        let names: Vec<String> = tools()
            .await
            .list_tools()
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(names, vec!["cuma_run", "cuma_explain", "cuma_agents"]);
    }

    #[tokio::test]
    async fn running_a_goal_returns_what_the_agents_produced() {
        let output = tools()
            .await
            .call_tool("cuma_run", json!({ "goal": "write docs for the API" }))
            .await
            .unwrap();
        assert!(output.contains("wrote the docs"), "{output}");
    }

    #[tokio::test]
    async fn explaining_a_goal_runs_nothing() {
        let output = tools()
            .await
            .call_tool("cuma_explain", json!({ "goal": "write docs for the API" }))
            .await
            .unwrap();
        assert!(output.starts_with("Plan for:"));
        assert!(output.contains("Routing for task 1"));
    }

    #[tokio::test]
    async fn a_missing_goal_is_a_tool_error() {
        let err = tools()
            .await
            .call_tool("cuma_run", json!({ "goal": "   " }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("goal"));
    }

    #[tokio::test]
    async fn the_agent_list_is_json() {
        let output = tools()
            .await
            .call_tool("cuma_agents", json!({}))
            .await
            .unwrap();
        let agents: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(agents[0]["id"], "worker");
    }
}
