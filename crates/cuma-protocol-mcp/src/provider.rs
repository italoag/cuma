//! The [`ToolProvider`] implementation over MCP.

use crate::registry::{McpServerConfig, McpServerRegistry};
use async_trait::async_trait;
use cuma_core::error::{MetaAgentError, Result};
use cuma_core::ports::{ToolDescriptor, ToolProvider};
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::TokioChildProcess;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// The largest tool result accepted, in characters.
///
/// Tool output goes straight into an agent's context. A server that returns a
/// 40MB log would blow the window and cost a fortune doing it, so results are
/// truncated with a visible marker rather than passed through whole. This is
/// the same instinct RTK serves, applied at the harness's own boundary.
const MAX_TOOL_RESULT_CHARS: usize = 32_000;

/// Tools reached over MCP.
#[derive(Clone)]
pub struct McpToolProvider {
    registry: McpServerRegistry,
    /// Tools discovered per server, cached so routing does not re-enumerate.
    cache: Arc<RwLock<BTreeMap<String, Vec<ToolDescriptor>>>>,
}

impl McpToolProvider {
    /// A provider over `registry`.
    pub fn new(registry: McpServerRegistry) -> Self {
        Self {
            registry,
            cache: Arc::new(RwLock::new(BTreeMap::new())),
        }
    }

    /// Build the child-process transport for one server.
    fn transport(config: &McpServerConfig) -> Result<TokioChildProcess> {
        let mut command = tokio::process::Command::new(&config.command);
        command.args(&config.args);

        for (key, value) in config.resolved_env() {
            command.env(key, value);
        }

        TokioChildProcess::new(command).map_err(|err| {
            MetaAgentError::protocol_msg(
                "mcp",
                format!("cannot launch MCP server {:?}: {err}", config.command),
            )
        })
    }

    /// Connect to a server, run `operation`, and shut the connection down.
    ///
    /// Connections are per-operation rather than pooled: an MCP server is a
    /// child process, and a pool of them outliving the tasks that needed them
    /// is a leak waiting to happen. Enumeration is cached instead, which is
    /// where the repeated cost actually was.
    async fn with_server<T, F, Fut>(&self, name: &str, operation: F) -> Result<T>
    where
        F: FnOnce(rmcp::service::RunningService<rmcp::RoleClient, ()>) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let Some(config) = self.registry.get(name) else {
            return Err(MetaAgentError::Configuration(format!(
                "no MCP server named {name:?} is configured"
            )));
        };

        if !config.enabled {
            return Err(MetaAgentError::Configuration(format!(
                "MCP server {name:?} is disabled"
            )));
        }

        let transport = Self::transport(config)?;

        let service = ().serve(transport).await.map_err(|err| {
            MetaAgentError::protocol_msg("mcp", format!("{name}: initialize failed: {err}"))
        })?;

        operation(service).await
    }

    /// Enumerate one server's tools, honouring its allowlist.
    pub async fn list_server_tools(&self, name: &str) -> Result<Vec<ToolDescriptor>> {
        if let Some(cached) = self.cache.read().await.get(name) {
            return Ok(cached.clone());
        }

        let Some(config) = self.registry.get(name).cloned() else {
            return Err(MetaAgentError::Configuration(format!(
                "no MCP server named {name:?} is configured"
            )));
        };

        let server_name = name.to_owned();
        let tools = self
            .with_server(name, |service| async move {
                let listed = service.list_all_tools().await.map_err(|err| {
                    MetaAgentError::protocol_msg(
                        "mcp",
                        format!("{server_name}: tools/list failed: {err}"),
                    )
                })?;

                let descriptors = listed
                    .into_iter()
                    // An allowlist filters at the point of discovery, so a
                    // disallowed tool is never even advertised to an agent.
                    .filter(|tool| config.permits(&tool.name))
                    .map(|tool| ToolDescriptor {
                        name: tool.name.to_string(),
                        description: tool
                            .description
                            .as_ref()
                            .map(ToString::to_string)
                            .unwrap_or_default(),
                        input_schema: serde_json::Value::Object((*tool.input_schema).clone()),
                        server: server_name.clone(),
                    })
                    .collect::<Vec<_>>();

                let _ = service.cancel().await;
                Ok(descriptors)
            })
            .await?;

        self.cache
            .write()
            .await
            .insert(name.to_owned(), tools.clone());

        Ok(tools)
    }

    /// Which server provides `tool`.
    async fn server_for_tool(&self, tool: &str) -> Option<String> {
        for (name, tools) in self.cache.read().await.iter() {
            if tools.iter().any(|t| t.name == tool) {
                return Some(name.clone());
            }
        }
        None
    }

    /// Truncate a tool result to something a context window can hold.
    fn bound_result(text: String) -> String {
        if text.chars().count() <= MAX_TOOL_RESULT_CHARS {
            return text;
        }

        let kept: String = text.chars().take(MAX_TOOL_RESULT_CHARS).collect();
        let omitted = text.chars().count() - MAX_TOOL_RESULT_CHARS;
        format!("{kept}\n[... tool output truncated, {omitted} characters omitted]")
    }
}

