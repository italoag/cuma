//! Agent sandbox providers: `[sandboxes.<name>]`.
//!
//! Each entry names a provider by `kind` and carries that kind's settings.
//! Which agent runs under which is `security.agent_sandbox` and
//! `[agents.<id>] sandbox`. The operator's guide is `docs/SANDBOXES.md`.

use cuma_core::error::{MetaAgentError, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The native runtimes, by the names configuration uses for them.
pub const NATIVE_RUNTIMES: [&str; 4] = ["ai-jail", "bubblewrap", "sandbox-exec", "firejail"];

/// What `security.agent_sandbox` defaults to: the native runtimes, in order.
pub const AUTO: &str = "auto";

/// One `[sandboxes.<name>]` entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum SandboxSettings {
    /// ai-jail, bubblewrap, `sandbox-exec` or firejail.
    Native(NativeSandbox),
    /// A container engine speaking Docker's CLI.
    Docker(DockerSandbox),
    /// microsandbox microVMs, through `msb`.
    Microsandbox(MicrosandboxSandbox),
    /// ArcBox sandbox microVMs, through `abctl`.
    Arcbox(ArcboxSandbox),
    /// kubernetes-sigs/agent-sandbox, through `kubectl`.
    Kubernetes(KubernetesSandbox),
    /// The E2B API: CubeSandbox, or E2B itself.
    E2b(E2bSandbox),
    /// An OpenSandbox server.
    Opensandbox(OpenSandboxSandbox),
    /// The Wasmer runtime.
    Wasmer(WasmerSandbox),
    /// A configured prefix.
    Command(CommandSandbox),
    /// An external program speaking the plugin protocol.
    Plugin(PluginSandbox),
}

/// `kind = "native"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NativeSandbox {
    /// `auto`, or one of [`NATIVE_RUNTIMES`].
    pub runtime: String,
}

impl Default for NativeSandbox {
    fn default() -> Self {
        Self {
            runtime: AUTO.to_owned(),
        }
    }
}

/// `kind = "docker"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DockerSandbox {
    /// The image; it must contain the agent's runtime.
    pub image: String,
    /// `docker`, `podman`, `nerdctl`, or a path.
    pub program: String,
    /// A docker context: ArcBox, OrbStack, Rancher Desktop, Colima…
    pub context: Option<String>,
    /// An OCI runtime: `runsc` (gVisor), `kata`, `kata-fc`…
    pub runtime: Option<String>,
    /// `docker --network`.
    pub network: String,
    /// The guest home the agent's `state` is mounted under.
    pub home: String,
    /// `docker --user`; default: the caller's uid:gid on Linux.
    pub user: Option<String>,
    /// `docker --memory`.
    pub memory: Option<String>,
    /// `docker --cpus`.
    pub cpus: Option<String>,
    /// `docker --entrypoint`; an empty string resets the image's.
    pub entrypoint: Option<String>,
    /// Appended to `docker run`, before the image.
    pub extra_args: Vec<String>,
}

impl Default for DockerSandbox {
    fn default() -> Self {
        Self {
            image: String::new(),
            program: "docker".to_owned(),
            context: None,
            runtime: None,
            network: "bridge".to_owned(),
            home: "/root".to_owned(),
            user: None,
            memory: None,
            cpus: None,
            entrypoint: None,
            extra_args: Vec::new(),
        }
    }
}

/// `kind = "microsandbox"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MicrosandboxSandbox {
    /// The OCI image.
    pub image: String,
    /// The `msb` executable.
    pub program: String,
    /// Virtual CPUs.
    pub cpus: Option<u32>,
    /// Memory, such as `2G`.
    pub memory: Option<String>,
    /// The guest home the agent's `state` is mounted under.
    pub home: String,
    /// Variable name → the hosts its real value may be sent to. The guest
    /// sees a placeholder; `msb` substitutes the value at the egress.
    pub secrets: BTreeMap<String, Vec<String>>,
    /// Non-secret variables, with their values written here.
    pub env: BTreeMap<String, String>,
}

