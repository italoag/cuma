//! An ACP agent in a real agent-sandbox pod, through the kubernetes provider.
//!
//! Opt-in: set `CUMA_LIVE_KUBERNETES_IMAGE` to an image with `sh`, `sed` and
//! `tar` the cluster can run, against a cluster with agent-sandbox installed
//! and `kubectl` configured. `CUMA_LIVE_KUBERNETES_NAMESPACE` (default
//! `default`) and `CUMA_LIVE_KUBERNETES_CONTEXT` are optional.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use cuma_config::sandbox::KubernetesSandbox;
use cuma_sandbox::kubernetes::KubernetesProvider;
use cuma_sandbox::{LaunchPurpose, LaunchRequest, SandboxProvider};
use std::time::Duration;

#[tokio::test]
async fn an_acp_agent_runs_in_an_agent_sandbox_pod_and_its_work_comes_back() {
    let Some(image) = common::live("CUMA_LIVE_KUBERNETES_IMAGE", "kubernetes") else {
        return;
    };
    let namespace =
        common::optional("CUMA_LIVE_KUBERNETES_NAMESPACE").unwrap_or_else(|| "default".into());
    let context = common::optional("CUMA_LIVE_KUBERNETES_CONTEXT");
    let provider = KubernetesProvider::new(
        "live-k8s",
        KubernetesSandbox {
            image: Some(image),
            namespace: namespace.clone(),
            context: context.clone(),
            ..KubernetesSandbox::default()
        },
    );
    provider.probe().await.unwrap();

    let ws = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(ws.path()).unwrap();
    let fixture = common::fixture_in(&workspace);

    let launch = provider
        .open(&LaunchRequest::bare(&workspace, LaunchPurpose::Execute))
        .await
        .unwrap();
    let mut agent = common::Agent::spawn_command(
        launch.prefix(),
        &["sh".to_owned(), fixture.display().to_string()],
    );
    agent.turn(&workspace).await;
    agent.close().await;
    common::assert_not_yet_back(&workspace);

    launch.finish().await.unwrap();
    common::assert_work_came_back(&workspace);

    // The Sandbox is deleted, and with it its pod.
    let mut args = Vec::new();
    if let Some(context) = &context {
        args.extend(["--context".to_owned(), context.clone()]);
    }
    args.extend(
        [
            "get",
            "sandboxes",
            "-n",
            &namespace,
            "-l",
            "dev.cuma.sandbox=live-k8s",
            "-o",
            "name",
        ]
        .map(str::to_owned),
    );
    let mut left = String::from("?");
    for _ in 0..60 {
        let out = std::process::Command::new("kubectl")
            .args(&args)
            .output()
            .unwrap();
        left = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if out.status.success() && left.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert_eq!(left, "", "the Sandbox was deleted");
    println!("LIVE-KUBERNETES-ROUND-TRIP-COMPLETE");
}
