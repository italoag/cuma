//! Sandboxed execution.
//!
//! `security.sandbox` is on by default, but sandboxing is inherently
//! platform-specific: a machine may have `bwrap`, `firejail`, a container
//! runtime, macOS `sandbox-exec`, or nothing at all. Rather than depend on one,
//! CUMA *wraps* a command with whatever the operator configured or whatever is
//! detected on `PATH`.
//!
//! The honest part is what happens when nothing is available. The sandbox does
//! not silently become a no-op: [`Sandbox::status`] reports that it is
//! unavailable so `cuma doctor` can say so, rather than an operator believing
//! they are protected when they are not.

use cuma_config::SecurityConfig;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Sandbox runtimes CUMA knows how to drive, in preference order.
///
/// Ordered by isolation strength, then by how commonly they are installed.
const KNOWN_RUNTIMES: &[(&str, &str)] = &[
    // First: the only runtime here built to confine a coding agent that needs
    // its model API and its own credentials.
    ("ai-jail", "ai-jail"),
    ("bwrap", "bubblewrap"),
    ("firejail", "firejail"),
    ("sandbox-exec", "macOS sandbox-exec"),
];

/// What sandboxing is actually doing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SandboxStatus {
    /// Sandboxing is off by configuration.
    Disabled,
    /// A runtime was found and will be used.
    Active {
        /// The binary driving it.
        runtime: String,
    },
    /// Sandboxing is requested but nothing can provide it.
    ///
    /// Commands still run. The operator is told they are unprotected rather
    /// than left believing otherwise.
    Unavailable {
        /// What was looked for.
        looked_for: Vec<String>,
    },
}

impl SandboxStatus {
    /// Whether commands are actually being confined.
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Active { .. })
    }

    /// A line for `cuma doctor`.
    pub fn describe(&self) -> String {
        match self {
            Self::Disabled => "sandbox: disabled by configuration".to_owned(),
            Self::Active { runtime } => format!("sandbox: active via {runtime}"),
            Self::Unavailable { looked_for } => format!(
                "sandbox: REQUESTED BUT UNAVAILABLE (looked for {}); \
                 agents run unconfined",
                looked_for.join(", ")
            ),
        }
    }
}

/// Wraps commands in a sandbox when one is available.
#[derive(Debug, Clone)]
pub struct Sandbox {
    status: SandboxStatus,
}

impl Sandbox {
    /// Detect what sandboxing is available under `config`.
    pub fn detect(config: &SecurityConfig) -> Self {
        if !config.sandbox {
            return Self {
                status: SandboxStatus::Disabled,
            };
        }

        // An explicit command wins: the operator knows their machine.
        if let Some(configured) = &config.sandbox_command {
            let binary = configured.split_whitespace().next().unwrap_or(configured);

            if which::which(binary).is_ok() {
                return Self {
                    status: SandboxStatus::Active {
                        runtime: configured.clone(),
                    },
                };
            }

            tracing::warn!(
                command = configured,
                "security.sandbox_command is not on PATH"
            );
        }

        for (binary, name) in KNOWN_RUNTIMES {
            if which::which(binary).is_ok() {
                return Self {
                    status: SandboxStatus::Active {
                        runtime: (*name).to_owned(),
                    },
                };
            }
        }

        Self {
            status: SandboxStatus::Unavailable {
                looked_for: KNOWN_RUNTIMES
                    .iter()
                    .map(|(b, _)| (*b).to_owned())
                    .collect(),
            },
        }
    }

    /// A sandbox that confines nothing, for tests.
    pub fn disabled() -> Self {
        Self {
            status: SandboxStatus::Disabled,
        }
    }

    /// What sandboxing is doing.
    pub fn status(&self) -> &SandboxStatus {
        &self.status
    }

    /// Wrap `command` so it runs confined to `workspace`.
    ///
    /// Returns the command unchanged when no sandbox is active — the caller
    /// runs the same string either way, and learns whether it was confined
    /// from [`Sandbox::status`] rather than by inspecting the result.
    pub fn wrap(&self, command: &str, workspace: &Path) -> String {
        let SandboxStatus::Active { runtime } = &self.status else {
            return command.to_owned();
        };

        let workspace = workspace.display();

        match runtime.as_str() {
            // `--exec`: no PTY proxy or status bar between the command and
            // its caller. Network stays off, which is ai-jail's default.
            "ai-jail" => format!("ai-jail --exec -- {command}"),
            "bubblewrap" => format!(
                // Read-only system, writable workspace, no network, no new
                // privileges. `--die-with-parent` stops an orphaned agent
                // outliving the session.
                "bwrap --ro-bind /usr /usr --ro-bind /lib /lib --ro-bind /lib64 /lib64 \
                 --ro-bind /bin /bin --ro-bind /etc /etc \
                 --bind {workspace} {workspace} --chdir {workspace} \
                 --proc /proc --dev /dev --unshare-net --unshare-pid \
                 --die-with-parent -- {command}"
            ),
            "firejail" => format!(
                "firejail --quiet --private={workspace} --net=none --nosound \
                 --no3d --nodvd --notv -- {command}"
            ),
            "macOS sandbox-exec" => {
                format!("sandbox-exec -p '(version 1)(allow default)(deny network*)' {command}")
            }
            // A configured command is used as a prefix verbatim; the operator
            // chose its flags and CUMA should not second-guess them.
            other => format!("{other} {command}"),
        }
    }
}