impl Default for MicrosandboxSandbox {
    fn default() -> Self {
        Self {
            image: String::new(),
            program: "msb".to_owned(),
            cpus: None,
            memory: None,
            home: "/root".to_owned(),
            secrets: BTreeMap::new(),
            env: BTreeMap::new(),
        }
    }
}

/// `kind = "arcbox"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ArcboxSandbox {
    /// A Docker image in ArcBox's image store.
    pub image: Option<String>,
    /// A catalog template, `name[:version]`.
    pub template: Option<String>,
    /// A Dockerfile, built inside ArcBox.
    pub dockerfile: Option<String>,
    /// The `abctl` executable.
    pub program: String,
    /// Virtual CPUs.
    pub cpus: Option<u32>,
    /// Memory in MiB.
    pub memory_mib: Option<u32>,
    /// Hard lifetime: a sandbox CUMA failed to remove still dies.
    pub ttl_secs: u64,
    /// The guest user the agent runs as.
    pub user: Option<String>,
}

impl Default for ArcboxSandbox {
    fn default() -> Self {
        Self {
            image: None,
            template: None,
            dockerfile: None,
            program: "abctl".to_owned(),
            cpus: None,
            memory_mib: None,
            ttl_secs: 3600,
            user: None,
        }
    }
}

/// `kind = "kubernetes"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KubernetesSandbox {
    /// An image: CUMA creates a `Sandbox` around it.
    pub image: Option<String>,
    /// A `SandboxWarmPool`: CUMA claims a sandbox from it.
    pub warm_pool: Option<String>,
    /// The `kubectl` executable.
    pub program: String,
    /// A kubeconfig context.
    pub context: Option<String>,
    /// A kubeconfig file.
    pub kubeconfig: Option<String>,
    /// The namespace sandboxes are created in.
    pub namespace: String,
    /// The container the agent runs in.
    pub container: String,
    /// `runtimeClassName`: `gvisor`, `kata`, `kata-fc`…
    pub runtime_class: Option<String>,
    /// `serviceAccountName`.
    pub service_account: Option<String>,
    /// How long to wait for the sandbox to become ready.
    pub ready_timeout_secs: u64,
    /// `shutdownTime`, from creation: the controller reaps what CUMA could not.
    pub lifetime_secs: u64,
}

impl Default for KubernetesSandbox {
    fn default() -> Self {
        Self {
            image: None,
            warm_pool: None,
            program: "kubectl".to_owned(),
            context: None,
            kubeconfig: None,
            namespace: "default".to_owned(),
            container: "agent".to_owned(),
            runtime_class: None,
            service_account: None,
            ready_timeout_secs: 300,
            lifetime_secs: 3600,
        }
    }
}

/// `kind = "e2b"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct E2bSandbox {
    /// The lifecycle API: CubeAPI, or `https://api.e2b.app`.
    pub api_url: String,
    /// Where `<port>-<sandbox>.<domain>` reaches a sandbox's envd.
    pub domain: String,
    /// The template sandboxes are created from.
    pub template: String,
    /// The variable holding the API key — a handle, never the key.
    pub api_key_ref: Option<String>,
    /// `https`, or `http` for a deployment without TLS.
    pub envd_scheme: String,
    /// envd's port.
    pub envd_port: u16,
    /// The sandbox's lifetime.
    pub timeout_secs: u64,
    /// The guest user.
    pub user: String,
    /// `allow_internet_access`.
    pub internet: bool,
}

impl Default for E2bSandbox {
    fn default() -> Self {
        Self {
            api_url: "https://api.e2b.app".to_owned(),
            domain: "e2b.app".to_owned(),
            template: String::new(),
            api_key_ref: Some("E2B_API_KEY".to_owned()),
            envd_scheme: "https".to_owned(),
            envd_port: 49983,
            timeout_secs: 3600,
            user: "user".to_owned(),
            internet: true,
        }
    }
}

