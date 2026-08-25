//! MCP server configuration.

use cuma_core::error::{MetaAgentError, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// How to launch one MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    /// Command to run.
    pub command: String,
    /// Arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variables to set for the child.
    ///
    /// Values may be secret *handles* rather than secrets; see
    /// [`McpServerConfig::resolved_env`].
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Whether this server is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Tools to expose from this server. Empty means all of them.
    ///
    /// An allowlist here is the difference between "the agent can read files"
    /// and "the agent can do whatever this server implements".
    #[serde(default)]
    pub allowed_tools: Vec<String>,
}

fn default_true() -> bool {
    true
}

impl McpServerConfig {
    /// A server launched by `command`.
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            enabled: true,
            allowed_tools: Vec::new(),
        }
    }

    /// Add an argument.
    #[must_use]
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Restrict which tools this server may expose.
    #[must_use]
    pub fn allowing(mut self, tools: Vec<String>) -> Self {
        self.allowed_tools = tools;
        self
    }

    /// Whether `tool` may be exposed from this server.
    pub fn permits(&self, tool: &str) -> bool {
        self.allowed_tools.is_empty() || self.allowed_tools.iter().any(|t| t == tool)
    }

    /// Resolve `$VAR` references in the environment from the process environment.
    ///
    /// Config files carry `GITHUB_TOKEN = "$GH_TOKEN"`, never the token. A
    /// reference that cannot be resolved is dropped with a warning rather than
    /// passed through literally, so a child never receives the string
    /// `"$GH_TOKEN"` and fails with a confusing auth error.
    pub fn resolved_env(&self) -> BTreeMap<String, String> {
        let mut resolved = BTreeMap::new();

        for (key, value) in &self.env {
            match value.strip_prefix('$') {
                Some(handle) => match std::env::var(handle) {
                    Ok(secret) => {
                        resolved.insert(key.clone(), secret);
                    }
                    Err(_) => {
                        tracing::warn!(
                            key,
                            handle,
                            "MCP server environment references an unset variable; omitting it"
                        );
                    }
                },
                None => {
                    resolved.insert(key.clone(), value.clone());
                }
            }
        }

        resolved
    }
}

/// The configured MCP servers.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct McpServerRegistry {
    servers: BTreeMap<String, McpServerConfig>,
}

impl McpServerRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a server.
    pub fn add(&mut self, name: impl Into<String>, config: McpServerConfig) {
        self.servers.insert(name.into(), config);
    }

    /// Look up a server.
    pub fn get(&self, name: &str) -> Option<&McpServerConfig> {
        self.servers.get(name)
    }

    /// Enabled servers.
    pub fn enabled(&self) -> impl Iterator<Item = (&String, &McpServerConfig)> {
        self.servers.iter().filter(|(_, c)| c.enabled)
    }

    /// How many servers are configured.
    pub fn len(&self) -> usize {
        self.servers.len()
    }

    /// Whether nothing is configured.
    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    /// Parse a `.cuma/mcp.toml` file.
    pub fn from_toml(text: &str) -> Result<Self> {
        toml_parse(text)
    }
}

fn toml_parse(text: &str) -> Result<McpServerRegistry> {
    // Parsed via serde_json's Value only to avoid pulling a second TOML
    // dependency chain into this crate; `cuma-config` owns TOML parsing.
    let parsed: BTreeMap<String, McpServerConfig> = match serde_json::from_str(text) {
        Ok(map) => map,
        Err(err) => {
            return Err(MetaAgentError::Configuration(format!(
                "invalid MCP server configuration: {err}"
            )));
        }
    };

    Ok(McpServerRegistry { servers: parsed })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn an_empty_allowlist_permits_every_tool() {
        let config = McpServerConfig::new("git-mcp");
        assert!(config.permits("git_status"));
        assert!(config.permits("git_push"));
    }

    #[test]
    fn an_allowlist_restricts_what_a_server_may_expose() {
        let config = McpServerConfig::new("git-mcp")
            .allowing(vec!["git_status".into(), "git_diff".into()]);

        assert!(config.permits("git_status"));
        assert!(
            !config.permits("git_push"),
            "an allowlist is the difference between reading and writing"
        );
    }

    #[test]
    fn literal_environment_values_pass_through() {
        let mut config = McpServerConfig::new("srv");
        config.env.insert("LOG_LEVEL".into(), "debug".into());

        assert_eq!(config.resolved_env().get("LOG_LEVEL").map(String::as_str), Some("debug"));
    }

    #[test]
    fn an_unresolvable_secret_reference_is_dropped_not_passed_through_literally() {
        let mut config = McpServerConfig::new("srv");
        config
            .env
            .insert("TOKEN".into(), "$CUMA_TEST_DEFINITELY_UNSET_VAR".into());

        assert!(
            !config.resolved_env().contains_key("TOKEN"),
            "passing the literal string \"$VAR\" through would produce a baffling auth error"
        );
    }

    #[test]
    fn disabled_servers_are_not_enumerated() {
        let mut registry = McpServerRegistry::new();
        registry.add("on", McpServerConfig::new("a"));

        let mut off = McpServerConfig::new("b");
        off.enabled = false;
        registry.add("off", off);

        assert_eq!(registry.len(), 2);
        assert_eq!(registry.enabled().count(), 1);
    }
}
