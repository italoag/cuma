//! ArcBox sandboxes: Firecracker microVMs nested in ArcBox's VM on macOS,
//! driven by `abctl sandbox`.
//!
//! Sandbox V1 rejects mounts, so the workspace is copied: archived, copied in
//! with `abctl sandbox cp`, unpacked with `abctl sandbox run`. The agent runs
//! through `abctl sandbox exec` behind a small `sh` wrapper that loads the
//! environment file and enters the workspace. Afterwards the workspace is
//! archived and copied out for the merge, and the sandbox removed.
//!
//! An execution survives its client in ArcBox, and a sandbox runs one at a
//! time, so archiving the result waits for the agent's execution to end.
//! Transfers are limited to 256 MiB per file.

use crate::process::{Input, Output, run};
use crate::sync::Snapshot;
use crate::{
    Capabilities, ENTER, Isolation, LaunchRequest, SandboxLaunch, SandboxProvider, SandboxSession,
    WorkspaceAccess, env_file, failure, forwarded_env, session_id,
};
use async_trait::async_trait;
use cuma_config::sandbox::ArcboxSandbox;
use cuma_core::error::Result;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Creating may build the image first.
const CREATE: Duration = Duration::from_secs(900);
/// Copying, unpacking, archiving.
const STEP: Duration = Duration::from_secs(600);

/// `kind = "arcbox"`.
pub struct ArcboxProvider {
    name: String,
    settings: ArcboxSandbox,
}

impl ArcboxProvider {
    /// A provider for `[sandboxes.<name>]`.
    pub fn new(name: impl Into<String>, settings: ArcboxSandbox) -> Self {
        Self {
            name: name.into(),
            settings,
        }
    }

    /// The `abctl sandbox create` arguments.
    pub fn create_args(&self, id: &str, ttl_secs: u64) -> Vec<String> {
        let s = &self.settings;
        let mut args: Vec<String> = ["sandbox", "create", "--id", id]
            .map(str::to_owned)
            .to_vec();
        for (flag, value) in [
            ("--from-image", &s.image),
            ("--template", &s.template),
            ("--from-dockerfile", &s.dockerfile),
        ] {
            if let Some(value) = value {
                args.extend([flag.to_owned(), value.clone()]);
            }
        }
        if let Some(cpus) = s.cpus {
            args.extend(["--cpus".to_owned(), cpus.to_string()]);
        }
        if let Some(memory) = s.memory_mib {
            args.extend(["--memory".to_owned(), memory.to_string()]);
        }
        args.extend(["--ttl".to_owned(), ttl_secs.to_string()]);
        args
    }

    /// The prefix the agent runs under in sandbox `id`.
    pub fn exec_prefix(&self, id: &str, workspace: &str) -> Vec<String> {
        let mut prefix: Vec<String> = [self.settings.program.as_str(), "sandbox", "exec"]
            .map(str::to_owned)
            .to_vec();
        if let Some(user) = &self.settings.user {
            prefix.extend(["--user".to_owned(), user.clone()]);
        }
        prefix.extend([id, "--", "sh", "-c", ENTER, "sh", workspace].map(str::to_owned));
        prefix
    }

    async fn abctl(&self, args: &[String], timeout: Duration) -> Result<String> {
        run(
            &self.name,
            &self.settings.program,
            args,
            Input::Nothing,
            Output::Capture,
            timeout,
        )
        .await
    }
}

fn words(list: &[&str]) -> Vec<String> {
    list.iter().map(|w| (*w).to_owned()).collect()
}

#[async_trait]
impl SandboxProvider for ArcboxProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> &'static str {
        "arcbox"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            isolation: Isolation::MicroVm,
            workspace: WorkspaceAccess::Copied,
            network_allowlist: false,
            secrets_outside: false,
        }
    }

    fn program(&self) -> String {
        self.settings.program.clone()
    }

    async fn probe(&self) -> Result<()> {
        let id = session_id();
        self.abctl(&self.create_args(&id, 300), CREATE).await?;
        let ran = self
            .abctl(&words(&["sandbox", "run", &id, "--", "true"]), STEP)
            .await;
        let removed = self.abctl(&words(&["sandbox", "rm", &id]), STEP).await;
        ran?;
        removed.map(|_| ())
    }

    async fn open(&self, request: &LaunchRequest) -> Result<SandboxLaunch> {
        let id = session_id();
        let workspace = crate::canonical(&request.workspace);
        self.abctl(&self.create_args(&id, self.settings.ttl_secs), CREATE)
            .await?;
        let session = Sandbox {
            provider: ArcboxProvider::new(self.name.clone(), self.settings.clone()),
            id: id.clone(),
            workspace: workspace.clone(),
            snapshot: None,
            work: tempfile::Builder::new()
                .prefix("cuma-arcbox-")
                .tempdir()
                .map_err(|e| failure(&self.name, e))?,
            keep: request
                .workspace
                .join(".cuma")
                .join("sandbox-results")
                .join(&id),
        };
        // From here on, a failure must not leave the sandbox behind.
        match session.populate(request).await {
            Ok(snapshot) => {
                let session = Sandbox {
                    snapshot,
                    ..session
                };
                Ok(SandboxLaunch::with_session(
                    self.exec_prefix(&id, &workspace),
                    Arc::new(session),
                ))
            }
            Err(err) => {
                session.remove().await;
                Err(err)
            }
        }
    }
}