/// `kind = "opensandbox"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OpenSandboxSandbox {
    /// The lifecycle API's base URL.
    pub server_url: String,
    /// The variable holding the API key.
    pub api_key_ref: Option<String>,
    /// The image; it must contain `node`, which runs the stdio tunnel.
    pub image: String,
    /// CPU limit.
    pub cpu: String,
    /// Memory limit.
    pub memory: String,
    /// Mount the workspace as a host-path volume instead of copying it.
    pub mount_workspace: bool,
    /// The sandbox's lifetime.
    pub timeout_secs: u64,
    /// The port the stdio tunnel listens on inside the sandbox.
    pub tunnel_port: u16,
}

impl Default for OpenSandboxSandbox {
    fn default() -> Self {
        Self {
            server_url: String::new(),
            api_key_ref: None,
            image: String::new(),
            cpu: "1".to_owned(),
            memory: "2Gi".to_owned(),
            mount_workspace: false,
            timeout_secs: 3600,
            tunnel_port: 7681,
        }
    }
}

/// `kind = "wasmer"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WasmerSandbox {
    /// The `wasmer` executable.
    pub program: String,
    /// Appended to `wasmer run`, before the package.
    pub extra_args: Vec<String>,
}

impl Default for WasmerSandbox {
    fn default() -> Self {
        Self {
            program: "wasmer".to_owned(),
            extra_args: Vec::new(),
        }
    }
}

/// `kind = "command"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CommandSandbox {
    /// The prefix; `{workspace}`, `{id}` and `{home}` are substituted.
    pub prefix: Vec<String>,
    /// How to check it works; default: the prefix running `true`.
    pub probe: Vec<String>,
    /// Run before each launch.
    pub setup: Vec<String>,
    /// Run after each launch.
    pub teardown: Vec<String>,
    /// What it isolates with, as reported: `process`, `container`,
    /// `microvm`, `wasm` or `remote`.
    pub isolation: String,
}

impl Default for CommandSandbox {
    fn default() -> Self {
        Self {
            prefix: Vec::new(),
            probe: Vec::new(),
            setup: Vec::new(),
            teardown: Vec::new(),
            isolation: "process".to_owned(),
        }
    }
}

/// `kind = "plugin"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PluginSandbox {
    /// The plugin's executable.
    pub program: String,
    /// The bound on each plugin operation.
    pub timeout_secs: u64,
    /// Handed to the plugin as JSON.
    pub options: toml::Table,
}

impl Default for PluginSandbox {
    fn default() -> Self {
        Self {
            program: String::new(),
            timeout_secs: 120,
            options: toml::Table::new(),
        }
    }
}

/// The isolation names a `command` sandbox may report.
pub const ISOLATIONS: [&str; 5] = ["process", "container", "microvm", "wasm", "remote"];

