//! Every sandbox the configuration declares, and which agent runs under which.
//!
//! An agent runs under its own `sandbox`, else `security.agent_sandbox`.
//! Either may name a `[sandboxes.<name>]` section or a built-in: `auto` (the
//! native runtimes, in order) or one native runtime. With `security.sandbox`
//! off, nothing is confined.

use crate::arcbox::ArcboxProvider;
use crate::command::CommandProvider;
use crate::docker::DockerProvider;
use crate::e2b::E2bProvider;
use crate::kubernetes::KubernetesProvider;
use crate::microsandbox::MicrosandboxProvider;
use crate::native::NativeProvider;
use crate::opensandbox::OpenSandboxProvider;
use crate::plugin::PluginProvider;
use crate::wasmer::WasmerProvider;
use crate::{Capabilities, SandboxProvider};
use cuma_config::sandbox::{AUTO, NATIVE_RUNTIMES, NativeSandbox};
use cuma_config::{Config, SandboxSettings, SecurityConfig};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

/// The sandboxes of one configuration.
pub struct Registry {
    providers: BTreeMap<String, Arc<dyn SandboxProvider>>,
    /// Agent id → the sandbox it names, resolved against the default.
    assignments: BTreeMap<String, String>,
    default: String,
    enabled: bool,
}

/// One row of `cuma sandbox list`.
#[derive(Debug, Clone, Serialize)]
pub struct SandboxEntry {
    /// Its name.
    pub name: String,
    /// Its kind.
    pub kind: &'static str,
    /// What it can and cannot do.
    #[serde(flatten)]
    pub capabilities: Capabilities,
    /// The program it needs.
    pub program: String,
    /// Whether it is `security.agent_sandbox`.
    pub default: bool,
    /// The agents that run under it.
    pub agents: Vec<String>,
}

/// Build the provider for one configured sandbox.
pub fn build(
    name: &str,
    settings: &SandboxSettings,
    security: &SecurityConfig,
    cuma: &Option<PathBuf>,
) -> Arc<dyn SandboxProvider> {
    let name = name.to_owned();
    match settings.clone() {
        SandboxSettings::Native(native) => Arc::new(NativeProvider::new(name, &native, security)),
        SandboxSettings::Docker(docker) => Arc::new(DockerProvider::new(name, docker)),
        SandboxSettings::Microsandbox(msb) => Arc::new(MicrosandboxProvider::new(name, msb)),
        SandboxSettings::Arcbox(arcbox) => Arc::new(ArcboxProvider::new(name, arcbox)),
        SandboxSettings::Kubernetes(k8s) => Arc::new(KubernetesProvider::new(name, k8s)),
        SandboxSettings::E2b(e2b) => Arc::new(E2bProvider::new(name, e2b, cuma.clone())),
        SandboxSettings::Opensandbox(osb) => {
            Arc::new(OpenSandboxProvider::new(name, osb, cuma.clone()))
        }
        SandboxSettings::Wasmer(wasmer) => Arc::new(WasmerProvider::new(name, wasmer)),
        SandboxSettings::Command(command) => Arc::new(CommandProvider::new(name, command)),
        SandboxSettings::Plugin(plugin) => Arc::new(PluginProvider::new(name, plugin)),
    }
}

impl Registry {
    /// The sandboxes `config` declares. `cuma` is CUMA's own executable,
    /// which providers that only speak HTTP launch agents through.
    ///
    /// Built-in names are resolved only when something uses them: detecting
    /// a native runtime runs it.
    pub fn from_config(config: &Config, cuma: Option<PathBuf>) -> Self {
        let security = &config.security;
        let mut providers: BTreeMap<String, Arc<dyn SandboxProvider>> = config
            .sandboxes
            .iter()
            .map(|(name, settings)| (name.clone(), build(name, settings, security, &cuma)))
            .collect();

        let default = security.agent_sandbox.clone();
        let assignments: BTreeMap<String, String> = config
            .agents
            .iter()
            .filter(|(_, agent)| agent.enabled)
            .map(|(id, agent)| {
                (
                    id.clone(),
                    agent.sandbox.clone().unwrap_or_else(|| default.clone()),
                )
            })
            .collect();

        if security.sandbox {
            let used = assignments.values().chain(std::iter::once(&default));
            for name in used {
                if providers.contains_key(name) {
                    continue;
                }
                let runtime = if name == AUTO || NATIVE_RUNTIMES.contains(&name.as_str()) {
                    name.clone()
                } else {
                    // Validation rejects unknown names; never guess one.
                    continue;
                };
                let native = NativeSandbox { runtime };
                providers.insert(
                    name.clone(),
                    Arc::new(NativeProvider::new(name.clone(), &native, security)),
                );
            }
        }

        Self {
            providers,
            assignments,
            default,
            enabled: security.sandbox,
        }
    }