#[async_trait]
impl ToolProvider for McpToolProvider {
    async fn list_tools(&self) -> Result<Vec<ToolDescriptor>> {
        let mut all = Vec::new();

        let names: Vec<String> = self
            .registry
            .enabled()
            .map(|(name, _)| name.clone())
            .collect();

        for name in names {
            // A single unreachable server must not hide every other server's
            // tools; report it and carry on.
            match self.list_server_tools(&name).await {
                Ok(tools) => all.extend(tools),
                Err(err) => {
                    tracing::warn!(server = name, error = %err, "cannot enumerate an MCP server");
                }
            }
        }

        Ok(all)
    }

    async fn call_tool(&self, name: &str, arguments: serde_json::Value) -> Result<String> {
        // Populate the cache if this is the first call, so the tool can be
        // located and its allowlist checked.
        if self.server_for_tool(name).await.is_none() {
            let _ = self.list_tools().await;
        }

        let Some(server) = self.server_for_tool(name).await else {
            return Err(MetaAgentError::Tool {
                tool: name.to_owned(),
                message: "no configured MCP server provides this tool".to_owned(),
            });
        };

        // Re-check the allowlist at the call site. Discovery filtered it too,
        // but a cached descriptor is not an authorization decision.
        if let Some(config) = self.registry.get(&server)
            && !config.permits(name)
        {
            return Err(MetaAgentError::Security(format!(
                "tool {name:?} is not on server {server:?}'s allowlist"
            )));
        }

        let arguments = match arguments {
            serde_json::Value::Object(map) => Some(map),
            serde_json::Value::Null => None,
            other => {
                return Err(MetaAgentError::Tool {
                    tool: name.to_owned(),
                    message: format!("arguments must be a JSON object, got {other}"),
                });
            }
        };

        let tool_name = name.to_owned();
        self.with_server(&server, |service| async move {
            let mut params = CallToolRequestParams::new(tool_name.clone());
            if let Some(arguments) = arguments {
                params = params.with_arguments(arguments);
            }

            let result = service
                .call_tool(params)
                .await
                .map_err(|err| MetaAgentError::Tool {
                    tool: tool_name.clone(),
                    message: err.to_string(),
                })?;

            let _ = service.cancel().await;

            // A tool that reports an error is a failed tool call, not a
            // successful call whose text happens to describe a failure.
            if result.is_error.unwrap_or(false) {
                return Err(MetaAgentError::Tool {
                    tool: tool_name,
                    message: render_content(&result),
                });
            }

            Ok(Self::bound_result(render_content(&result)))
        })
        .await
    }
}

/// Flatten an MCP tool result into text.
fn render_content(result: &rmcp::model::CallToolResult) -> String {
    let mut out = String::new();

    for item in &result.content {
        if let Some(text) = item.as_text() {
            out.push_str(&text.text);
            out.push('\n');
        }
    }

    if out.is_empty()
        && let Some(structured) = &result.structured_content
    {
        out.push_str(&structured.to_string());
    }

    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn provider_with(name: &str, config: McpServerConfig) -> McpToolProvider {
        let mut registry = McpServerRegistry::new();
        registry.add(name, config);
        McpToolProvider::new(registry)
    }

    #[tokio::test]
    async fn calling_a_tool_no_server_provides_is_a_tool_error() {
        let provider = McpToolProvider::new(McpServerRegistry::new());
        let err = provider
            .call_tool("git_status", serde_json::json!({}))
            .await
            .unwrap_err();

        assert_eq!(err.class(), cuma_core::ErrorClass::ToolFailure);
    }

    #[tokio::test]
    async fn an_unconfigured_server_is_a_configuration_error() {
        let provider = McpToolProvider::new(McpServerRegistry::new());
        let err = provider.list_server_tools("nope").await.unwrap_err();
        assert_eq!(err.class(), cuma_core::ErrorClass::Configuration);
    }

    #[tokio::test]
    async fn a_disabled_server_contributes_no_tools() {
        let mut config = McpServerConfig::new("echo");
        config.enabled = false;
        let provider = provider_with("off", config);

        assert!(provider.list_tools().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_unlaunchable_server_does_not_fail_the_whole_enumeration() {
        let mut registry = McpServerRegistry::new();
        registry.add(
            "broken",
            McpServerConfig::new("definitely-not-a-binary-a83f"),
        );
        let provider = McpToolProvider::new(registry);

        // One dead server must not make every other server's tools disappear.
        assert!(provider.list_tools().await.is_ok());
    }

    #[tokio::test]
    async fn non_object_arguments_are_rejected_before_any_process_is_spawned() {
        let provider = provider_with("srv", McpServerConfig::new("echo"));
        let err = provider
            .call_tool("anything", serde_json::json!("a bare string"))
            .await
            .unwrap_err();

        assert_eq!(err.class(), cuma_core::ErrorClass::ToolFailure);
    }

    #[test]
    fn oversized_tool_output_is_truncated_with_a_visible_marker() {
        let huge = "x".repeat(MAX_TOOL_RESULT_CHARS * 2);
        let bounded = McpToolProvider::bound_result(huge);

        assert!(bounded.chars().count() < MAX_TOOL_RESULT_CHARS + 200);
        assert!(bounded.contains("truncated"));
    }

    #[test]
    fn output_within_the_cap_is_passed_through_unchanged() {
        let small = "the file has 3 lines".to_owned();
        assert_eq!(McpToolProvider::bound_result(small.clone()), small);
    }
}
