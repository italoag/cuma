//! An ACP agent in a real container, through the docker provider.
//!
//! Opt-in: set `CUMA_LIVE_DOCKER_IMAGE` to an image with `sh` and `sed`
//! (e.g. `alpine:3`) and have a container engine running. Without it the
//! test passes without doing anything, so `cargo test` stays hermetic.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use cuma_config::sandbox::DockerSandbox;
use cuma_sandbox::docker::DockerProvider;
use cuma_sandbox::{LaunchPurpose, LaunchRequest, SandboxProvider};

#[tokio::test]
async fn an_acp_agent_runs_in_a_container_and_the_container_is_removed() {
    let Ok(image) = std::env::var("CUMA_LIVE_DOCKER_IMAGE") else {
        eprintln!("CUMA_LIVE_DOCKER_IMAGE is not set; skipping the live docker test");
        return;
    };
    let provider = DockerProvider::new(
        "live-test",
        DockerSandbox {
            image,
            // An image's own entrypoint may print to stdout, which is ACP's.
            entrypoint: Some(String::new()),
            network: "none".into(),
            ..DockerSandbox::default()
        },
    );
    let ws = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(ws.path()).unwrap();
    let mut request = LaunchRequest::bare(&workspace, LaunchPurpose::Execute);
    request.readable = vec![common::fixture()];

    let launch = provider.open(&request).await.unwrap();
    let mut agent = common::Agent::spawn(launch.prefix());
    agent.turn(&workspace).await;

    // The agent worked in the workspace, mounted at its own path.
    assert_eq!(
        std::fs::read_to_string(workspace.join("hello.txt")).unwrap(),
        "written by the sandboxed agent\n"
    );
    agent.close().await;
    launch.finish().await.unwrap();

    let left = std::process::Command::new("docker")
        .args([
            "ps",
            "-a",
            "--filter",
            "label=dev.cuma.sandbox=live-test",
            "-q",
        ])
        .output()
        .unwrap();
    assert!(left.status.success());
    assert_eq!(
        String::from_utf8_lossy(&left.stdout).trim(),
        "",
        "the container was removed"
    );
    println!("LIVE-DOCKER-ROUND-TRIP-COMPLETE");
}
