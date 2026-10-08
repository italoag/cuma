//! An ACP agent in a real ArcBox sandbox (a Firecracker microVM on macOS),
//! through the arcbox provider.
//!
//! Opt-in: set `CUMA_LIVE_ARCBOX_IMAGE` to an image with `sh`, `sed` and
//! `tar`, and have ArcBox running; `CUMA_LIVE_ARCBOX_PROGRAM` names `abctl`
//! when it is not on `PATH`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use cuma_config::sandbox::ArcboxSandbox;
use cuma_sandbox::arcbox::ArcboxProvider;
use cuma_sandbox::{LaunchPurpose, LaunchRequest, SandboxProvider};

#[tokio::test]
async fn an_acp_agent_runs_in_an_arcbox_sandbox_and_its_work_comes_back() {
    let Some(image) = common::live("CUMA_LIVE_ARCBOX_IMAGE", "arcbox") else {
        return;
    };
    let defaults = ArcboxSandbox::default();
    let program = common::optional("CUMA_LIVE_ARCBOX_PROGRAM").unwrap_or(defaults.program.clone());
    let provider = ArcboxProvider::new(
        "live-arcbox",
        ArcboxSandbox {
            image: Some(image),
            program: program.clone(),
            ..defaults
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
    let id = launch
        .prefix()
        .iter()
        .find(|w| w.starts_with("cuma-"))
        .expect("the prefix names the sandbox")
        .clone();
    let mut agent = common::Agent::spawn_command(
        launch.prefix(),
        &["sh".to_owned(), fixture.display().to_string()],
    );
    agent.turn(&workspace).await;
    agent.close().await;
    common::assert_not_yet_back(&workspace);

    launch.finish().await.unwrap();
    common::assert_work_came_back(&workspace);

    let listed = std::process::Command::new(&program)
        .args(["sandbox", "ls"])
        .output()
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&listed.stdout).contains(&id),
        "the sandbox {id} was removed"
    );
    println!("LIVE-ARCBOX-ROUND-TRIP-COMPLETE");
}
