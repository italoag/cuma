//! Hexagonal ports.
//!
//! These traits are the *only* way the core reaches the outside world. ACP,
//! A2A, MCP, SQLite, `ai-memory` and every provider SDK sit behind one of them.
//! A new protocol is a new adapter, not a change to the orchestrator.
//!
//! All ports are `Send + Sync` and object safe, so the orchestrator can hold
//! `Arc<dyn Port>` and swap implementations (including test doubles) freely.

use crate::agent::{AgentDescriptor, ModelDescriptor};
use crate::error::Result;
use crate::handoff::AgentHandoff;
use crate::ids::{AgentId, ModelId, SkillId};
use crate::task::{ExecutionOutcome, Task, TaskGraph};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Everything an adapter needs to execute one task.
///
/// Note what is *not* here: the conversation history. Context assembly is the
/// context manager's job, and it hands over only what this task needs.
#[derive(Debug, Clone)]
pub struct ExecutionRequest {
    /// The task to run.
    pub task: Task,
    /// The model to use, when the agent exposes a choice.
    pub model: Option<ModelId>,
    /// The assembled prompt, including any handoff preamble.
    pub prompt: String,
    /// Working directory the agent should operate in.
    pub workspace: std::path::PathBuf,
    /// Handoff from a previous agent, when this is a fallback.
    pub handoff: Option<AgentHandoff>,
    /// Hard deadline in milliseconds.
    pub timeout_ms: u64,
}

/// An incremental update from a running agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ExecutionUpdate {
    /// A chunk of assistant text.
    Text {
        /// The chunk.
        content: String,
    },
    /// The agent started a tool call.
    ToolCall {
        /// Tool name.
        name: String,
        /// Human-readable status.
        status: String,
    },
    /// The agent revised its own plan.
    Plan {
        /// Rendered plan entries.
        entries: Vec<String>,
    },
    /// The agent asked permission to do something.
    ///
    /// The orchestrator answers from policy; it never forwards this to a human
    /// unless policy says to.
    PermissionRequest {
        /// What is being requested.
        description: String,
        /// The choices the agent offered.
        options: Vec<String>,
    },
}

/// Executes tasks on one agent, whatever protocol it speaks.
///
/// This is the single most important port in the system: it is what makes ACP
/// agents, A2A agents and mocks interchangeable to the orchestrator.
#[async_trait]
pub trait AgentAdapter: Send + Sync {
    /// Which agent this adapter drives.
    fn agent_id(&self) -> &AgentId;

    /// Re-read capabilities, models and health from the live agent.
    ///
    /// Called at discovery time and on health checks. Adapters that cannot
    /// interrogate their agent return the statically configured descriptor.
    async fn describe(&self) -> Result<AgentDescriptor>;

    /// Run one task to completion.
    ///
    /// `updates` receives streaming progress. Implementations must respect
    /// `request.timeout_ms` and must return promptly when the task is
    /// cancelled — a fallback that waits for a hung agent is not a fallback.
    async fn execute(
        &self,
        request: ExecutionRequest,
        updates: tokio::sync::mpsc::Sender<ExecutionUpdate>,
    ) -> Result<ExecutionOutcome>;

    /// Cheap liveness probe. Used by the circuit breaker to half-open.
    async fn health_check(&self) -> Result<()> {
        Ok(())
    }

    /// Release resources (terminate a child process, close a session).
    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

/// Discovers agents from some source: config, an ACP registry, A2A Agent Cards.
#[async_trait]
pub trait AgentDiscovery: Send + Sync {
    /// A label for logs and for explaining where an agent came from.
    fn source_name(&self) -> &str;

    /// Find agents. Implementations should be quick and must not spawn work
    /// that outlives the call.
    async fn discover(&self) -> Result<Vec<AgentDescriptor>>;
}

/// Decomposes a user goal into a task graph.
#[async_trait]
pub trait Planner: Send + Sync {
    /// Produce an executable plan for `goal`.
    async fn plan(&self, goal: &str, context: &PlanningContext) -> Result<TaskGraph>;

    /// Revise a plan after a task failed in a way retrying will not fix.
    ///
    /// The default refuses rather than silently returning the same plan, which
    /// would spin the orchestrator.
    async fn replan(
        &self,
        _graph: &TaskGraph,
        _failed: &Task,
        _reason: &str,
    ) -> Result<Option<TaskGraph>> {
        Ok(None)
    }
}

/// What the planner is allowed to know about the world.
#[derive(Debug, Clone, Default)]
pub struct PlanningContext {
    /// The project root.
    pub workspace: std::path::PathBuf,
    /// Capabilities available across all registered agents, so the planner
    /// does not produce tasks nothing can execute.
    pub available_capabilities: crate::capability::CapabilitySet,
    /// Relevant long-term memories.
    pub memories: Vec<MemoryEntry>,
    /// Free-form hints from the user or from project config.
    pub hints: BTreeMap<String, String>,
}

/// Assembles the minimum context a task needs.
#[async_trait]
pub trait ContextManager: Send + Sync {
    /// Build the prompt for `task`.
    ///
    /// Implementations must respect `token_budget`: exceeding a model's
    /// context window is a failure the router cannot recover from cheaply.
    async fn assemble(
        &self,
        task: &Task,
        graph: &TaskGraph,
        handoff: Option<&AgentHandoff>,
        token_budget: u64,
    ) -> Result<String>;
}

/// One remembered fact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    /// Backend-assigned identifier.
    pub id: String,
    /// The content.
    pub content: String,
    /// What kind of memory this is (decision, convention, finding).
    pub kind: String,
    /// Relevance to the query that retrieved it, when the backend scores.
    pub relevance: Option<f64>,
    /// When it was recorded.
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Long-term memory shared across agents and sessions.
///
/// The harness does not implement semantic search itself — see ADR-005. This
/// port exists so a dedicated memory system can be plugged in behind it.
#[async_trait]
pub trait MemoryStore: Send + Sync {
    /// Whether the backend is actually reachable.
    ///
    /// Memory is optional; a harness with no memory backend must still run.
    async fn is_available(&self) -> bool;

