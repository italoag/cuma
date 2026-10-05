//! microsandbox: local libkrun microVMs from OCI images, driven by `msb`.
//!
//! `msb create` boots a named VM with the workspace mounted at its own path,
//! the network rules and the secrets; the agent runs through
//! `msb exec --stream`, which microsandbox documents for ACP agents (input
//! forwarded and output flushed as it arrives, without a PTY); `msb rm
//! --force` removes the VM when the launch ends.
//!
//! A secret is declared as `NAME@host,…`: `msb` reads the value from CUMA's
//! environment, the guest only ever sees a placeholder, and the real value is
//! substituted at the egress for the listed hosts.

use crate::process::{Input, Output, run};
use crate::{
    Capabilities, Isolation, LaunchRequest, SandboxLaunch, SandboxProvider, SandboxSession,
    WorkspaceAccess, guest_readable, session_id, writable_mounts,
};
use async_trait::async_trait;
use cuma_config::sandbox::MicrosandboxSandbox;
use cuma_core::error::Result;
use std::sync::Arc;
use std::time::Duration;

/// Booting may pull the image first.
const BOOT: Duration = Duration::from_secs(600);

/// `kind = "microsandbox"`.
pub struct MicrosandboxProvider {
    name: String,
    settings: MicrosandboxSandbox,
}

impl MicrosandboxProvider {
    /// A provider for `[sandboxes.<name>]`.
    pub fn new(name: impl Into<String>, settings: MicrosandboxSandbox) -> Self {
        Self {
            name: name.into(),
            settings,
        }
    }

    /// The `msb create` arguments for one launch's VM.
    pub fn create_args(&self, id: &str, request: &LaunchRequest) -> Vec<String> {
        let s = &self.settings;
        let workspace = crate::canonical(&request.workspace);
        let mut args: Vec<String> = ["create", &s.image, "--name", id, "-w", &workspace]
            .map(str::to_owned)
            .to_vec();
        if let Some(cpus) = s.cpus {
            args.extend(["-c".to_owned(), cpus.to_string()]);
        }
        if let Some(memory) = &s.memory {
            args.extend(["-m".to_owned(), memory.clone()]);
        }
        for (host, guest) in writable_mounts(request, &s.home) {
            args.extend(["-v".to_owned(), format!("{host}:{guest}")]);
        }
        for path in guest_readable(request) {
            args.extend(["-v".to_owned(), format!("{path}:{path}:ro")]);
        }
        for (variable, hosts) in &s.secrets {
            args.extend([
                "--secret".to_owned(),
                format!("{variable}@{}", hosts.join(",")),
            ]);
        }
        // Values the operator wrote in the configuration, not secrets.
        for (variable, value) in &s.env {
            args.extend(["-e".to_owned(), format!("{variable}={value}")]);
        }
        if !request.allowed_hosts.is_empty() {
            let rules: Vec<String> = request
                .allowed_hosts
                .iter()
                .map(|host| format!("allow@{host}"))
                .collect();
            args.extend([
                "--net-default".to_owned(),
                "deny".to_owned(),
                "--net-rule".to_owned(),
                rules.join(","),
            ]);
        }
        args
    }
}

#[async_trait]
impl SandboxProvider for MicrosandboxProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> &'static str {
        "microsandbox"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            isolation: Isolation::MicroVm,
            workspace: WorkspaceAccess::Mounted,
            network_allowlist: true,
            secrets_outside: true,
        }
    }

    fn program(&self) -> String {
        self.settings.program.clone()
    }

    async fn probe(&self) -> Result<()> {
        let args: Vec<String> = ["run", "--no-stdin", &self.settings.image, "--", "true"]
            .map(str::to_owned)
            .to_vec();
        run(
            &self.name,
            &self.settings.program,
            &args,
            Input::Nothing,
            Output::Capture,
            BOOT,
        )
        .await
        .map(|_| ())
    }

    async fn open(&self, request: &LaunchRequest) -> Result<SandboxLaunch> {
        let id = session_id();
        let program = &self.settings.program;
        run(
            &self.name,
            program,
            &self.create_args(&id, request),
            Input::Nothing,
            Output::Capture,
            BOOT,
        )
        .await?;
        let workspace = crate::canonical(&request.workspace);
        let prefix: Vec<String> = [
            program.as_str(),
            "exec",
            "--stream",
            "-w",
            &workspace,
            &id,
            "--",
        ]
        .map(str::to_owned)
        .to_vec();
        Ok(SandboxLaunch::with_session(
            prefix,
            Arc::new(Vm {
                sandbox: self.name.clone(),
                program: program.clone(),
                id,
            }),
        ))
    }
}