/// One launch's sandbox.
struct Sandbox {
    provider: ArcboxProvider,
    id: String,
    workspace: String,
    snapshot: Option<Arc<Snapshot>>,
    work: tempfile::TempDir,
    keep: PathBuf,
}

impl Sandbox {
    /// Copy in the environment file and, for a task, the workspace.
    async fn populate(&self, request: &LaunchRequest) -> Result<Option<Arc<Snapshot>>> {
        let p = &self.provider;
        let env = self.work.path().join("env");
        write_private(&env, &env_file(&forwarded_env(request)))
            .map_err(|e| failure(&p.name, format!("writing the environment file: {e}")))?;
        p.abctl(
            &words(&[
                "sandbox",
                "cp",
                &env.display().to_string(),
                &format!("{}:/tmp/cuma-env", self.id),
            ]),
            STEP,
        )
        .await?;

        let snapshot = if request.collects() {
            let snapshot = Snapshot::take_async(&request.workspace, self.work.path()).await?;
            if let Some(archive) = snapshot.archive() {
                p.abctl(
                    &words(&[
                        "sandbox",
                        "cp",
                        &archive.display().to_string(),
                        &format!("{}:/tmp/cuma-ws.tar", self.id),
                    ]),
                    STEP,
                )
                .await?;
            }
            Some(snapshot)
        } else {
            None
        };
        let unpack = if snapshot.is_some() {
            "chmod 600 /tmp/cuma-env; mkdir -p \"$1\" && tar -xf /tmp/cuma-ws.tar -C \"$1\" && rm -f /tmp/cuma-ws.tar"
        } else {
            "chmod 600 /tmp/cuma-env"
        };
        p.abctl(
            &words(&[
                "sandbox",
                "run",
                &self.id,
                "--",
                "sh",
                "-c",
                unpack,
                "sh",
                &self.workspace,
            ]),
            STEP,
        )
        .await?;
        Ok(snapshot)
    }

    /// Archive the sandbox's workspace and merge it. The agent's execution
    /// may still be winding down: a sandbox runs one execution at a time.
    async fn collect(&self, snapshot: &Arc<Snapshot>) -> Result<()> {
        let p = &self.provider;
        let archive = words(&[
            "sandbox",
            "run",
            &self.id,
            "--",
            "sh",
            "-c",
            "tar -cf /tmp/cuma-out.tar -C \"$1\" .",
            "sh",
            &self.workspace,
        ]);
        let mut attempt = 0;
        loop {
            match p.abctl(&archive, STEP).await {
                Ok(_) => break,
                Err(_) if attempt < 20 => {
                    attempt += 1;
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Err(err) => return Err(err),
            }
        }
        let local = self.work.path().join("out.tar");
        p.abctl(
            &words(&[
                "sandbox",
                "cp",
                &format!("{}:/tmp/cuma-out.tar", self.id),
                &local.display().to_string(),
            ]),
            STEP,
        )
        .await?;
        let report = Arc::clone(snapshot)
            .merge_archive_async(local, self.work.path().join("result"), self.keep.clone())
            .await?;
        tracing::info!(sandbox = %p.name, %report, "brought the agent's work back");
        Ok(())
    }

    async fn remove(&self) {
        if let Err(err) = self
            .provider
            .abctl(&words(&["sandbox", "rm", &self.id]), STEP)
            .await
        {
            tracing::warn!(sandbox = %self.id, error = %err, "removing the ArcBox sandbox failed");
        }
    }
}

#[async_trait]
impl SandboxSession for Sandbox {
    async fn finish(&self) -> Result<()> {
        let collected = match &self.snapshot {
            Some(snapshot) => self.collect(snapshot).await,
            None => Ok(()),
        };
        self.remove().await;
        collected
    }