    /// The sandbox an agent runs under, or `None` when sandboxing is off.
    pub fn for_agent(&self, agent: &str) -> Option<Arc<dyn SandboxProvider>> {
        if !self.enabled {
            return None;
        }
        let name = self.assignments.get(agent).unwrap_or(&self.default);
        self.providers.get(name).cloned()
    }

    /// The default sandbox, or `None` when sandboxing is off.
    pub fn default_provider(&self) -> Option<Arc<dyn SandboxProvider>> {
        if !self.enabled {
            return None;
        }
        self.providers.get(&self.default).cloned()
    }

    /// A sandbox by name: a configured one, or a built-in in use.
    pub fn get(&self, name: &str) -> Option<Arc<dyn SandboxProvider>> {
        self.providers.get(name).cloned()
    }

    /// Whether sandboxing is on.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Every sandbox, for listing.
    pub fn entries(&self) -> Vec<SandboxEntry> {
        self.providers
            .iter()
            .map(|(name, provider)| SandboxEntry {
                name: name.clone(),
                kind: provider.kind(),
                capabilities: provider.capabilities(),
                program: provider.program(),
                default: *name == self.default,
                agents: self
                    .assignments
                    .iter()
                    .filter(|(_, sandbox)| *sandbox == name)
                    .map(|(agent, _)| agent.clone())
                    .collect(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn config(toml: &str) -> Config {
        let config = Config::from_toml(toml).unwrap();
        config.validate().unwrap();
        config
    }

    #[test]
    fn an_agent_uses_its_own_sandbox_else_the_default() {
        let registry = Registry::from_config(
            &config(
                r#"
                [security]
                agent_sandbox = "box"
                [sandboxes.box]
                kind = "docker"
                image = "node:22"
                [sandboxes.vm]
                kind = "microsandbox"
                image = "node:22"
                [agents.a]
                command = "a"
                [agents.b]
                command = "b"
                sandbox = "vm"
                "#,
            ),
            None,
        );
        assert_eq!(registry.for_agent("a").unwrap().kind(), "docker");
        assert_eq!(registry.for_agent("b").unwrap().kind(), "microsandbox");
        assert_eq!(registry.for_agent("unknown").unwrap().name(), "box");

        let entries = registry.entries();
        let vm = entries.iter().find(|e| e.name == "vm").unwrap();
        assert_eq!(vm.agents, ["b"]);
        assert!(entries.iter().find(|e| e.name == "box").unwrap().default);
    }

    #[test]
    fn with_sandboxing_off_nothing_is_confined() {
        let registry = Registry::from_config(
            &config(
                "[security]\nsandbox = false\nagent_sandbox = \"box\"\n\
                 [sandboxes.box]\nkind = \"docker\"\nimage = \"x\"\n",
            ),
            None,
        );
        assert!(registry.for_agent("a").is_none());
        assert!(registry.default_provider().is_none());
    }

    #[test]
    fn a_built_in_name_resolves_to_the_native_runtime() {
        let registry = Registry::from_config(
            &config("[security]\nagent_sandbox = \"sandbox-exec\"\n"),
            None,
        );
        let provider = registry.default_provider().unwrap();
        assert_eq!(provider.kind(), "native");
        assert_eq!(provider.name(), "sandbox-exec");
    }

    #[test]
    fn every_kind_builds() {
        let registry = Registry::from_config(
            &config(
                r#"
                [sandboxes.n]
                kind = "native"
                [sandboxes.d]
                kind = "docker"
                image = "i"
                [sandboxes.m]
                kind = "microsandbox"
                image = "i"
                [sandboxes.a]
                kind = "arcbox"
                image = "i"
                [sandboxes.k]
                kind = "kubernetes"
                image = "i"
                [sandboxes.e]
                kind = "e2b"
                template = "t"
                [sandboxes.o]
                kind = "opensandbox"
                server_url = "http://localhost:8080"
                image = "i"
                [sandboxes.w]
                kind = "wasmer"
                [sandboxes.c]
                kind = "command"
                prefix = ["env"]
                [sandboxes.p]
                kind = "plugin"
                program = "/bin/false"
                "#,
            ),
            None,
        );
        let kinds: Vec<&str> = registry.entries().iter().map(|e| e.kind).collect();
        for kind in [
            "native",
            "docker",
            "microsandbox",
            "arcbox",
            "kubernetes",
            "e2b",
            "opensandbox",
            "wasmer",
            "command",
            "plugin",
        ] {
            assert!(kinds.contains(&kind), "{kind}");
        }
    }
}