/// How a sandboxed agent may reach the outside world.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentConfinement {
    /// Hosts the agent may reach. Empty means unrestricted network.
    pub allowed_hosts: Vec<String>,
    /// Environment variables forwarded into the sandbox.
    pub forwarded_env: Vec<String>,
}

impl AgentConfinement {
    /// Confinement as the security configuration describes it.
    pub fn from_config(config: &SecurityConfig) -> Self {
        Self {
            allowed_hosts: config.network_allowlist.clone(),
            forwarded_env: config.agent_env.clone(),
        }
    }
}

impl Sandbox {
    /// The prefix that launches a coding agent inside the sandbox, or `None`
    /// when this sandbox cannot confine one.
    ///
    /// A coding agent is not an arbitrary command: it needs its model API and
    /// its own credentials, and speaks JSON-RPC over stdio, so a profile that
    /// cuts the network or interposes a terminal breaks it outright. Only
    /// ai-jail is built for that — `--exec` for a clean stdio channel,
    /// `--agent-state` for the agent's own login, `--allow-host` for filtered
    /// egress — so only ai-jail is used here, and every other runtime reports
    /// agents as unconfined rather than pretending.
    pub fn agent_launch_prefix(
        &self,
        workspace: &Path,
        confinement: &AgentConfinement,
    ) -> Option<Vec<String>> {
        let SandboxStatus::Active { runtime } = &self.status else {
            return None;
        };
        if runtime != "ai-jail" {
            return None;
        }

        let mut prefix = vec![
            "ai-jail".to_owned(),
            "--exec".to_owned(),
            "--agent-state".to_owned(),
        ];

        if confinement.allowed_hosts.is_empty() {
            prefix.push("--network".to_owned());
        } else {
            for host in &confinement.allowed_hosts {
                prefix.push("--allow-host".to_owned());
                prefix.push(host.clone());
            }
        }

        for name in &confinement.forwarded_env {
            prefix.push("--env".to_owned());
            prefix.push(name.clone());
        }

        // ai-jail makes its working directory writable. The agent is launched
        // from CUMA's, so a workspace elsewhere is mapped explicitly.
        let here = std::env::current_dir().ok();
        if here.as_deref() != Some(workspace) {
            prefix.push("--rw-map".to_owned());
            prefix.push(workspace.display().to_string());
        }

        prefix.push("--".to_owned());
        Some(prefix)
    }

