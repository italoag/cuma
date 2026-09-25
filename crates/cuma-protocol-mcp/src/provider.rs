//! The [`ToolProvider`] implementation over MCP.

use crate::registry::{McpServerConfig, McpServerRegistry};
use async_trait::async_trait;
use cuma_core::error::{MetaAgentError, Result};
use cuma_core::ports::{ToolDescriptor, ToolProvider};
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::TokioChildProcess;
use std::collections::BTreeMap;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// The largest tool result accepted, in characters.
///
/// Tool output goes straight into an agent's context. A server that returns a
/// 40MB log would blow the window and cost a fortune doing it, so results are
/// truncated with a visible marker rather than passed through whole. This is
/// the same instinct RTK serves, applied at the harness's own boundary.
const MAX_TOOL_RESULT_CHARS: usize = 32_000;

/// How long a connection may sit unused before its server is shut down.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// How often idle connections are looked for.
const REAP_INTERVAL: Duration = Duration::from_secs(30);

type Client = rmcp::service::RunningService<rmcp::RoleClient, ()>;

/// One live connection to a server: its child process and MCP session.
struct Connection {
    service: Client,
    last_used: std::sync::Mutex<Instant>,
}

impl Connection {
    /// Whether the session can still carry a call. A server that exited on
    /// its own shows as a closed transport, not as a cancelled service.
    fn is_alive(&self) -> bool {
        !self.service.is_closed() && !self.service.is_transport_closed()
    }

    fn touch(&self) {
        *self
            .last_used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Instant::now();
    }

    fn idle_for(&self) -> Duration {
        self.last_used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .elapsed()
    }
}

/// The connection to one server, if there is one. Behind an async lock that
/// is held only while connecting, so two first calls start one process, and
/// calls themselves run concurrently over the shared session.
type Slot = Arc<tokio::sync::Mutex<Option<Arc<Connection>>>>;

/// Connections by server name.
type Pool = std::sync::Mutex<BTreeMap<String, Slot>>;

/// Tools reached over MCP.
///
/// Connections are kept open and shared between calls: launching a server
/// and repeating the MCP handshake per call made chatty tools — and memory
/// recall over MCP — slow. A connection whose server has exited is replaced
/// on next use; one unused for five minutes is shut down; dropping the
/// provider shuts down every server it started.
#[derive(Clone)]
pub struct McpToolProvider {
    registry: McpServerRegistry,
    /// Tools discovered per server, cached so routing does not re-enumerate.
    cache: Arc<RwLock<BTreeMap<String, Vec<ToolDescriptor>>>>,
    connections: Arc<Pool>,
    reaper: Arc<std::sync::Once>,
}

impl McpToolProvider {
    /// A provider over `registry`.
    pub fn new(registry: McpServerRegistry) -> Self {
        Self {
            registry,
            cache: Arc::new(RwLock::new(BTreeMap::new())),
            connections: Arc::default(),
            reaper: Arc::new(std::sync::Once::new()),
        }
    }

    /// How many servers currently have a live connection.
    pub async fn open_connections(&self) -> usize {
        let slots: Vec<Slot> = self.pool().values().cloned().collect();
        let mut open = 0;
        for slot in slots {
            if slot.lock().await.as_ref().is_some_and(|c| c.is_alive()) {
                open += 1;
            }
        }
        open
    }

