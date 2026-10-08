//! kubernetes-sigs/agent-sandbox, driven by `kubectl`.
//!
//! A `Sandbox` (`agents.x-k8s.io/v1beta1`) is created around `image` — or a
//! `SandboxClaim` takes one from a warm pool — and waited for until `Ready`.
//! Its pod carries the sandbox's name. The workspace is streamed in through
//! `kubectl exec -i … tar -x`, the agent runs through `kubectl exec -i`
//! behind the `sh` wrapper, the workspace is streamed back for the merge, and
//! the sandbox deleted. `shutdownTime` is set too, so the controller reaps a
//! sandbox CUMA could not delete.
//!
//! The agent's variables reach it as a mode-0600 file inside the pod, not as
//! a Kubernetes Secret: tokens are never stored in etcd.

use crate::process::{Input, Output, run};
use crate::sync::Snapshot;
use crate::{
    Capabilities, ENTER, Isolation, LaunchRequest, SandboxLaunch, SandboxProvider, SandboxSession,
    WorkspaceAccess, env_file, failure, forwarded_env, session_id,
};
use async_trait::async_trait;
use cuma_config::sandbox::KubernetesSandbox;
use cuma_core::error::Result;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Keeps the pod's container alive until the sandbox is deleted.
const KEEPALIVE: &str = "trap 'exit 0' TERM; while true; do sleep 3600 & wait $!; done";
/// Streaming the workspace either way.
const STREAM: Duration = Duration::from_secs(600);

/// `kind = "kubernetes"`.
pub struct KubernetesProvider {
    name: String,
    settings: KubernetesSandbox,
}

impl KubernetesProvider {
    /// A provider for `[sandboxes.<name>]`.
    pub fn new(name: impl Into<String>, settings: KubernetesSandbox) -> Self {
        Self {
            name: name.into(),
            settings,
        }
    }