    /// One line on whether agents themselves run confined.
    pub fn describe_agent_confinement(&self) -> String {
        match &self.status {
            SandboxStatus::Active { runtime } if runtime == "ai-jail" => {
                "agents: confined by ai-jail".to_owned()
            }
            SandboxStatus::Active { runtime } => format!(
                "agents: UNCONFINED — {runtime} cannot confine a networked agent; \
                 install ai-jail to sandbox agents themselves"
            ),
            SandboxStatus::Disabled => "agents: unconfined (sandbox disabled)".to_owned(),
            SandboxStatus::Unavailable { .. } => {
                "agents: UNCONFINED — no sandbox runtime is available".to_owned()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use std::path::PathBuf;

    fn jail() -> Sandbox {
        Sandbox {
            status: SandboxStatus::Active {
                runtime: "ai-jail".to_owned(),
            },
        }
    }

    #[test]
    fn an_agent_under_ai_jail_keeps_its_login_and_a_clean_stdio_channel() {
        let prefix = jail()
            .agent_launch_prefix(Path::new("/work"), &AgentConfinement::default())
            .unwrap();
        assert_eq!(&prefix[..3], ["ai-jail", "--exec", "--agent-state"]);
        assert!(
            prefix.contains(&"--network".to_owned()),
            "no allowlist: open network"
        );
        assert_eq!(prefix.last().map(String::as_str), Some("--"));
    }

    #[test]
    fn a_network_allowlist_becomes_filtered_egress() {
        let prefix = jail()
            .agent_launch_prefix(
                Path::new("/work"),
                &AgentConfinement {
                    allowed_hosts: vec!["api.anthropic.com".into(), "registry.npmjs.org".into()],
                    forwarded_env: vec!["ANTHROPIC_API_KEY".into()],
                },
            )
            .unwrap();
        assert!(!prefix.contains(&"--network".to_owned()));
        let joined = prefix.join(" ");
        assert!(joined.contains("--allow-host api.anthropic.com --allow-host registry.npmjs.org"));
        assert!(joined.contains("--env ANTHROPIC_API_KEY"));
    }

    #[test]
    fn a_workspace_other_than_the_working_directory_is_mapped() {
        let prefix = jail()
            .agent_launch_prefix(
                Path::new("/elsewhere/project"),
                &AgentConfinement::default(),
            )
            .unwrap();
        let joined = prefix.join(" ");
        assert!(joined.contains("--rw-map /elsewhere/project"), "{joined}");
    }

    #[test]
    fn other_runtimes_do_not_pretend_to_confine_agents() {
        let bwrap = Sandbox {
            status: SandboxStatus::Active {
                runtime: "bubblewrap".to_owned(),
            },
        };
        assert!(
            bwrap
                .agent_launch_prefix(Path::new("/w"), &AgentConfinement::default())
                .is_none()
        );
        assert!(bwrap.describe_agent_confinement().contains("UNCONFINED"));
        assert!(
            Sandbox::disabled()
                .agent_launch_prefix(Path::new("/w"), &AgentConfinement::default())
                .is_none()
        );
    }

    fn workspace() -> PathBuf {
        PathBuf::from("/projects/app")
    }

    #[test]
    fn sandboxing_off_leaves_commands_untouched() {
        let sandbox = Sandbox::detect(&SecurityConfig {
            sandbox: false,
            ..SecurityConfig::default()
        });

        assert_eq!(sandbox.status(), &SandboxStatus::Disabled);
        assert_eq!(sandbox.wrap("cargo test", &workspace()), "cargo test");
    }

    #[test]
    fn an_unavailable_sandbox_says_so_rather_than_pretending() {
        // The failure mode this guards against: an operator reading
        // `sandbox = true` in their config and believing they are protected.
        let status = SandboxStatus::Unavailable {
            looked_for: vec!["bwrap".into()],
        };

        assert!(!status.is_active());
        assert!(status.describe().contains("UNAVAILABLE"));
        assert!(status.describe().contains("unconfined"));
    }

    #[test]
    fn an_unavailable_sandbox_still_runs_the_command() {
        let sandbox = Sandbox {
            status: SandboxStatus::Unavailable {
                looked_for: vec!["bwrap".into()],
            },
        };

        // Refusing to run anything would make an unsandboxable machine
        // unusable; the operator is warned instead.
        assert_eq!(sandbox.wrap("cargo test", &workspace()), "cargo test");
    }

    #[test]
    fn bubblewrap_confines_to_the_workspace_and_removes_the_network() {
        let sandbox = Sandbox {
            status: SandboxStatus::Active {
                runtime: "bubblewrap".into(),
            },
        };

        let wrapped = sandbox.wrap("cargo test", &workspace());

        assert!(wrapped.starts_with("bwrap "));
        assert!(wrapped.contains("--bind /projects/app /projects/app"));
        assert!(wrapped.contains("--unshare-net"));
        assert!(
            wrapped.contains("--die-with-parent"),
            "an orphan must not outlive the session"
        );
        assert!(wrapped.ends_with("cargo test"));
    }

    #[test]
    fn firejail_confines_to_the_workspace() {
        let sandbox = Sandbox {
            status: SandboxStatus::Active {
                runtime: "firejail".into(),
            },
        };

        let wrapped = sandbox.wrap("cargo test", &workspace());
        assert!(wrapped.contains("--private=/projects/app"));
        assert!(wrapped.contains("--net=none"));
    }

    #[test]
    fn a_configured_runtime_is_used_as_a_prefix_verbatim() {
        // The operator chose the flags; CUMA should not second-guess them.
        let sandbox = Sandbox {
            status: SandboxStatus::Active {
                runtime: "my-jail --strict".into(),
            },
        };

        assert_eq!(
            sandbox.wrap("cargo test", &workspace()),
            "my-jail --strict cargo test"
        );
    }

    #[test]
    fn a_configured_command_that_is_not_installed_falls_back_to_detection() {
        let sandbox = Sandbox::detect(&SecurityConfig {
            sandbox: true,
            sandbox_command: Some("definitely-not-a-sandbox-9f3a".into()),
            ..SecurityConfig::default()
        });

        assert!(
            !matches!(
                sandbox.status(),
                SandboxStatus::Active { runtime } if runtime.contains("9f3a")
            ),
            "a missing sandbox must not be reported as active"
        );
    }

    #[test]
    fn detection_reports_something_actionable_on_every_machine() {
        // Whatever this machine has, the status must be one of the three
        // states and must describe itself.
        let sandbox = Sandbox::detect(&SecurityConfig::default());
        assert!(!sandbox.status().describe().is_empty());
    }
}
