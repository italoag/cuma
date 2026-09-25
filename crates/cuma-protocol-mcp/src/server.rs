//! Serving tools over MCP.
//!
//! One server, two uses:
//!
//! - **`cuma serve --protocol mcp`** exposes CUMA itself — run a goal,
//!   explain a routing decision, list agents — as tools any MCP host can call.
//! - **`cuma mcp proxy <name>`** re-exposes one configured MCP server with its
//!   allowlist enforced. This is how a server is shared with the ACP agents
//!   CUMA delegates to: the agent launches the proxy, never the server
//!   directly, so a tool the operator did not allow cannot be reached, and a
//!   secret in the server's environment is resolved inside CUMA rather than
//!   written into the agent's session request.
//!
//! Both are the same [`ToolServer`] over a different
//! [`ToolProvider`](cuma_core::ports::ToolProvider).

use cuma_core::error::{MetaAgentError, Result};
use cuma_core::ports::ToolProvider;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};
use std::sync::Arc;

/// An MCP server exposing whatever a [`ToolProvider`] offers.
#[derive(Clone)]
pub struct ToolServer {
    provider: Arc<dyn ToolProvider>,
    name: String,
    instructions: Option<String>,
}

impl ToolServer {
    /// A server named `name` over `provider`.
    pub fn new(name: impl Into<String>, provider: Arc<dyn ToolProvider>) -> Self {
        Self {
            provider,
            name: name.into(),
            instructions: None,
        }
    }

    /// Instructions for the host's model, sent during initialization.
    #[must_use]
    pub fn with_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = Some(instructions.into());
        self
    }

    /// Serve over stdin/stdout until the client disconnects.
    ///
    /// Stdout is the protocol channel: logging must go to stderr.
    pub async fn serve_stdio(self) -> Result<()> {
        self.serve_on(rmcp::transport::stdio()).await
    }

    /// Serve over any transport rmcp accepts — a duplex stream, a
    /// `(reader, writer)` pair — until the client disconnects.
    pub async fn serve_on<T, E, A>(self, transport: T) -> Result<()>
    where
        T: rmcp::transport::IntoTransport<RoleServer, E, A>,
        E: std::error::Error + Send + Sync + 'static,
    {
        let running = self.serve(transport).await.map_err(|err| {
            MetaAgentError::protocol_msg("mcp", format!("MCP initialization failed: {err}"))
        })?;
        running
            .waiting()
            .await
            .map(|_| ())
            .map_err(|err| MetaAgentError::protocol_msg("mcp", format!("MCP server failed: {err}")))
    }
}

impl ServerHandler for ToolServer {
    fn get_info(&self) -> ServerInfo {
        let mut info =
            ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
                Implementation::new(self.name.clone(), env!("CARGO_PKG_VERSION")),
            );
        if let Some(instructions) = &self.instructions {
            info = info.with_instructions(instructions.clone());
        }
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, ErrorData> {
        let tools = self
            .provider
            .list_tools()
            .await
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;

        Ok(ListToolsResult::with_all_items(
            tools
                .into_iter()
                .map(|tool| {
                    let schema = match tool.input_schema {
                        serde_json::Value::Object(map) => map,
                        // MCP requires an object schema; a tool that gave
                        // none takes any object.
                        _ => serde_json::Map::from_iter([(
                            "type".to_owned(),
                            serde_json::Value::from("object"),
                        )]),
                    };
                    Tool::new(tool.name, tool.description, Arc::new(schema))
                })
                .collect(),
        ))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResponse, ErrorData> {
        let arguments = request
            .arguments
            .map_or(serde_json::Value::Null, serde_json::Value::Object);

        // A tool that fails is reported as a failed *tool result*, which the
        // host's model can read and react to, not as a protocol error that
        // looks like the server itself broke. A refusal on security grounds
        // is the exception: it is the server declining the call.
        match self.provider.call_tool(&request.name, arguments).await {
            Ok(text) => Ok(CallToolResult::success(vec![ContentBlock::text(text)]).into()),
            Err(MetaAgentError::Security(message)) => Err(ErrorData::invalid_params(message, None)),
            Err(err) => Ok(CallToolResult::error(vec![ContentBlock::text(err.to_string())]).into()),
        }
    }
}
