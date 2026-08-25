//! Translating ACP capability negotiation into domain capabilities.

use agent_client_protocol::schema::v1::InitializeResponse;
use cuma_core::{Capability, CapabilitySet};

/// Map an ACP `initialize` response onto domain capabilities.
///
/// ACP negotiates *protocol* features (can this agent load a session? does it
/// accept images?), not the kind of work an agent is good at. The mapping is
/// therefore partly derived and partly assumed:
///
/// - Protocol features that imply a domain capability are read directly.
/// - Every ACP agent is a coding agent by construction, so the coding
///   baseline is assumed rather than negotiated.
///
/// The assumed baseline is deliberately conservative: it claims only what an
/// agent that speaks ACP at all must be able to do. Anything more specific
/// belongs in configuration, where an operator can state it.
pub fn capabilities_from_initialize(response: &InitializeResponse) -> CapabilitySet {
    let mut capabilities = CapabilitySet::new()
        // The ACP contract is "send a prompt about a codebase, get work back".
        .with(Capability::CodeComprehension)
        .with(Capability::CodeGeneration)
        .with(Capability::CodeEditing)
        .with(Capability::FileSystem)
        .with(Capability::Debugging)
        .with(Capability::Refactoring)
        .with(Capability::Testing)
        .with(Capability::CodeReview)
        .with(Capability::ShellExecution)
        .with(Capability::VersionControl)
        .with(Capability::ToolUse);

    let prompt = &response.agent_capabilities.prompt_capabilities;

    if prompt.image {
        capabilities.insert(Capability::Vision);
    }

    // An agent that can be handed MCP servers can reach documentation and the
    // web through them.
    if response.agent_capabilities.mcp_capabilities.http
        || response.agent_capabilities.mcp_capabilities.sse
    {
        capabilities.insert(Capability::Research);
    }

    capabilities
}

/// The command that launches a well-known ACP agent adapter.
///
/// These are the npm-published adapters the ACP project itself ships. Being
/// able to name an agent instead of a command line is a convenience only —
/// any command that speaks ACP over stdio works.
pub fn well_known_agent_command(name: &str) -> Option<&'static str> {
    match name.trim().to_ascii_lowercase().as_str() {
        "claude" | "claude-code" | "claude-agent" => {
            Some("npx -y @agentclientprotocol/claude-agent-acp@latest")
        }
        "codex" | "openai-codex" => Some("npx -y @agentclientprotocol/codex-acp@latest"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn well_known_agents_resolve_to_their_published_adapters() {
        assert!(
            well_known_agent_command("claude-code")
                .unwrap()
                .contains("claude-agent-acp")
        );
        assert!(well_known_agent_command("Codex").unwrap().contains("codex-acp"));
    }

    #[test]
    fn an_unknown_agent_name_resolves_to_nothing_rather_than_a_guess() {
        assert_eq!(well_known_agent_command("my-custom-agent"), None);
    }
}
