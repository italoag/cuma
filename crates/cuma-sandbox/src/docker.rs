//! Container engines that speak Docker's CLI: Docker, Podman, nerdctl, and
//! the engines of ArcBox, OrbStack, Rancher Desktop and Colima through a
//! context. gVisor or Kata Containers (including Kata on Firecracker) through
//! `runtime`.
//!
//! Each launch is `docker run --rm -i --init` under a fresh container name,
//! with the workspace mounted at its own path. The container is removed by
//! name when the launch ends: the agent's client is killed after its turn,
//! and killing a `docker run` client does not stop its container.

use crate::process::{Input, Output, run};
use crate::{
    Capabilities, Isolation, LaunchRequest, SandboxLaunch, SandboxProvider, SandboxSession,
    WorkspaceAccess, forwarded_env, guest_readable, session_id, writable_mounts,
};
use async_trait::async_trait;
use cuma_config::sandbox::DockerSandbox;
use cuma_core::error::Result;
use std::sync::Arc;
use std::time::Duration;

/// `kind = "docker"`.
pub struct DockerProvider {
    name: String,
    settings: DockerSandbox,
}

impl DockerProvider {
    /// A provider for `[sandboxes.<name>]`.
    pub fn new(name: impl Into<String>, settings: DockerSandbox) -> Self {
        Self {
            name: name.into(),
            settings,
        }
    }

    /// Global flags: the context, which must come before the subcommand.
    fn global(&self) -> Vec<String> {
        self.settings
            .context
            .iter()
            .flat_map(|context| ["--context".to_owned(), context.clone()])
            .collect()
    }

    /// The `docker run` words for one launch, up to and including the image.
    pub fn run_args(&self, id: &str, request: &LaunchRequest) -> Vec<String> {
        let s = &self.settings;
        let workspace = crate::canonical(&request.workspace);
        let mut args = self.global();
        args.extend(
            [
                "run",
                "--rm",
                "-i",
                "--init",
                "--name",
                id,
                "--label",
                &format!("dev.cuma.sandbox={}", self.name),
                "--security-opt",
                "no-new-privileges",
                "--cap-drop",
                "ALL",
                "--network",
                &s.network,
                "-w",
                &workspace,
            ]
            .map(str::to_owned),
        );
        for (flag, value) in [
            ("--runtime", &s.runtime),
            ("--user", &s.user),
            ("--memory", &s.memory),
            ("--cpus", &s.cpus),
            ("--entrypoint", &s.entrypoint),
        ] {
            if let Some(value) = value {
                args.extend([flag.to_owned(), value.clone()]);
            }
        }
        for (host, guest) in writable_mounts(request, &s.home) {
            args.extend(["-v".to_owned(), format!("{host}:{guest}")]);
        }
        for path in guest_readable(request) {
            args.extend(["-v".to_owned(), format!("{path}:{path}:ro")]);
        }
        // By name: the engine copies each value from CUMA's environment.
        for name in forwarded_env(request) {
            args.extend(["-e".to_owned(), name]);
        }
        args.extend(s.extra_args.iter().cloned());
        args.push(s.image.clone());
        args
    }
}

#[async_trait]
impl SandboxProvider for DockerProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> &'static str {
        "docker"
    }

    fn capabilities(&self) -> Capabilities {
        let runtime = self.settings.runtime.as_deref().unwrap_or_default();
        Capabilities {
            // Kata runs each container in a virtual machine.
            isolation: if runtime.contains("kata") || runtime.contains("firecracker") {
                Isolation::MicroVm
            } else {
                Isolation::Container
            },
            workspace: WorkspaceAccess::Mounted,
            network_allowlist: false,
            secrets_outside: false,
        }
    }

    fn program(&self) -> String {
        self.settings.program.clone()
    }

    async fn probe(&self) -> Result<()> {
        let mut args = self.global();
        args.extend(["run", "--rm", "--network", "none"].map(str::to_owned));
        if let Some(runtime) = &self.settings.runtime {
            args.extend(["--runtime".to_owned(), runtime.clone()]);
        }
        if let Some(entrypoint) = &self.settings.entrypoint {
            args.extend(["--entrypoint".to_owned(), entrypoint.clone()]);
        }
        args.extend([self.settings.image.clone(), "true".to_owned()]);
        // A first run may pull the image.
        run(
            &self.name,
            &self.settings.program,
            &args,
            Input::Nothing,
            Output::Capture,
            Duration::from_secs(600),
        )
        .await
        .map(|_| ())
    }

    async fn open(&self, request: &LaunchRequest) -> Result<SandboxLaunch> {
        let id = session_id();
        let mut prefix = vec![self.settings.program.clone()];
        prefix.extend(self.run_args(&id, request));
        let session = Arc::new(Container {
            sandbox: self.name.clone(),
            program: self.settings.program.clone(),
            global: self.global(),
            id,
        });
        Ok(SandboxLaunch::with_session(prefix, session))
    }
}