    async fn abort(&self) {
        self.remove().await;
    }
}

/// Write `contents` to `path`, readable by the owner only.
pub(crate) fn write_private(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents.as_bytes())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use cuma_core::ports::LaunchPurpose;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn create_names_the_source_the_resources_and_a_hard_lifetime() {
        let p = ArcboxProvider::new(
            "mac",
            ArcboxSandbox {
                image: Some("node:22".into()),
                cpus: Some(2),
                memory_mib: Some(2048),
                ..ArcboxSandbox::default()
            },
        );
        assert_eq!(
            p.create_args("cuma-1", 3600),
            words(&[
                "sandbox",
                "create",
                "--id",
                "cuma-1",
                "--from-image",
                "node:22",
                "--cpus",
                "2",
                "--memory",
                "2048",
                "--ttl",
                "3600"
            ])
        );
    }

    #[test]
    fn the_agent_enters_the_workspace_through_the_wrapper() {
        let p = ArcboxProvider::new(
            "mac",
            ArcboxSandbox {
                image: Some("node:22".into()),
                user: Some("node".into()),
                ..ArcboxSandbox::default()
            },
        );
        let prefix = p.exec_prefix("cuma-1", "/work");
        assert_eq!(
            prefix,
            words(&[
                "abctl", "sandbox", "exec", "--user", "node", "cuma-1", "--", "sh", "-c", ENTER,
                "sh", "/work"
            ])
        );
    }

    #[test]
    fn the_wrapper_loads_the_environment_and_runs_the_agent_in_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(ENTER)
            .arg("sh")
            .arg(dir.path())
            .args(["sh", "-c", "pwd"])
            .output()
            .unwrap();
        let pwd = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            std::fs::canonicalize(pwd.trim()).unwrap(),
            std::fs::canonicalize(dir.path()).unwrap()
        );
    }

    /// A stand-in for `abctl` backed by a directory per sandbox, so the copy
    /// in, the agent's edit and the copy out really happen.
    fn fake_abctl(dir: &std::path::Path) -> PathBuf {
        let root = dir.join("sandboxes");
        std::fs::create_dir_all(&root).unwrap();
        let script = format!(
            r#"#!/bin/sh
root={root}
echo "$@" >> {log}
[ "$1" = sandbox ] || exit 2
shift; op=$1; shift
case "$op" in
  create) mkdir -p "$root/$2" ;;
  cp) src=$1; dst=$2
      case "$src" in *:*) cp "$root/${{src%%:*}}${{src#*:}}" "$dst" ;;
      *) mkdir -p "$root/${{dst%%:*}}/tmp"; cp "$src" "$root/${{dst%%:*}}${{dst#*:}}" ;; esac ;;
  run) id=$1; shift; shift
       # Now: sh -c SCRIPT sh WORKSPACE. Run with paths re-rooted in the
       # sandbox's directory.
       cmd=$(printf '%s' "$3" | sed "s#/tmp/#$root/$id/tmp/#g")
       ws="$root/$id$5"
       mkdir -p "$root/$id/tmp"
       sh -c "$cmd" sh "$ws" ;;
  rm) rm -rf "$root/$1" ;;
esac
"#,
            root = root.display(),
            log = dir.join("calls").display()
        );
        let path = dir.join("abctl");
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[tokio::test]
    async fn a_task_copies_the_workspace_in_and_brings_the_agents_work_back() {
        let dir = tempfile::tempdir().unwrap();
        let abctl = fake_abctl(dir.path());
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("main.rs"), "before\n").unwrap();
        let p = ArcboxProvider::new(
            "mac",
            ArcboxSandbox {
                image: Some("node:22".into()),
                program: abctl.display().to_string(),
                ..ArcboxSandbox::default()
            },
        );

        let launch = p
            .open(&LaunchRequest::bare(ws.path(), LaunchPurpose::Execute))
            .await
            .unwrap();
        let id = launch.prefix()[3].clone();
        // The agent edits its copy.
        let copy = dir
            .path()
            .join("sandboxes")
            .join(&id)
            .join(crate::canonical(ws.path()).trim_start_matches('/'));
        assert_eq!(
            std::fs::read_to_string(copy.join("main.rs")).unwrap(),
            "before\n"
        );
        std::fs::write(copy.join("main.rs"), "after\n").unwrap();
        launch.finish().await.unwrap();

        assert_eq!(
            std::fs::read_to_string(ws.path().join("main.rs")).unwrap(),
            "after\n"
        );
        assert!(!dir.path().join("sandboxes").join(&id).exists(), "removed");
        let calls = std::fs::read_to_string(dir.path().join("calls")).unwrap();
        assert!(
            calls.contains("/tmp/cuma-env"),
            "the environment file is copied, never passed"
        );
    }
}
