//! Long-term memory over ai-memory's MCP server.
//!
//! `ai-memory serve --transport stdio` exposes typed tools; this backend
//! calls three of them through the [`ToolProvider`] port, so it depends on no
//! MCP SDK itself:
//!
//! | CUMA | ai-memory tool |
//! |---|---|
//! | [`recall`](MemoryStore::recall) | `memory_query {query, limit}` |
//! | [`remember`](MemoryStore::remember) | `memory_write_page {path, body, tags}` |
//! | [`record_handoff`](MemoryStore::record_handoff) | `memory_handoff_begin {summary, open_questions, next_steps, files_touched, cwd}` |
//!
//! Handoffs are where this backend earns its place over the CLI one:
//! ai-memory's handoffs are typed, owned and claimed exactly once, which is
//! the same contract [`AgentHandoff`] expresses inside CUMA.

use crate::cli::{AiMemoryCli, MemoryPage};
use async_trait::async_trait;
use cuma_core::error::Result;
use cuma_core::handoff::AgentHandoff;
use cuma_core::ports::{MemoryEntry, MemoryStore, ToolProvider};
use serde_json::json;
use std::sync::Arc;

/// Long-term memory reached through ai-memory's MCP tools.
pub struct AiMemoryMcp {
    tools: Arc<dyn ToolProvider>,
    workspace: Option<std::path::PathBuf>,
}

impl AiMemoryMcp {
    /// A backend calling ai-memory's tools through `tools`.
    pub fn new(tools: Arc<dyn ToolProvider>) -> Self {
        Self {
            tools,
            workspace: None,
        }
    }

    /// Record `workspace` as the working directory on handoffs.
    #[must_use]
    pub fn in_workspace(mut self, workspace: impl Into<std::path::PathBuf>) -> Self {
        self.workspace = Some(workspace.into());
        self
    }
}

/// Render a handoff as `memory_handoff_begin` arguments.
pub(crate) fn handoff_arguments(
    handoff: &AgentHandoff,
    workspace: Option<&std::path::Path>,
) -> serde_json::Value {
    let mut summary = format!(
        "{} handed \"{}\" over: {}",
        handoff.from_agent, handoff.task_description, handoff.reason
    );
    if let Some(to) = &handoff.to_agent {
        summary.push_str(&format!(" (to {to})"));
    }
    for done in &handoff.completed_work {
        summary.push_str(&format!("\n- done: {done}"));
    }
    for decision in &handoff.decisions {
        summary.push_str(&format!("\n- decided: {decision}"));
    }

    let mut next_steps = handoff.remaining_work.clone();
    next_steps.extend(
        handoff
            .validation_results
            .iter()
            .map(|v| format!("check: {v}")),
    );

    let mut arguments = json!({
        "summary": summary,
        "open_questions": handoff.warnings,
        "next_steps": next_steps,
        "files_touched": handoff.changed_files,
    });
    if let Some(workspace) = workspace {
        arguments["cwd"] = json!(workspace.display().to_string());
    }
    arguments
}

#[async_trait]
impl MemoryStore for AiMemoryMcp {
    async fn is_available(&self) -> bool {
        match self.tools.list_tools().await {
            Ok(tools) => tools.iter().any(|t| t.name == "memory_query"),
            Err(err) => {
                tracing::info!(error = %err, "the ai-memory MCP server is unreachable");
                false
            }
        }
    }

    async fn recall(&self, query: &str, limit: usize) -> Result<Vec<MemoryEntry>> {
        let output = self
            .tools
            .call_tool(
                "memory_query",
                json!({ "query": query, "limit": limit.clamp(1, 100) }),
            )
            .await?;
        Ok(AiMemoryCli::parse_recall(&output))
    }

    async fn remember(&self, content: &str, kind: &str) -> Result<String> {
        let page = MemoryPage::new(content, kind);
        self.tools
            .call_tool(
                "memory_write_page",
                json!({ "path": page.path, "body": page.body, "tags": ["cuma", page.kind] }),
            )
            .await?;
        Ok(page.path)
    }