    /// Retrieve memories relevant to `query`.
    async fn recall(&self, query: &str, limit: usize) -> Result<Vec<MemoryEntry>>;

    /// Record a memory.
    async fn remember(&self, content: &str, kind: &str) -> Result<String>;
}

/// A tool the harness or an agent can call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDescriptor {
    /// Tool name as exposed to agents.
    pub name: String,
    /// What it does.
    pub description: String,
    /// JSON Schema for its arguments.
    pub input_schema: serde_json::Value,
    /// Which MCP server provides it.
    pub server: String,
}

/// Access to tools, typically over MCP.
#[async_trait]
pub trait ToolProvider: Send + Sync {
    /// Tools currently available.
    async fn list_tools(&self) -> Result<Vec<ToolDescriptor>>;

    /// Invoke a tool.
    ///
    /// The result is *untrusted input* — it may be attacker-controlled. Callers
    /// must not treat it as instructions. See `docs/SECURITY.md`.
    async fn call_tool(&self, name: &str, arguments: serde_json::Value) -> Result<String>;
}

/// How much a skill is trusted, and therefore what it is allowed to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustLevel {
    /// Ships with the harness, or signed by a key the operator configured.
    Trusted,
    /// From a known registry with a verified signature or hash.
    Verified,
    /// From a public registry, unsigned.
    Community,
    /// Unknown origin. Never auto-installed, never run unsandboxed.
    Untrusted,
}

/// A skill's declared metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillManifest {
    /// Identifier.
    pub id: SkillId,
    /// Display name.
    pub name: String,
    /// What it does.
    pub description: String,
    /// Semantic version.
    pub version: String,
    /// Where it came from.
    pub source: String,
    /// Capabilities it grants.
    pub capabilities: crate::capability::CapabilitySet,
    /// Permissions it requests (filesystem paths, network hosts, commands).
    pub requested_permissions: Vec<String>,
    /// Content hash, when the registry publishes one.
    pub checksum: Option<String>,
    /// Signature, when the registry publishes one.
    pub signature: Option<String>,
    /// Assessed trust level.
    pub trust: TrustLevel,
}

/// Where skills are found and installed from.
#[async_trait]
pub trait SkillRegistry: Send + Sync {
    /// A label for logs.
    fn name(&self) -> &str;

    /// Search for skills matching `query`.
    async fn search(&self, query: &str) -> Result<Vec<SkillManifest>>;

    /// Fetch the full manifest for a skill.
    async fn inspect(&self, id: &SkillId) -> Result<SkillManifest>;

    /// Install a skill.
    ///
    /// Implementations must not execute skill code during installation, and
    /// must not install a manifest that has not passed validation.
    async fn install(&self, id: &SkillId) -> Result<SkillManifest>;
}

/// Direct model access, for the harness's own reasoning.
///
/// Used by the planner, the classifier and summarization — never as a
/// substitute for ACP/A2A when talking to a coding agent (see ADR-002).
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Provider name for logs and usage attribution.
    fn name(&self) -> &str;

    /// Models this provider exposes.
    async fn models(&self) -> Result<Vec<ModelDescriptor>>;

    /// One completion.
    async fn complete(&self, system: &str, user: &str, model: Option<&ModelId>) -> Result<String>;
}

/// Resolves secrets without ever holding them in the domain.
///
/// Descriptors carry a *handle*; this port turns a handle into a secret at the
/// moment of use. Nothing else in the system may store the value.
#[async_trait]
pub trait SecretStore: Send + Sync {
    /// Resolve a handle. Returns `None` when the secret is not set.
    async fn get(&self, handle: &str) -> Result<Option<String>>;

    /// Store a secret under a handle.
    async fn set(&self, handle: &str, value: &str) -> Result<()>;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    /// The orchestrator holds `Arc<dyn AgentAdapter>`; if these traits stop
    /// being object safe the whole design collapses, so pin it with a test.
    #[test]
    fn ports_are_object_safe() {
        fn assert_object_safe<T: ?Sized>() {}
        assert_object_safe::<dyn AgentAdapter>();
        assert_object_safe::<dyn AgentDiscovery>();
        assert_object_safe::<dyn Planner>();
        assert_object_safe::<dyn ContextManager>();
        assert_object_safe::<dyn MemoryStore>();
        assert_object_safe::<dyn ToolProvider>();
        assert_object_safe::<dyn SkillRegistry>();
        assert_object_safe::<dyn LlmProvider>();
        assert_object_safe::<dyn SecretStore>();
    }

    #[test]
    fn trust_levels_order_from_most_to_least_trusted() {
        assert!(TrustLevel::Trusted < TrustLevel::Verified);
        assert!(TrustLevel::Verified < TrustLevel::Community);
        assert!(TrustLevel::Community < TrustLevel::Untrusted);
    }
}
