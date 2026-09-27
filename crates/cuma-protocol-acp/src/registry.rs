//! The ACP agent registry.
//!
//! The ACP project publishes a catalogue of agents and how to launch each —
//! an `npx` package, a `uvx` package, or a per-platform binary archive — at
//! [`REGISTRY_URL`]. This module reads it so `cuma agents discover` can show
//! what exists and `cuma agents add` can configure one.
//!
//! The catalogue is remote data. Ids are checked before they become config
//! keys, and launch commands are assembled from parts rather than taken as
//! strings, so a hostile entry cannot smuggle extra shell words or TOML into
//! the configuration it is written to.

use cuma_core::error::{MetaAgentError, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

/// Where the ACP project publishes its registry.
pub const REGISTRY_URL: &str =
    "https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json";

/// The largest registry document accepted.
const MAX_BYTES: usize = 8 * 1024 * 1024;

/// The registry document.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentRegistry {
    /// Format version.
    #[serde(default)]
    pub version: String,
    /// The agents.
    #[serde(default)]
    pub agents: Vec<RegistryAgent>,
}

/// One agent in the registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryAgent {
    /// Registry id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Current stable version.
    #[serde(default)]
    pub version: String,
    /// What it is.
    #[serde(default)]
    pub description: String,
    /// Source repository.
    #[serde(default)]
    pub repository: Option<String>,
    /// Licence identifier.
    #[serde(default)]
    pub license: Option<String>,
    /// How to launch it.
    #[serde(default)]
    pub distribution: Distribution,
    /// The preview channel, when there is one.
    #[serde(default)]
    pub preview: Option<Preview>,
}

/// A preview release.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preview {
    /// Its version.
    pub version: String,
    /// How to launch it (npx or uvx only).
    pub distribution: Distribution,
}

/// The ways an agent is distributed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Distribution {
    /// An npm package run with `npx`.
    #[serde(default)]
    pub npx: Option<Package>,
    /// A Python package run with `uvx`.
    #[serde(default)]
    pub uvx: Option<Package>,
    /// Binary archives, by platform (`linux-x86_64`, `darwin-aarch64`, …).
    #[serde(default)]
    pub binary: BTreeMap<String, Binary>,
}

/// A package-manager distribution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Package {
    /// The package, with its version.
    pub package: String,
    /// Arguments after the package.
    #[serde(default)]
    pub args: Vec<String>,
}

/// A binary archive for one platform.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Binary {
    /// Where to download it.
    pub archive: String,
    /// Its SHA-256.
    #[serde(default)]
    pub sha256: Option<String>,
    /// The executable inside it.
    pub cmd: String,
    /// Arguments that start ACP mode.
    #[serde(default)]
    pub args: Vec<String>,
}

/// How CUMA can launch a registry agent on this machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Launch {
    /// A command that runs the agent, installing it on first use.
    Command {
        /// The full command line.
        command: String,
        /// The launcher it needs on `PATH` (`npx`, `uvx`).
        launcher: String,
    },
    /// A binary archive that must be downloaded and unpacked first.
    Binary {
        /// Where to download it.
        archive: String,
        /// Its SHA-256, to check the download against.
        sha256: Option<String>,
        /// The executable and arguments, relative to the unpacked archive.
        command: String,
    },
    /// Nothing is published for this platform.
    Unsupported,
}

/// This machine's platform, in the registry's spelling.
pub fn current_platform() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    format!("{os}-{}", std::env::consts::ARCH)
}

/// Whether a registry id can safely become a config key and a CLI argument.
pub fn is_plain_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Whether a package spec is a package spec and not something else.
fn is_plain_package(package: &str) -> bool {
    !package.is_empty()
        && !package.starts_with('-')
        && package
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "@/._-+:~=<>![],".contains(c))
}