    /// `--context` and `--kubeconfig`, before every subcommand.
    fn global(&self) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(context) = &self.settings.context {
            args.extend(["--context".to_owned(), context.clone()]);
        }
        if let Some(kubeconfig) = &self.settings.kubeconfig {
            args.extend(["--kubeconfig".to_owned(), kubeconfig.clone()]);
        }
        args
    }

    /// The resource CUMA creates for one launch: a `Sandbox`, or a
    /// `SandboxClaim` on the warm pool.
    pub fn manifest(&self, id: &str, now: chrono::DateTime<chrono::Utc>) -> Value {
        let s = &self.settings;
        let lifetime = i64::try_from(s.lifetime_secs).unwrap_or(i64::MAX);
        let shutdown = (now
            + chrono::TimeDelta::try_seconds(lifetime).unwrap_or(chrono::TimeDelta::MAX))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let metadata = json!({
            "name": id,
            "namespace": s.namespace,
            "labels": {
                "app.kubernetes.io/managed-by": "cuma",
                "dev.cuma.sandbox": self.name,
            },
        });
        if let Some(pool) = &s.warm_pool {
            return json!({
                "apiVersion": "extensions.agents.x-k8s.io/v1beta1",
                "kind": "SandboxClaim",
                "metadata": metadata,
                "spec": {
                    "warmPoolRef": { "name": pool },
                    "lifecycle": { "shutdownTime": shutdown, "shutdownPolicy": "Delete" },
                },
            });
        }
        let mut pod = json!({
            "automountServiceAccountToken": false,
            "containers": [{
                "name": s.container,
                "image": s.image,
                "command": ["sh", "-c", KEEPALIVE],
            }],
        });
        if let Some(class) = &s.runtime_class {
            pod["runtimeClassName"] = json!(class);
        }
        if let Some(account) = &s.service_account {
            pod["serviceAccountName"] = json!(account);
        }
        json!({
            "apiVersion": "agents.x-k8s.io/v1beta1",
            "kind": "Sandbox",
            "metadata": metadata,
            "spec": {
                "podTemplate": {
                    "metadata": { "labels": { "dev.cuma.sandbox": self.name } },
                    "spec": pod,
                },
                "shutdownTime": shutdown,
                "shutdownPolicy": "Delete",
            },
        })
    }

    /// `kubectl exec` into the pod, with `-i` when it reads stdin.
    fn exec(&self, pod: &str, stdin: bool) -> Vec<String> {
        let s = &self.settings;
        let mut args = self.global();
        args.push("exec".to_owned());
        if stdin {
            args.push("-i".to_owned());
        }
        args.extend(["-n", &s.namespace, pod, "-c", &s.container, "--"].map(str::to_owned));
        args
    }

    async fn kubectl(
        &self,
        args: Vec<String>,
        input: Input<'_>,
        output: Output<'_>,
        timeout: Duration,
    ) -> Result<String> {
        run(
            &self.name,
            &self.settings.program,
            &args,
            input,
            output,
            timeout,
        )
        .await
    }

    fn resource(&self, id: &str) -> String {
        match self.settings.warm_pool {
            Some(_) => format!("sandboxclaim/{id}"),
            None => format!("sandbox/{id}"),
        }
    }

    /// Wait for the sandbox behind `id` and return its pod's name.
    async fn ready(&self, id: &str) -> Result<String> {
        let s = &self.settings;
        let deadline = std::time::Instant::now() + Duration::from_secs(s.ready_timeout_secs);
        let sandbox = if s.warm_pool.is_some() {
            loop {
                let mut args = self.global();
                args.extend(
                    [
                        "get",
                        &self.resource(id),
                        "-n",
                        &s.namespace,
                        "-o",
                        "jsonpath={.status.sandbox.name}",
                    ]
                    .map(str::to_owned),
                );
                let name = self
                    .kubectl(
                        args,
                        Input::Nothing,
                        Output::Capture,
                        Duration::from_secs(30),
                    )
                    .await?;
                if !name.trim().is_empty() {
                    break name.trim().to_owned();
                }
                if std::time::Instant::now() > deadline {
                    return Err(failure(
                        &self.name,
                        format!("no sandbox was assigned to claim {id}"),
                    ));
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        } else {
            id.to_owned()
        };
        let remaining = deadline
            .saturating_duration_since(std::time::Instant::now())
            .as_secs()
            .max(1);
        let mut args = self.global();
        args.extend(
            [
                "wait",
                "--for=condition=Ready",
                &format!("sandbox/{sandbox}"),
                "-n",
                &s.namespace,
                &format!("--timeout={remaining}s"),
            ]
            .map(str::to_owned),
        );
        self.kubectl(
            args,
            Input::Nothing,
            Output::Capture,
            Duration::from_secs(remaining + 30),
        )
        .await?;
        Ok(sandbox)
    }
}

#[async_trait]
impl SandboxProvider for KubernetesProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> &'static str {
        "kubernetes"
    }

    fn capabilities(&self) -> Capabilities {
        let class = self.settings.runtime_class.as_deref().unwrap_or_default();
        Capabilities {
            isolation: if class.contains("kata") {
                Isolation::MicroVm
            } else {
                Isolation::Container
            },
            workspace: WorkspaceAccess::Copied,
            network_allowlist: false,
            secrets_outside: false,
        }
    }

    fn program(&self) -> String {
        self.settings.program.clone()
    }

    async fn probe(&self) -> Result<()> {
        let s = &self.settings;
        let resource = match s.warm_pool {
            Some(_) => "sandboxclaims.extensions.agents.x-k8s.io",
            None => "sandboxes.agents.x-k8s.io",
        };
        let mut crd = self.global();
        crd.extend(["get", "crd", resource].map(str::to_owned));
        self.kubectl(
            crd,
            Input::Nothing,
            Output::Capture,
            Duration::from_secs(30),
        )
        .await
        .map_err(|err| failure(&self.name, format!("agent-sandbox is not installed: {err}")))?;
        for (verb, what) in [("create", resource), ("create", "pods/exec")] {
            let mut can = self.global();
            can.extend(["auth", "can-i", verb, what, "-n", &s.namespace].map(str::to_owned));
            let answer = self
                .kubectl(
                    can,
                    Input::Nothing,
                    Output::Capture,
                    Duration::from_secs(30),
                )
                .await
                .unwrap_or_default();
            if answer.trim() != "yes" {
                return Err(failure(
                    &self.name,
                    format!("not allowed to {verb} {what} in namespace {}", s.namespace),
                ));
            }
        }
        Ok(())
    }

    async fn open(&self, request: &LaunchRequest) -> Result<SandboxLaunch> {
        let id = session_id();
        let manifest = serde_json::to_vec(&self.manifest(&id, chrono::Utc::now()))
            .map_err(|e| failure(&self.name, e))?;
        let mut apply = self.global();
        apply.extend(["apply", "-n", &self.settings.namespace, "-f", "-"].map(str::to_owned));
        self.kubectl(
            apply,
            Input::Bytes(&manifest),
            Output::Capture,
            Duration::from_secs(60),
        )
        .await?;

        let mut session = Pod {
            provider: KubernetesProvider::new(self.name.clone(), self.settings.clone()),
            resource: self.resource(&id),
            pod: String::new(),
            workspace: crate::canonical(&request.workspace),
            snapshot: None,
            work: tempfile::Builder::new()
                .prefix("cuma-k8s-")
                .tempdir()
                .map_err(|e| failure(&self.name, e))?,
            keep: request
                .workspace
                .join(".cuma")
                .join("sandbox-results")
                .join(&id),
        };
        // From here on, a failure must not leave the sandbox behind.
        let populated = async {
            session.pod = self.ready(&id).await?;
            session.populate(request).await
        }
        .await;
        match populated {
            Ok(snapshot) => {
                session.snapshot = snapshot;
                let mut prefix = vec![self.settings.program.clone()];
                prefix.extend(self.exec(&session.pod, true));
                prefix.extend(["sh", "-c", ENTER, "sh", &session.workspace].map(str::to_owned));
                Ok(SandboxLaunch::with_session(prefix, Arc::new(session)))
            }
            Err(err) => {
                session.delete().await;
                Err(err)
            }
        }
    }
}

