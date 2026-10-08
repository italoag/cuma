//! An ACP agent in a real microVM, through the microsandbox provider.
//!
//! Opt-in: set `CUMA_LIVE_MICROSANDBOX_IMAGE` to an image with `sh` and
//! `sed` that `msb` can pull, and have `msb` installed. Without it the test
//! passes without doing anything.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use cuma_config::sandbox::MicrosandboxSandbox;
use cuma_sandbox::microsandbox::MicrosandboxProvider;
use cuma_sandbox::{LaunchPurpose, LaunchRequest, SandboxProvider};

#[tokio::test]
async fn an_acp_agent_runs_in_a_microvm_and_the_microvm_is_removed() {
    let Ok(image) = std::env::var("CUMA_LIVE_MICROSANDBOX_IMAGE") else {
        eprintln!("CUMA_LIVE_MICROSANDBOX_IMAGE is not set; skipping the live microsandbox test");
        return;
    };
    let provider = MicrosandboxProvider::new(
        "live-msb",
        MicrosandboxSandbox {
            image,
            ..MicrosandboxSandbox::default()
        },
    );
    provider.probe().await.unwrap();

    let ws = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(ws.path()).unwrap();
    let mut request = LaunchRequest::bare(&workspace, LaunchPurpose::Execute);
    request.readable = vec![common::fixture()];

    let launch = provider.open(&request).await.unwrap();
    let id = launch.prefix()[launch.prefix().len() - 2].clone();
    let mut agent = common::Agent::spawn(launch.prefix());
    agent.turn(&workspace).await;

    // The agent worked in the workspace, mounted into the VM at its own path.
    assert_eq!(
        std::fs::read_to_string(workspace.join("hello.txt")).unwrap(),
        "written by the sandboxed agent\n"
    );
    agent.close().await;
    launch.finish().await.unwrap();

    let listed = std::process::Command::new("msb")
        .args(["ls"])
        .output()
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&listed.stdout).contains(&id),
        "the microVM {id} was removed"
    );
    println!("LIVE-MICROSANDBOX-ROUND-TRIP-COMPLETE");
}