impl SandboxSettings {
    /// The `kind` this entry was written with.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Native(_) => "native",
            Self::Docker(_) => "docker",
            Self::Microsandbox(_) => "microsandbox",
            Self::Arcbox(_) => "arcbox",
            Self::Kubernetes(_) => "kubernetes",
            Self::E2b(_) => "e2b",
            Self::Opensandbox(_) => "opensandbox",
            Self::Wasmer(_) => "wasmer",
            Self::Command(_) => "command",
            Self::Plugin(_) => "plugin",
        }
    }

    /// Reject settings that cannot work, naming the field.
    pub fn validate(&self, name: &str) -> Result<()> {
        let field = |key: &str| format!("sandboxes.{name}.{key}");
        let invalid = |message: String| Err(MetaAgentError::Configuration(message));
        let required = |key: &str, value: &str| {
            if value.trim().is_empty() {
                invalid(format!("{} must be set", field(key)))
            } else {
                Ok(())
            }
        };
        let absolute = |key: &str, value: &str| {
            if value.starts_with('/') {
                Ok(())
            } else {
                invalid(format!(
                    "{} must be an absolute path, got {value:?}",
                    field(key)
                ))
            }
        };
        let url = |key: &str, value: &str| {
            if value.starts_with("http://") || value.starts_with("https://") {
                Ok(())
            } else {
                invalid(format!(
                    "{} must be an http(s) URL, got {value:?}",
                    field(key)
                ))
            }
        };
        let one_of = |keys: &[&str], values: &[&Option<String>]| {
            let set = values.iter().filter(|v| v.is_some()).count();
            if set == 1 {
                Ok(())
            } else {
                let names: Vec<String> = keys.iter().map(|k| field(k)).collect();
                invalid(format!("exactly one of {} must be set", names.join(", ")))
            }
        };
        let positive = |key: &str, value: u64| {
            if value > 0 {
                Ok(())
            } else {
                invalid(format!("{} must be positive", field(key)))
            }
        };

        match self {
            Self::Native(native) => {
                if native.runtime != AUTO && !NATIVE_RUNTIMES.contains(&native.runtime.as_str()) {
                    return invalid(format!(
                        "{} must be auto or one of {}, got {:?}",
                        field("runtime"),
                        NATIVE_RUNTIMES.join(", "),
                        native.runtime
                    ));
                }
            }
            Self::Docker(docker) => {
                required("image", &docker.image)?;
                required("program", &docker.program)?;
                required("network", &docker.network)?;
                absolute("home", &docker.home)?;
            }
            Self::Microsandbox(msb) => {
                required("image", &msb.image)?;
                required("program", &msb.program)?;
                absolute("home", &msb.home)?;
                for (variable, hosts) in &msb.secrets {
                    if !is_env_name(variable) {
                        return invalid(format!(
                            "{} is not a variable name",
                            field(&format!("secrets.{variable}"))
                        ));
                    }
                    if hosts.is_empty() || hosts.iter().any(|h| h.trim().is_empty()) {
                        return invalid(format!(
                            "{} must name the hosts the secret may be sent to",
                            field(&format!("secrets.{variable}"))
                        ));
                    }
                }
                if let Some(variable) = msb.env.keys().find(|k| !is_env_name(k)) {
                    return invalid(format!(
                        "{} is not a variable name",
                        field(&format!("env.{variable}"))
                    ));
                }
            }
            Self::Arcbox(arcbox) => {
                one_of(
                    &["image", "template", "dockerfile"],
                    &[&arcbox.image, &arcbox.template, &arcbox.dockerfile],
                )?;
                required("program", &arcbox.program)?;
                positive("ttl_secs", arcbox.ttl_secs)?;
            }
            Self::Kubernetes(k8s) => {
                one_of(&["image", "warm_pool"], &[&k8s.image, &k8s.warm_pool])?;
                required("program", &k8s.program)?;
                for (key, value) in [("namespace", &k8s.namespace), ("container", &k8s.container)] {
                    if !is_dns_label(value) {
                        return invalid(format!(
                            "{} must be a lowercase DNS label, got {value:?}",
                            field(key)
                        ));
                    }
                }
                positive("ready_timeout_secs", k8s.ready_timeout_secs)?;
                positive("lifetime_secs", k8s.lifetime_secs)?;
            }
            Self::E2b(e2b) => {
                url("api_url", &e2b.api_url)?;
                required("domain", &e2b.domain)?;
                required("template", &e2b.template)?;
                required("user", &e2b.user)?;
                if !matches!(e2b.envd_scheme.as_str(), "http" | "https") {
                    return invalid(format!("{} must be http or https", field("envd_scheme")));
                }
                positive("envd_port", u64::from(e2b.envd_port))?;
                positive("timeout_secs", e2b.timeout_secs)?;
                if let Some(handle) = &e2b.api_key_ref
                    && !is_env_name(handle)
                {
                    return invalid(format!("{} must name a variable", field("api_key_ref")));
                }
            }
            Self::Opensandbox(osb) => {
                url("server_url", &osb.server_url)?;
                required("image", &osb.image)?;
                required("cpu", &osb.cpu)?;
                required("memory", &osb.memory)?;
                positive("tunnel_port", u64::from(osb.tunnel_port))?;
                // OpenSandbox's minimum sandbox lifetime.
                if osb.timeout_secs < 60 {
                    return invalid(format!("{} must be at least 60", field("timeout_secs")));
                }
                if let Some(handle) = &osb.api_key_ref
                    && !is_env_name(handle)
                {
                    return invalid(format!("{} must name a variable", field("api_key_ref")));
                }
            }
            Self::Wasmer(wasmer) => required("program", &wasmer.program)?,
            Self::Command(command) => {
                if command
                    .prefix
                    .first()
                    .is_none_or(|word| word.trim().is_empty())
                {
                    return invalid(format!("{} must name a program", field("prefix")));
                }
                if !ISOLATIONS.contains(&command.isolation.as_str()) {
                    return invalid(format!(
                        "{} must be one of {}",
                        field("isolation"),
                        ISOLATIONS.join(", ")
                    ));
                }
            }
            Self::Plugin(plugin) => {
                required("program", &plugin.program)?;
                positive("timeout_secs", plugin.timeout_secs)?;
            }
        }
        Ok(())
    }
}

