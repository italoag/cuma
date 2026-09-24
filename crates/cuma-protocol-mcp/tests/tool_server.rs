//! A real rmcp client against [`ToolServer`], in process.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use async_trait::async_trait;
use cuma_core::error::{MetaAgentError, Result};
use cuma_core::ports::{ToolDescriptor, ToolProvider};
use cuma_protocol_mcp::ToolServer;
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use serde_json::json;
use std::sync::Arc;

struct Tools;

#[async_trait]
impl ToolProvider for Tools {
    async fn list_tools(&self) -> Result<Vec<ToolDescriptor>> {
        Ok(vec![
            ToolDescriptor {
                name: "echo".into(),
                description: "Echo the input".into(),
                input_schema: json!({ "type": "object", "properties": { "text": { "type": "string" } } }),
                server: "test".into(),
            },
            ToolDescriptor {
                name: "schemaless".into(),
                description: String::new(),
                input_schema: serde_json::Value::Null,
                server: "test".into(),
            },
        ])
    }

    async fn call_tool(&self, name: &str, arguments: serde_json::Value) -> Result<String> {
        match name {
            "echo" => Ok(arguments["text"].as_str().unwrap_or_default().to_owned()),
            "forbidden" => Err(MetaAgentError::Security("not on the allowlist".into())),
            _ => Err(MetaAgentError::Tool {
                tool: name.into(),
                message: "no such tool".into(),
            }),
        }
    }
}

async fn client() -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
    let (client_side, server_side) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let _ = ToolServer::new("test", Arc::new(Tools))
            .with_instructions("tools for tests")
            .serve_on(server_side)
            .await;
    });
    ().serve(client_side).await.unwrap()
}

#[tokio::test]
async fn a_client_lists_the_providers_tools() {
    let client = client().await;
    let tools = client.list_all_tools().await.unwrap();

    let names: Vec<_> = tools.iter().map(|t| t.name.to_string()).collect();
    assert_eq!(names, vec!["echo", "schemaless"]);
    assert_eq!(
        tools[1].input_schema.get("type").and_then(|t| t.as_str()),
        Some("object"),
        "a tool with no schema still gets a valid object schema"
    );

    let info = client.peer_info().unwrap();
    assert_eq!(info.server_info.as_ref().unwrap().name, "test");
    assert_eq!(info.instructions.as_deref(), Some("tools for tests"));
}

#[tokio::test]
async fn a_tool_call_returns_the_providers_text() {
    let client = client().await;
    let mut arguments = serde_json::Map::new();
    arguments.insert("text".into(), json!("hello"));

    let result = client
        .call_tool(CallToolRequestParams::new("echo").with_arguments(arguments))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(false));
    assert_eq!(result.content[0].as_text().unwrap().text, "hello");
}

#[tokio::test]
async fn a_failing_tool_is_a_failed_result_not_a_broken_server() {
    let client = client().await;
    let result = client
        .call_tool(CallToolRequestParams::new("missing"))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(
        result.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("no such tool")
    );
}

#[tokio::test]
async fn a_refused_tool_is_a_protocol_error() {
    let client = client().await;
    let err = client
        .call_tool(CallToolRequestParams::new("forbidden"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("allowlist"), "{err}");
}