    async fn record_handoff(&self, handoff: &AgentHandoff) -> Result<Option<String>> {
        let output = self
            .tools
            .call_tool(
                "memory_handoff_begin",
                handoff_arguments(handoff, self.workspace.as_deref()),
            )
            .await?;

        let id = serde_json::from_str::<serde_json::Value>(&output)
            .ok()
            .and_then(|v| {
                ["id", "handoff_id"].iter().find_map(|key| {
                    v.get(key)
                        .map(|id| id.to_string().trim_matches('"').to_owned())
                })
            });
        Ok(id.or(Some("recorded".to_owned())))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use cuma_core::ports::ToolDescriptor;
    use cuma_core::{AgentId, TaskId};
    use std::sync::Mutex;

    /// Records calls and answers from a script.
    #[derive(Default)]
    struct FakeTools {
        calls: Mutex<Vec<(String, serde_json::Value)>>,
    }

    #[async_trait]
    impl ToolProvider for FakeTools {
        async fn list_tools(&self) -> Result<Vec<ToolDescriptor>> {
            Ok(vec![ToolDescriptor {
                name: "memory_query".into(),
                description: String::new(),
                input_schema: json!({}),
                server: "ai-memory".into(),
            }])
        }

        async fn call_tool(&self, name: &str, arguments: serde_json::Value) -> Result<String> {
            self.calls
                .lock()
                .unwrap()
                .push((name.to_owned(), arguments));
            Ok(match name {
                "memory_query" => {
                    r#"{"hits":[{"id":"p1","path":"notes/a.md","title":"A","snippet":"alpha","rank":1.0}]}"#.to_owned()
                }
                "memory_handoff_begin" => r#"{"id":"h-42"}"#.to_owned(),
                _ => "{}".to_owned(),
            })
        }
    }

    #[tokio::test]
    async fn recall_uses_memory_query_and_parses_its_hits() {
        let tools = Arc::new(FakeTools::default());
        let memory = AiMemoryMcp::new(tools.clone());

        let entries = memory.recall("auth", 500).await.unwrap();
        assert_eq!(entries[0].content, "A: alpha");

        let calls = tools.calls.lock().unwrap();
        assert_eq!(calls[0].0, "memory_query");
        assert_eq!(
            calls[0].1["limit"], 100,
            "the tool's own bound is respected"
        );
    }

    #[tokio::test]
    async fn remember_writes_a_page() {
        let tools = Arc::new(FakeTools::default());
        let path = AiMemoryMcp::new(tools.clone())
            .remember("Use tabs", "convention")
            .await
            .unwrap();

        let calls = tools.calls.lock().unwrap();
        assert_eq!(calls[0].0, "memory_write_page");
        assert_eq!(calls[0].1["path"], path);
        assert!(
            calls[0].1["body"]
                .as_str()
                .unwrap()
                .starts_with("# Use tabs")
        );
    }

    #[tokio::test]
    async fn a_handoff_becomes_an_ai_memory_handoff() {
        let tools = Arc::new(FakeTools::default());
        let handoff = AgentHandoff::new(
            TaskId::new("t1"),
            "add OAuth",
            AgentId::new("codex"),
            "rate limited",
        )
        .completed("wrote the callback handler")
        .remaining("wire the token refresh")
        .warning("the staging IdP rejects PKCE")
        .changed_file("src/auth.rs");

        let id = AiMemoryMcp::new(tools.clone())
            .in_workspace("/projects/app")
            .record_handoff(&handoff)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("h-42"));

        let calls = tools.calls.lock().unwrap();
        let (name, arguments) = &calls[0];
        assert_eq!(name, "memory_handoff_begin");
        assert!(
            arguments["summary"]
                .as_str()
                .unwrap()
                .contains("rate limited")
        );
        assert!(
            arguments["summary"]
                .as_str()
                .unwrap()
                .contains("wrote the callback handler")
        );
        assert_eq!(arguments["next_steps"][0], "wire the token refresh");
        assert_eq!(
            arguments["open_questions"][0],
            "the staging IdP rejects PKCE"
        );
        assert_eq!(arguments["files_touched"][0], "src/auth.rs");
        assert_eq!(arguments["cwd"], "/projects/app");
    }
}