impl RegistryAgent {
    /// How to launch this agent here, from the stable channel or the preview.
    pub fn launch(&self, preview: bool) -> Launch {
        let distribution = match (&self.preview, preview) {
            (Some(p), true) => &p.distribution,
            _ => &self.distribution,
        };

        let command = |launcher: &str, prefix: &[&str], package: &Package| {
            if !is_plain_package(&package.package) {
                return None;
            }
            let mut words: Vec<String> = prefix.iter().map(|w| (*w).to_owned()).collect();
            words.push(package.package.clone());
            words.extend(package.args.iter().cloned());
            Some(Launch::Command {
                command: shell_words::join(&words),
                launcher: launcher.to_owned(),
            })
        };

        if let Some(launch) = distribution
            .npx
            .as_ref()
            .and_then(|p| command("npx", &["npx", "-y"], p))
        {
            return launch;
        }
        if let Some(launch) = distribution
            .uvx
            .as_ref()
            .and_then(|p| command("uvx", &["uvx"], p))
        {
            return launch;
        }
        match distribution.binary.get(&current_platform()) {
            Some(binary) if binary.archive.starts_with("https://") => {
                let mut words = vec![binary.cmd.clone()];
                words.extend(binary.args.iter().cloned());
                Launch::Binary {
                    archive: binary.archive.clone(),
                    sha256: binary.sha256.clone(),
                    command: shell_words::join(&words),
                }
            }
            _ => Launch::Unsupported,
        }
    }
}

/// Parse a registry document.
pub fn parse_registry(json: &str) -> Result<AgentRegistry> {
    let registry: AgentRegistry = serde_json::from_str(json).map_err(|err| {
        MetaAgentError::protocol_msg("acp", format!("the ACP registry is malformed: {err}"))
    })?;
    Ok(AgentRegistry {
        agents: registry
            .agents
            .into_iter()
            .filter(|agent| {
                let plain = is_plain_id(&agent.id);
                if !plain {
                    tracing::warn!(
                        id = agent.id,
                        "ignoring a registry agent whose id is not a plain identifier"
                    );
                }
                plain
            })
            .collect(),
        ..registry
    })
}

/// A registry, and whether it came from the cache because the network failed.
#[derive(Debug, Clone)]
pub struct Fetched {
    /// The registry.
    pub registry: AgentRegistry,
    /// Set when the live fetch failed and a cached copy was used instead.
    pub stale: Option<String>,
}

/// Fetch the registry, caching it at `cache`, and falling back to the cached
/// copy — marked stale — when the network fails.
pub async fn fetch_registry(url: &str, cache: &Path) -> Result<Fetched> {
    if !url.starts_with("https://") {
        return Err(MetaAgentError::Security(format!(
            "the ACP registry must be fetched over https, not {url:?}"
        )));
    }

    match download(url).await {
        Ok(body) => {
            let registry = parse_registry(&body)?;
            if let Some(parent) = cache.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            if let Err(err) = tokio::fs::write(cache, &body).await {
                tracing::debug!(error = %err, "could not cache the ACP registry");
            }
            Ok(Fetched {
                registry,
                stale: None,
            })
        }
        Err(err) => {
            let cached = tokio::fs::read_to_string(cache).await.map_err(|_| {
                MetaAgentError::protocol_msg(
                    "acp",
                    format!("cannot fetch the ACP registry and nothing is cached: {err}"),
                )
            })?;
            Ok(Fetched {
                registry: parse_registry(&cached)?,
                stale: Some(err.to_string()),
            })
        }
    }
}

async fn download(url: &str) -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("cuma/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|err| MetaAgentError::Configuration(err.to_string()))?;
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|err| MetaAgentError::protocol_msg("acp", format!("{url}: {err}")))?;
    if !response.status().is_success() {
        return Err(MetaAgentError::protocol_msg(
            "acp",
            format!("{url} returned {}", response.status()),
        ));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|err| MetaAgentError::protocol_msg("acp", err.to_string()))?
    {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_BYTES {
            return Err(MetaAgentError::Security(format!(
                "{url} exceeds {MAX_BYTES} bytes"
            )));
        }
    }
    String::from_utf8(body)
        .map_err(|_| MetaAgentError::protocol_msg("acp", "the registry is not UTF-8"))
}

/// The TOML for an `[agents.<id>]` entry launching `command`.
///
/// Values go through the TOML serializer, so a hostile command string cannot
/// close its quotes and add keys of its own.
pub fn config_entry(id: &str, command: &str) -> Result<String> {
    if !is_plain_id(id) {
        return Err(MetaAgentError::Security(format!(
            "{id:?} is not a plain agent id"
        )));
    }
    Ok(format!(
        "\n[agents.{id}]\nprotocol = \"acp\"\ncommand = {}\n",
        toml_string(command)
    ))
}