/// One launch's VM.
struct Vm {
    sandbox: String,
    program: String,
    id: String,
}

impl Vm {
    async fn remove(&self) -> Result<()> {
        let args: Vec<String> = ["rm", "--force", &self.id].map(str::to_owned).to_vec();
        run(
            &self.sandbox,
            &self.program,
            &args,
            Input::Nothing,
            Output::Capture,
            Duration::from_secs(120),
        )
        .await
        .map(|_| ())
    }
}

#[async_trait]
impl SandboxSession for Vm {
    async fn finish(&self) -> Result<()> {
        self.remove().await
    }

    async fn abort(&self) {
        if let Err(err) = self.remove().await {
            tracing::warn!(vm = %self.id, error = %err, "removing the microVM failed");
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use cuma_core::ports::LaunchPurpose;
    use std::collections::BTreeMap;

    fn provider() -> MicrosandboxProvider {
        MicrosandboxProvider::new(
            "vm",
            MicrosandboxSandbox {
                image: "node:22".into(),
                cpus: Some(2),
                memory: Some("2G".into()),
                secrets: BTreeMap::from([(
                    "ANTHROPIC_API_KEY".to_owned(),
                    vec!["api.anthropic.com".to_owned()],
                )]),
                ..MicrosandboxSandbox::default()
            },
        )
    }

    #[test]
    fn the_vm_mounts_the_workspace_and_declares_secrets_by_name_only() {
        let ws = tempfile::tempdir().unwrap();
        let path = crate::canonical(ws.path());
        let args = provider().create_args(
            "cuma-1",
            &LaunchRequest::bare(ws.path(), LaunchPurpose::Execute),
        );
        assert_eq!(&args[..4], ["create", "node:22", "--name", "cuma-1"]);
        assert!(
            args.windows(2)
                .any(|w| w == ["-v", &format!("{path}:{path}")]),
            "{args:?}"
        );
        assert!(args.windows(2).any(|w| w == ["-w", &path]));
        assert!(args.windows(2).any(|w| w == ["-c", "2"]));
        assert!(
            args.windows(2)
                .any(|w| w == ["--secret", "ANTHROPIC_API_KEY@api.anthropic.com"])
        );
    }

    #[test]
    fn an_allowlist_denies_everything_else() {
        let ws = tempfile::tempdir().unwrap();
        let mut request = LaunchRequest::bare(ws.path(), LaunchPurpose::Execute);
        request.allowed_hosts = vec!["api.anthropic.com".into(), "*.githubusercontent.com".into()];
        let args = provider().create_args("cuma-1", &request);
        let net = args.iter().position(|a| a == "--net-default").unwrap();
        assert_eq!(
            &args[net..net + 4],
            [
                "--net-default",
                "deny",
                "--net-rule",
                "allow@api.anthropic.com,allow@*.githubusercontent.com"
            ]
        );

        let open = provider().create_args(
            "cuma-1",
            &LaunchRequest::bare(ws.path(), LaunchPurpose::Execute),
        );
        assert!(
            !open.iter().any(|a| a == "--net-default"),
            "no allowlist, no rules"
        );
    }

    #[tokio::test]
    async fn the_agent_runs_through_a_streaming_exec_and_the_vm_is_removed_after() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("calls");
        // A stand-in for msb that records how it was called.
        let fake = dir.path().join("msb");
        std::fs::write(
            &fake,
            format!("#!/bin/sh\necho \"$@\" >> {}\n", log.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let p = MicrosandboxProvider::new(
            "vm",
            MicrosandboxSandbox {
                image: "node:22".into(),
                program: fake.display().to_string(),
                ..MicrosandboxSandbox::default()
            },
        );
        let ws = tempfile::tempdir().unwrap();
        let launch = p
            .open(&LaunchRequest::bare(ws.path(), LaunchPurpose::Execute))
            .await
            .unwrap();
        let prefix = launch.prefix().to_vec();
        assert_eq!(&prefix[1..3], ["exec", "--stream"]);
        assert_eq!(prefix.last().unwrap(), "--");
        let id = prefix[prefix.len() - 2].clone();
        launch.finish().await.unwrap();

        let calls = std::fs::read_to_string(log).unwrap();
        let lines: Vec<&str> = calls.lines().collect();
        assert!(
            lines[0].starts_with(&format!("create node:22 --name {id}")),
            "{calls}"
        );
        assert_eq!(lines[1], format!("rm --force {id}"));
    }
}