    fn pool(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Slot>> {
        self.connections
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Shut down connections unused for longer than `idle`, and not in use.
    async fn reap(pool: &Pool, idle: Duration) {
        let slots: Vec<(String, Slot)> = pool
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(name, slot)| (name.clone(), Arc::clone(slot)))
            .collect();
        for (name, slot) in slots {
            // A slot being connected is skipped rather than waited for.
            let Ok(mut guard) = slot.try_lock() else {
                continue;
            };
            let expired = guard.as_ref().is_some_and(|connection| {
                !connection.is_alive()
                    || (connection.idle_for() >= idle && Arc::strong_count(connection) == 1)
            });
            if expired && let Some(connection) = guard.take() {
                tracing::debug!(server = name, "closing an idle MCP connection");
                if let Ok(connection) = Arc::try_unwrap(connection) {
                    let _ = connection.service.cancel().await;
                }
            }
        }
    }

    /// Start the background reaper, once, holding only a weak reference so it
    /// ends with the provider.
    fn start_reaper(&self) {
        let pool: Weak<Pool> = Arc::downgrade(&self.connections);
        self.reaper.call_once(|| {
            let Ok(runtime) = tokio::runtime::Handle::try_current() else {
                return;
            };
            runtime.spawn(async move {
                let mut tick = tokio::time::interval(REAP_INTERVAL);
                tick.tick().await;
                loop {
                    tick.tick().await;
                    let Some(pool) = pool.upgrade() else {
                        return;
                    };
                    Self::reap(&pool, IDLE_TIMEOUT).await;
                }
            });
        });
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

    /// Run `operation` on a live connection to `name`, opening one if needed.
    ///
    /// A failed operation is not repeated: a tool may have acted before its
    /// server died, and running it twice is worse than reporting the error.
    /// A connection the failure left dead is dropped, so the next call starts
    /// a fresh server.
    async fn with_server<T, F, Fut>(&self, name: &str, operation: F) -> Result<T>
    where
        F: FnOnce(Arc<Connection>) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let connection = self.connection(name).await?;
        connection.touch();
        let result = operation(Arc::clone(&connection)).await;
        connection.touch();

        if result.is_err() && !connection.is_alive() {
            let slot = self.pool().get(name).cloned();
            if let Some(slot) = slot {
                let mut guard = slot.lock().await;
                if guard.as_ref().is_some_and(|c| Arc::ptr_eq(c, &connection)) {
                    *guard = None;
                }
            }
        }
        result
    }

    /// A live connection to `name`.
    async fn connection(&self, name: &str) -> Result<Arc<Connection>> {
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

        let slot = Arc::clone(self.pool().entry(name.to_owned()).or_default());
        let mut guard = slot.lock().await;
        if let Some(connection) = guard.as_ref()
            && connection.is_alive()
        {
            return Ok(Arc::clone(connection));
        }

        let transport = Self::transport(config)?;
        let service = ().serve(transport).await.map_err(|err| {
            MetaAgentError::protocol_msg("mcp", format!("{name}: initialize failed: {err}"))
        })?;
        tracing::debug!(server = name, "opened an MCP connection");

        let connection = Arc::new(Connection {
            service,
            last_used: std::sync::Mutex::new(Instant::now()),
        });
        *guard = Some(Arc::clone(&connection));
        drop(guard);
        self.start_reaper();
        Ok(connection)
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
            .with_server(name, |connection| async move {
                let listed = connection.service.list_all_tools().await.map_err(|err| {
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
        self.with_server(&server, |connection| async move {
            let mut params = CallToolRequestParams::new(tool_name.clone());
            if let Some(arguments) = arguments {
                params = params.with_arguments(arguments);
            }

            let result =
                connection
                    .service
                    .call_tool(params)
                    .await
                    .map_err(|err| MetaAgentError::Tool {
                        tool: tool_name.clone(),
                        message: err.to_string(),
                    })?;

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
    use serde_json::Value;

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

    // --- connection reuse, against a real server process ------------------

    /// A provider over the counting fixture, or `None` without python3.
    fn counting_provider() -> Option<McpToolProvider> {
        which::which("python3").ok()?;
        let script = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/counting_server.py"
        );
        Some(provider_with(
            "counting",
            McpServerConfig::new("python3").arg(script),
        ))
    }

    /// `(pid, calls)` from a `whoami` or `slow` answer.
    fn pid_and_calls(answer: &str) -> (u32, u32) {
        let field = |key: &str| {
            answer
                .split_whitespace()
                .find_map(|part| part.strip_prefix(key))
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(|| panic!("no {key} in {answer:?}"))
        };
        (field("pid="), field("calls="))
    }

    #[tokio::test]
    async fn one_server_process_answers_every_call() {
        let Some(provider) = counting_provider() else {
            return;
        };

        let (first_pid, first) =
            pid_and_calls(&provider.call_tool("whoami", Value::Null).await.unwrap());
        let (second_pid, second) =
            pid_and_calls(&provider.call_tool("whoami", Value::Null).await.unwrap());

        assert_eq!(first_pid, second_pid, "the connection was reused");
        assert_eq!((first, second), (1, 2), "one process saw both calls");
        assert_eq!(provider.open_connections().await, 1);
    }

    #[tokio::test]
    async fn concurrent_calls_share_one_connection() {
        let Some(provider) = counting_provider() else {
            return;
        };
        provider.call_tool("whoami", Value::Null).await.unwrap();

        let started = Instant::now();
        let calls = (0..5).map(|_| provider.call_tool("slow", Value::Null));
        let answers = futures::future::join_all(calls).await;
        let elapsed = started.elapsed();

        let pids: std::collections::BTreeSet<u32> = answers
            .iter()
            .map(|a| pid_and_calls(a.as_ref().unwrap()).0)
            .collect();
        assert_eq!(pids.len(), 1, "one server process");
        assert!(
            elapsed < Duration::from_millis(1400),
            "calls were not serialised: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn a_server_that_died_is_replaced_and_the_failed_call_is_not_repeated() {
        let Some(provider) = counting_provider() else {
            return;
        };
        let (before, _) = pid_and_calls(&provider.call_tool("whoami", Value::Null).await.unwrap());

        assert!(
            provider.call_tool("die", Value::Null).await.is_err(),
            "the crash is reported"
        );

        let (after, calls) =
            pid_and_calls(&provider.call_tool("whoami", Value::Null).await.unwrap());
        assert_ne!(before, after, "a fresh server replaced the dead one");
        assert_eq!(
            calls, 1,
            "the new server saw only the new call; `die` was not repeated"
        );
    }

    #[tokio::test]
    async fn idle_connections_are_shut_down() {
        let Some(provider) = counting_provider() else {
            return;
        };
        provider.call_tool("whoami", Value::Null).await.unwrap();
        assert_eq!(provider.open_connections().await, 1);

        McpToolProvider::reap(&provider.connections, Duration::ZERO).await;
        assert_eq!(provider.open_connections().await, 0);

        // And the next call simply opens a new one.
        let (_, calls) = pid_and_calls(&provider.call_tool("whoami", Value::Null).await.unwrap());
        assert_eq!(calls, 1);
    }
}