/// A TOML basic string, escaped.
fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    const SAMPLE: &str = r#"{
      "version": "1.0.0",
      "agents": [
        { "id": "claude-acp", "name": "Claude Agent", "version": "0.81.2",
          "distribution": { "npx": { "package": "@agentclientprotocol/claude-agent-acp@0.81.2" } },
          "preview": { "version": "0.82.0-preview.1",
            "distribution": { "npx": { "package": "@agentclientprotocol/claude-agent-acp@0.82.0-preview.1" } } } },
        { "id": "fast-agent", "name": "fast-agent",
          "distribution": { "uvx": { "package": "fast-agent-acp==0.3.1", "args": ["-x"] } } },
        { "id": "goose", "name": "goose",
          "distribution": { "binary": {
            "linux-x86_64": { "archive": "https://example.invalid/goose.tar.bz2", "sha256": "ab", "cmd": "./goose", "args": ["acp"] },
            "darwin-aarch64": { "archive": "https://example.invalid/goose-mac.tar.bz2", "cmd": "./goose", "args": ["acp"] },
            "linux-aarch64": { "archive": "https://example.invalid/goose-arm.tar.bz2", "cmd": "./goose", "args": ["acp"] } } } },
        { "id": "../evil", "name": "evil", "distribution": {} }
      ]
    }"#;

    #[test]
    fn the_registry_parses_and_drops_ids_that_are_not_identifiers() {
        let registry = parse_registry(SAMPLE).unwrap();
        let ids: Vec<_> = registry.agents.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["claude-acp", "fast-agent", "goose"]);
    }

    #[test]
    fn npx_and_uvx_agents_get_a_command() {
        let registry = parse_registry(SAMPLE).unwrap();
        assert_eq!(
            registry.agents[0].launch(false),
            Launch::Command {
                command: "npx -y @agentclientprotocol/claude-agent-acp@0.81.2".into(),
                launcher: "npx".into()
            }
        );
        assert!(matches!(
            registry.agents[0].launch(true),
            Launch::Command { command, .. } if command.contains("preview")
        ));
        assert_eq!(
            registry.agents[1].launch(false),
            Launch::Command {
                command: "uvx 'fast-agent-acp==0.3.1' -x".into(),
                launcher: "uvx".into()
            }
        );
    }

    #[test]
    fn a_binary_agent_is_described_not_downloaded() {
        let registry = parse_registry(SAMPLE).unwrap();
        match registry.agents[2].launch(false) {
            Launch::Binary {
                archive, command, ..
            } => {
                assert!(archive.starts_with("https://"));
                assert_eq!(command, "./goose acp");
            }
            Launch::Unsupported => {
                // A platform the sample does not cover.
                assert!(
                    !["linux-x86_64", "linux-aarch64", "darwin-aarch64"]
                        .contains(&current_platform().as_str())
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_package_that_looks_like_a_flag_is_refused() {
        let agent: RegistryAgent = serde_json::from_str(
            r#"{ "id": "x", "name": "x", "distribution": { "npx": { "package": "--registry=https://evil" } } }"#,
        )
        .unwrap();
        assert_eq!(agent.launch(false), Launch::Unsupported);
    }

    #[test]
    fn a_config_entry_cannot_be_used_to_inject_keys() {
        let entry = config_entry("x", "npx -y pkg\"\nenabled = false\n[agents.y").unwrap();
        let parsed: toml::Value = toml::from_str(&entry).unwrap();
        let table = parsed["agents"]["x"].as_table().unwrap();
        assert_eq!(table.len(), 2, "only protocol and command: {entry}");
        assert!(parsed["agents"].get("y").is_none());
        assert!(config_entry("../x", "npx").is_err());
    }

    #[tokio::test]
    async fn a_cached_registry_is_used_when_the_network_fails() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("registry.json");
        std::fs::write(&cache, SAMPLE).unwrap();

        let fetched = fetch_registry("https://127.0.0.1:1/registry.json", &cache)
            .await
            .unwrap();
        assert!(fetched.stale.is_some());
        assert_eq!(fetched.registry.agents.len(), 3);
    }

    #[tokio::test]
    async fn a_cleartext_registry_url_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            fetch_registry("http://example.invalid/r.json", &dir.path().join("r"))
                .await
                .is_err()
        );
    }
}