/// One launch's container.
struct Container {
    sandbox: String,
    program: String,
    global: Vec<String>,
    id: String,
}

impl Container {
    async fn remove(&self) {
        let mut args = self.global.clone();
        args.extend(["rm".to_owned(), "-f".to_owned(), self.id.clone()]);
        // Already gone (`--rm`) is the usual case, not an error.
        if let Err(err) = run(
            &self.sandbox,
            &self.program,
            &args,
            Input::Nothing,
            Output::Capture,
            Duration::from_secs(60),
        )
        .await
            && !err.to_string().contains("No such container")
        {
            tracing::warn!(container = %self.id, error = %err, "removing the container failed");
        }
    }
}

#[async_trait]
impl SandboxSession for Container {
    async fn finish(&self) -> Result<()> {
        self.remove().await;
        Ok(())
    }

    async fn abort(&self) {
        self.remove().await;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use cuma_core::ports::LaunchPurpose;

    fn provider(settings: DockerSandbox) -> DockerProvider {
        DockerProvider::new("box", settings)
    }

    fn settings() -> DockerSandbox {
        DockerSandbox {
            image: "node:22".into(),
            ..DockerSandbox::default()
        }
    }

    #[test]
    fn the_workspace_is_mounted_at_its_own_path_and_the_image_comes_last() {
        let ws = tempfile::tempdir().unwrap();
        let path = crate::canonical(ws.path());
        let args = provider(settings()).run_args(
            "cuma-1",
            &LaunchRequest::bare(ws.path(), LaunchPurpose::Execute),
        );

        assert_eq!(args.last().unwrap(), "node:22");
        assert!(
            args.windows(2)
                .any(|w| w == ["-v", &format!("{path}:{path}")]),
            "{args:?}"
        );
        assert!(args.windows(2).any(|w| w == ["-w", &path]));
        for hardening in [
            ["--cap-drop", "ALL"],
            ["--security-opt", "no-new-privileges"],
        ] {
            assert!(args.windows(2).any(|w| w == hardening), "{hardening:?}");
        }
        assert!(args.windows(2).any(|w| w == ["--name", "cuma-1"]));
    }

    #[test]
    fn variables_are_forwarded_by_name_and_never_by_value() {
        let ws = tempfile::tempdir().unwrap();
        let mut request = LaunchRequest::bare(ws.path(), LaunchPurpose::Execute);
        // PATH is always set; ask for it explicitly.
        request.keep_env = vec!["PATH".into()];
        let args = provider(settings()).run_args("cuma-1", &request);
        assert!(args.windows(2).any(|w| w == ["-e", "PATH"]), "{args:?}");
        let path = std::env::var("PATH").unwrap();
        assert!(
            args.iter().all(|a| !a.contains(&path)),
            "no value on the command line"
        );
    }

    #[test]
    fn state_under_home_is_mounted_under_the_guest_home() {
        let home = dirs::home_dir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let mut request = LaunchRequest::bare(ws.path(), LaunchPurpose::Execute);
        // The home directory itself exists everywhere; use a real child of it.
        let state = std::fs::read_dir(&home)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| p.is_dir())
            .unwrap();
        request.state = vec![state.clone()];
        let args = provider(settings()).run_args("cuma-1", &request);
        let relative = state.strip_prefix(&home).unwrap().display().to_string();
        let expected = format!("{}:/root/{relative}", crate::canonical(&state));
        assert!(args.iter().any(|a| a == &expected), "{args:?}");
    }

    #[test]
    fn a_context_and_a_runtime_are_honoured() {
        let ws = tempfile::tempdir().unwrap();
        let p = provider(DockerSandbox {
            context: Some("arcbox".into()),
            runtime: Some("kata-fc".into()),
            ..settings()
        });
        let args = p.run_args(
            "cuma-1",
            &LaunchRequest::bare(ws.path(), LaunchPurpose::Execute),
        );
        assert_eq!(&args[..3], ["--context", "arcbox", "run"]);
        assert!(args.windows(2).any(|w| w == ["--runtime", "kata-fc"]));
        assert_eq!(p.capabilities().isolation, Isolation::MicroVm);
    }

    #[tokio::test]
    async fn each_launch_has_its_own_container_and_a_session_to_remove_it() {
        let ws = tempfile::tempdir().unwrap();
        let p = provider(DockerSandbox {
            // A program that accepts anything, so removal "succeeds".
            program: "true".into(),
            ..settings()
        });
        let request = LaunchRequest::bare(ws.path(), LaunchPurpose::Execute);
        let a = p.open(&request).await.unwrap();
        let b = p.open(&request).await.unwrap();
        let name = |l: &SandboxLaunch| {
            let i = l.prefix().iter().position(|w| w == "--name").unwrap();
            l.prefix()[i + 1].clone()
        };
        assert_ne!(name(&a), name(&b));
        assert!(a.has_session());
        a.finish().await.unwrap();
        b.finish().await.unwrap();
    }
}
