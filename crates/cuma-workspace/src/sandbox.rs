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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use std::path::PathBuf;

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
