//! A configured prefix: any sandbox with a wrapper CLI that passes stdio
//! through, without writing code.
//!
//! `{workspace}`, `{id}` (a fresh name per launch) and `{home}` are
//! substituted in `prefix`, `probe`, `setup` and `teardown`. CUMA cannot tell
//! what such a sandbox enforces, so it is never reported as enforcing a
//! network allowlist.

use crate::process::{Input, Output, run};
use crate::{
    Capabilities, Isolation, LaunchRequest, SandboxLaunch, SandboxProvider, SandboxSession,
    WorkspaceAccess, failure, session_id,
};
use async_trait::async_trait;
use cuma_config::sandbox::CommandSandbox;
use cuma_core::error::Result;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// How long a setup, teardown or probe command may take.
const STEP: Duration = Duration::from_secs(300);

/// `kind = "command"`.
pub struct CommandProvider {
    name: String,
    settings: CommandSandbox,
}

impl CommandProvider {
    /// A provider for `[sandboxes.<name>]`.
    pub fn new(name: impl Into<String>, settings: CommandSandbox) -> Self {
        Self {
            name: name.into(),
            settings,
        }
    }
}

/// `words` with the placeholders filled in.
fn render(words: &[String], id: &str, workspace: &Path) -> Vec<String> {
    let home = dirs::home_dir()
        .map(|h| h.display().to_string())
        .unwrap_or_default();
    let workspace = crate::canonical(workspace);
    words
        .iter()
        .map(|word| {
            word.replace("{workspace}", &workspace)
                .replace("{id}", id)
                .replace("{home}", &home)
        })
        .collect()
}

async fn step(sandbox: &str, words: &[String]) -> Result<()> {
    let Some((program, args)) = words.split_first() else {
        return Ok(());
    };
    run(
        sandbox,
        program,
        args,
        Input::Nothing,
        Output::Capture,
        STEP,
    )
    .await
    .map(|_| ())
}

#[async_trait]
impl SandboxProvider for CommandProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> &'static str {
        "command"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            isolation: Isolation::from_name(&self.settings.isolation),
            workspace: WorkspaceAccess::Mounted,
            network_allowlist: false,
            secrets_outside: false,
        }
    }

    fn program(&self) -> String {
        self.settings.prefix.first().cloned().unwrap_or_default()
    }

    fn prefix_hint(&self, request: &LaunchRequest) -> Vec<String> {
        render(&self.settings.prefix, "{id}", &request.workspace)
    }

    async fn probe(&self) -> Result<()> {
        let here = std::env::current_dir().unwrap_or_default();
        let words = if self.settings.probe.is_empty() {
            let mut words = self.settings.prefix.clone();
            words.push("true".to_owned());
            words
        } else {
            self.settings.probe.clone()
        };
        step(&self.name, &render(&words, "cuma-probe", &here)).await
    }

    async fn open(&self, request: &LaunchRequest) -> Result<SandboxLaunch> {
        let id = session_id();
        step(
            &self.name,
            &render(&self.settings.setup, &id, &request.workspace),
        )
        .await
        .map_err(|err| failure(&self.name, format!("setup failed: {err}")))?;
        let prefix = render(&self.settings.prefix, &id, &request.workspace);
        if self.settings.teardown.is_empty() {
            return Ok(SandboxLaunch::new(prefix));
        }
        let teardown = render(&self.settings.teardown, &id, &request.workspace);
        Ok(SandboxLaunch::with_session(
            prefix,
            Arc::new(Teardown {
                sandbox: self.name.clone(),
                words: teardown,
            }),
        ))
    }
}

/// The configured teardown, run once a launch ends.
struct Teardown {
    sandbox: String,
    words: Vec<String>,
}

#[async_trait]
impl SandboxSession for Teardown {
    async fn finish(&self) -> Result<()> {
        step(&self.sandbox, &self.words).await
    }

    async fn abort(&self) {
        if let Err(err) = step(&self.sandbox, &self.words).await {
            tracing::warn!(sandbox = %self.sandbox, error = %err, "teardown failed");
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use cuma_core::ports::LaunchPurpose;

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|w| (*w).to_owned()).collect()
    }

    #[tokio::test]
    async fn placeholders_are_filled_and_setup_and_teardown_run_around_the_launch() {
        let ws = tempfile::tempdir().unwrap();
        let log = ws.path().join("log");
        let log_text = log.display().to_string();
        let provider = CommandProvider::new(
            "custom",
            CommandSandbox {
                prefix: words(&[
                    "my-sandbox",
                    "--mount",
                    "{workspace}:{workspace}",
                    "--name",
                    "{id}",
                    "--",
                ]),
                setup: words(&["sh", "-c", &format!("echo setup {{id}} >> {log_text}")]),
                teardown: words(&["sh", "-c", &format!("echo teardown {{id}} >> {log_text}")]),
                ..CommandSandbox::default()
            },
        );
        let launch = provider
            .open(&LaunchRequest::bare(ws.path(), LaunchPurpose::Execute))
            .await
            .unwrap();
        let path = crate::canonical(ws.path());
        assert_eq!(launch.prefix()[2], format!("{path}:{path}"));
        let id = launch.prefix()[4].clone();
        assert!(id.starts_with("cuma-"));
        launch.finish().await.unwrap();

        let log = std::fs::read_to_string(log).unwrap();
        assert_eq!(log, format!("setup {id}\nteardown {id}\n"));
    }

    #[tokio::test]
    async fn the_default_probe_runs_true_under_the_prefix() {
        let works = CommandProvider::new(
            "env",
            CommandSandbox {
                prefix: words(&["env", "--"]),
                ..CommandSandbox::default()
            },
        );
        works.probe().await.unwrap();
        let broken = CommandProvider::new(
            "broken",
            CommandSandbox {
                prefix: words(&["false"]),
                ..CommandSandbox::default()
            },
        );
        assert!(broken.probe().await.is_err());
        assert!(!works.capabilities().network_allowlist, "never claimed");
    }

    #[tokio::test]
    async fn a_failed_setup_fails_the_launch() {
        let ws = tempfile::tempdir().unwrap();
        let provider = CommandProvider::new(
            "custom",
            CommandSandbox {
                prefix: words(&["env"]),
                setup: words(&["false"]),
                ..CommandSandbox::default()
            },
        );
        let err = provider
            .open(&LaunchRequest::bare(ws.path(), LaunchPurpose::Execute))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("setup failed"), "{err}");
    }
}