/// One launch's sandbox and pod.
struct Pod {
    provider: KubernetesProvider,
    resource: String,
    pod: String,
    workspace: String,
    snapshot: Option<Arc<Snapshot>>,
    work: tempfile::TempDir,
    keep: PathBuf,
}

impl Pod {
    async fn populate(&self, request: &LaunchRequest) -> Result<Option<Arc<Snapshot>>> {
        let p = &self.provider;
        let env = env_file(&forwarded_env(request));
        let mut write_env = p.exec(&self.pod, true);
        write_env.extend(["sh", "-c", "umask 077 && cat > /tmp/cuma-env"].map(str::to_owned));
        p.kubectl(
            write_env,
            Input::Bytes(env.as_bytes()),
            Output::Capture,
            STREAM,
        )
        .await?;
        if !request.collects() {
            return Ok(None);
        }
        let snapshot = Snapshot::take_async(&request.workspace, self.work.path()).await?;
        if let Some(archive) = snapshot.archive() {
            let mut unpack = p.exec(&self.pod, true);
            unpack.extend(
                [
                    "sh",
                    "-c",
                    "mkdir -p \"$1\" && tar -xf - -C \"$1\"",
                    "sh",
                    &self.workspace,
                ]
                .map(str::to_owned),
            );
            p.kubectl(unpack, Input::File(archive), Output::Capture, STREAM)
                .await?;
        }
        Ok(Some(snapshot))
    }

    async fn collect(&self, snapshot: &Arc<Snapshot>) -> Result<()> {
        let p = &self.provider;
        let local = self.work.path().join("out.tar");
        let mut archive = p.exec(&self.pod, false);
        archive.extend(["tar", "-cf", "-", "-C", &self.workspace, "."].map(str::to_owned));
        p.kubectl(archive, Input::Nothing, Output::File(&local), STREAM)
            .await?;
        let report = Arc::clone(snapshot)
            .merge_archive_async(local, self.work.path().join("result"), self.keep.clone())
            .await?;
        tracing::info!(sandbox = %p.name, %report, "brought the agent's work back");
        Ok(())
    }

    async fn delete(&self) {
        let p = &self.provider;
        let mut args = p.global();
        args.extend(
            [
                "delete",
                &self.resource,
                "-n",
                &p.settings.namespace,
                "--ignore-not-found",
                "--wait=false",
            ]
            .map(str::to_owned),
        );
        if let Err(err) = p
            .kubectl(
                args,
                Input::Nothing,
                Output::Capture,
                Duration::from_secs(60),
            )
            .await
        {
            tracing::warn!(resource = %self.resource, error = %err, "deleting the sandbox failed");
        }
    }
}

#[async_trait]
impl SandboxSession for Pod {
    async fn finish(&self) -> Result<()> {
        let collected = match &self.snapshot {
            Some(snapshot) => self.collect(snapshot).await,
            None => Ok(()),
        };
        self.delete().await;
        collected
    }