/// Whether `name` is something `security.agent_sandbox` or an agent's
/// `sandbox` may name: `auto`, a native runtime, or a defined sandbox.
pub fn is_known_sandbox(name: &str, sandboxes: &BTreeMap<String, SandboxSettings>) -> bool {
    name == AUTO || NATIVE_RUNTIMES.contains(&name) || sandboxes.contains_key(name)
}

/// A sandbox's own name: a plain identifier that is not a built-in name.
pub fn check_sandbox_name(name: &str) -> Result<()> {
    let plain = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !plain {
        return Err(MetaAgentError::Configuration(format!(
            "sandboxes.{name}: names must be letters, digits, '-' or '_'"
        )));
    }
    if name == AUTO || NATIVE_RUNTIMES.contains(&name) {
        return Err(MetaAgentError::Configuration(format!(
            "sandboxes.{name}: {name:?} is a built-in name; choose another"
        )));
    }
    Ok(())
}

/// A portable environment variable name.
pub fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A Kubernetes DNS-1123 label.
fn is_dns_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !value.starts_with('-')
        && !value.ends_with('-')
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::Config;
    use std::collections::BTreeSet;

    /// Every ```toml block in a document.
    fn toml_blocks(document: &str) -> Vec<String> {
        document
            .split("```toml\n")
            .skip(1)
            .filter_map(|rest| rest.split_once("\n```").map(|(block, _)| block.to_owned()))
            .collect()
    }

    #[test]
    fn every_sandbox_kind_parses_from_its_documented_example() {
        let mut kinds = BTreeSet::new();
        // The guide's examples are whole configurations; the reference in
        // CONFIGURATION.md lists every key at once, so only its sandboxes are
        // checked on their own.
        for (document, text, whole) in [
            (
                "SANDBOXES.md",
                include_str!("../../../docs/SANDBOXES.md"),
                true,
            ),
            (
                "CONFIGURATION.md",
                include_str!("../../../docs/CONFIGURATION.md"),
                false,
            ),
        ] {
            for block in toml_blocks(text) {
                if !block.contains("[sandboxes.") && !(whole && block.contains("[agents.")) {
                    continue;
                }
                let config = Config::from_toml(&block)
                    .unwrap_or_else(|err| panic!("{document}: {err}\n{block}"));
                if whole {
                    config
                        .validate()
                        .unwrap_or_else(|err| panic!("{document}: {err}\n{block}"));
                }
                for (name, sandbox) in &config.sandboxes {
                    check_sandbox_name(name).unwrap();
                    sandbox
                        .validate(name)
                        .unwrap_or_else(|err| panic!("{document}: {err}\n{block}"));
                }
                kinds.extend(config.sandboxes.values().map(SandboxSettings::kind));
            }
        }
        let all: BTreeSet<&str> = [
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
        ]
        .into_iter()
        .collect();
        assert_eq!(kinds, all, "every kind has a documented, valid example");
    }

    #[test]
    fn an_unknown_key_in_a_sandbox_is_rejected() {
        let err = Config::from_toml(
            "[sandboxes.c]\nkind = \"docker\"\nimage = \"node\"\nimgae = \"x\"\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("imgae"), "{err}");
    }

    #[test]
    fn an_unknown_kind_is_rejected() {
        let err = Config::from_toml("[sandboxes.c]\nkind = \"vagrant\"\n").unwrap_err();
        assert!(err.to_string().contains("vagrant"), "{err}");
    }

    #[test]
    fn an_agent_naming_an_undefined_sandbox_is_rejected() {
        let config =
            Config::from_toml("[agents.devin]\nprotocol = \"acp\"\nsandbox = \"vm\"\n").unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("agents.devin.sandbox"), "{err}");

        let mut config = Config::default();
        config.security.agent_sandbox = "nowhere".into();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("security.agent_sandbox"), "{err}");

        // Built-in names need no section.
        let config = Config::from_toml(
            "[security]\nagent_sandbox = \"sandbox-exec\"\n[agents.a]\nsandbox = \"auto\"\n",
        )
        .unwrap();
        config.validate().unwrap();
    }

    #[test]
    fn agent_state_and_env_are_read_and_checked() {
        let config = Config::from_toml(
            r#"
            [agents.hermes]
            state = ["~/.hermes"]
            env = ["OPENROUTER_API_KEY"]
            "#,
        )
        .unwrap();
        config.validate().unwrap();
        assert_eq!(config.agents["hermes"].state, vec!["~/.hermes"]);
        assert_eq!(config.agents["hermes"].env, vec!["OPENROUTER_API_KEY"]);

        let leaky = Config::from_toml("[agents.a]\nenv = [\"TOKEN=abc\"]\n").unwrap();
        let err = leaky.validate().unwrap_err().to_string();
        assert!(err.contains("never values"), "{err}");
    }

    #[test]
    fn image_sources_are_exactly_one() {
        for (toml, field) in [
            ("[sandboxes.a]\nkind = \"arcbox\"\n", "image"),
            (
                "[sandboxes.a]\nkind = \"arcbox\"\nimage = \"x\"\ntemplate = \"y\"\n",
                "template",
            ),
            ("[sandboxes.k]\nkind = \"kubernetes\"\n", "warm_pool"),
        ] {
            let err = Config::from_toml(toml).unwrap().validate().unwrap_err();
            assert!(err.to_string().contains(field), "{err}");
        }
    }

    #[test]
    fn a_sandbox_cannot_take_a_built_in_name() {
        for name in ["auto", "ai-jail", "sandbox-exec"] {
            let config =
                Config::from_toml(&format!("[sandboxes.{name}]\nkind = \"native\"\n")).unwrap();
            assert!(config.validate().is_err(), "{name}");
        }
    }

    #[test]
    fn microsandbox_secrets_name_variables_and_hosts() {
        let config = Config::from_toml(
            "[sandboxes.vm]\nkind = \"microsandbox\"\nimage = \"node\"\n[sandboxes.vm.secrets]\nKEY = []\n",
        )
        .unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("hosts"), "{err}");
    }

    #[test]
    fn a_project_can_redefine_one_sandbox_and_keep_the_rest() {
        let mut global = Config::from_toml(
            "[sandboxes.a]\nkind = \"docker\"\nimage = \"one\"\n[sandboxes.b]\nkind = \"wasmer\"\n",
        )
        .unwrap();
        let project =
            Config::from_toml("[sandboxes.a]\nkind = \"docker\"\nimage = \"two\"\n").unwrap();
        global.merge(project);
        assert_eq!(global.sandboxes.len(), 2);
        let SandboxSettings::Docker(docker) = &global.sandboxes["a"] else {
            panic!("a is docker");
        };
        assert_eq!(docker.image, "two");
    }
}