    async fn abort(&self) {
        self.delete().await;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use chrono::TimeZone as _;
    use cuma_core::ports::LaunchPurpose;
    use std::os::unix::fs::PermissionsExt as _;

    fn settings() -> KubernetesSandbox {
        KubernetesSandbox {
            image: Some("node:22".into()),
            runtime_class: Some("gvisor".into()),
            ..KubernetesSandbox::default()
        }
    }

    #[test]
    fn a_sandbox_wraps_the_image_with_a_keepalive_and_an_expiry() {
        let p = KubernetesProvider::new("cluster", settings());
        let now = chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let m = p.manifest("cuma-1", now);
        assert_eq!(m["apiVersion"], "agents.x-k8s.io/v1beta1");
        assert_eq!(m["kind"], "Sandbox");
        assert_eq!(m["metadata"]["name"], "cuma-1");
        let pod = &m["spec"]["podTemplate"]["spec"];
        assert_eq!(pod["containers"][0]["name"], "agent");
        assert_eq!(pod["containers"][0]["image"], "node:22");
        assert_eq!(pod["runtimeClassName"], "gvisor");
        assert_eq!(pod["automountServiceAccountToken"], false);
        assert_eq!(m["spec"]["shutdownTime"], "2026-01-01T01:00:00Z");
        assert_eq!(m["spec"]["shutdownPolicy"], "Delete");
    }

    #[test]
    fn a_warm_pool_is_claimed_rather_than_a_sandbox_created() {
        let p = KubernetesProvider::new(
            "cluster",
            KubernetesSandbox {
                image: None,
                warm_pool: Some("agents".into()),
                ..KubernetesSandbox::default()
            },
        );
        let m = p.manifest("cuma-1", chrono::Utc::now());
        assert_eq!(m["apiVersion"], "extensions.agents.x-k8s.io/v1beta1");
        assert_eq!(m["kind"], "SandboxClaim");
        assert_eq!(m["spec"]["warmPoolRef"]["name"], "agents");
        assert_eq!(p.resource("cuma-1"), "sandboxclaim/cuma-1");
    }

    #[test]
    fn exec_targets_the_container_in_the_namespace() {
        let p = KubernetesProvider::new(
            "cluster",
            KubernetesSandbox {
                context: Some("kind-agents".into()),
                namespace: "agents".into(),
                ..settings()
            },
        );
        assert_eq!(
            p.exec("cuma-1", true),
            [
                "--context",
                "kind-agents",
                "exec",
                "-i",
                "-n",
                "agents",
                "cuma-1",
                "-c",
                "agent",
                "--"
            ]
        );
    }

    /// A stand-in for `kubectl` backed by a directory per pod.
    fn fake_kubectl(dir: &std::path::Path) -> PathBuf {
        let root = dir.join("pods");
        std::fs::create_dir_all(&root).unwrap();
        let script = format!(
            r#"#!/bin/sh
root={root}
echo "$@" >> {log}
case "$1" in
  apply) name=$(sed -n 's/.*"name":"\(cuma-[0-9a-f]*\)".*/\1/p'); mkdir -p "$root/$name/tmp" ;;
  wait) exit 0 ;;
  delete) rm -rf "$root/${{2#*/}}" ;;
  exec) shift
        [ "$1" = -i ] && shift
        shift; ns=$1; shift; pod=$1; shift; shift; shift; shift
        if [ "$1" = tar ]; then
          (cd "$root/$pod$5" && tar -cf - .)
        else
          cmd=$(printf '%s' "$3" | sed "s#/tmp/#$root/$pod/tmp/#g")
          ws="$root/$pod$5"
          sh -c "$cmd" sh "$ws"
        fi ;;
esac
"#,
            root = root.display(),
            log = dir.join("calls").display()
        );
        let path = dir.join("kubectl");
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[tokio::test]
    async fn a_task_streams_the_workspace_in_and_the_agents_work_back() {
        let dir = tempfile::tempdir().unwrap();
        let kubectl = fake_kubectl(dir.path());
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("lib.rs"), "v1\n").unwrap();
        let p = KubernetesProvider::new(
            "cluster",
            KubernetesSandbox {
                program: kubectl.display().to_string(),
                ..settings()
            },
        );

        let launch = p
            .open(&LaunchRequest::bare(ws.path(), LaunchPurpose::Execute))
            .await
            .unwrap();
        let prefix = launch.prefix().to_vec();
        let pod = prefix[prefix.iter().position(|w| w == "-n").unwrap() + 2].clone();
        assert!(prefix.windows(2).any(|w| w == ["exec", "-i"]));
        let copy = dir
            .path()
            .join("pods")
            .join(&pod)
            .join(crate::canonical(ws.path()).trim_start_matches('/'));
        assert_eq!(
            std::fs::read_to_string(copy.join("lib.rs")).unwrap(),
            "v1\n"
        );
        std::fs::write(copy.join("lib.rs"), "v2\n").unwrap();
        std::fs::write(copy.join("new.rs"), "new\n").unwrap();
        launch.finish().await.unwrap();

        assert_eq!(
            std::fs::read_to_string(ws.path().join("lib.rs")).unwrap(),
            "v2\n"
        );
        assert_eq!(
            std::fs::read_to_string(ws.path().join("new.rs")).unwrap(),
            "new\n"
        );
        assert!(!dir.path().join("pods").join(&pod).exists(), "deleted");
        let calls = std::fs::read_to_string(dir.path().join("calls")).unwrap();
        assert!(
            calls.contains(&format!("wait --for=condition=Ready sandbox/{pod}")),
            "{calls}"
        );
    }
}
