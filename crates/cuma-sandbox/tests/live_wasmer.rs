//! An ACP agent in a real WASIX sandbox, through the wasmer provider.
//!
//! Opt-in: set `CUMA_LIVE_WASMER_PACKAGE` to a package with bash (e.g.
//! `wasmer/bash`) and have `wasmer` installed; `CUMA_LIVE_WASMER_PROGRAM`
//! names it when it is not on `PATH`. Without the package the test passes
//! without doing anything.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use cuma_config::sandbox::WasmerSandbox;
use cuma_sandbox::wasmer::WasmerProvider;
use cuma_sandbox::{LaunchPurpose, LaunchRequest, SandboxProvider};

#[tokio::test]
async fn an_acp_agent_runs_in_a_wasix_sandbox_with_the_workspace_mapped() {
    let Ok(package) = std::env::var("CUMA_LIVE_WASMER_PACKAGE") else {
        eprintln!("CUMA_LIVE_WASMER_PACKAGE is not set; skipping the live wasmer test");
        return;
    };
    let provider = WasmerProvider::new(
        "live-wasmer",
        WasmerSandbox {
            program: std::env::var("CUMA_LIVE_WASMER_PROGRAM").unwrap_or_else(|_| "wasmer".into()),
            ..WasmerSandbox::default()
        },
    );
    provider.probe().await.unwrap();

    let ws = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(ws.path()).unwrap();
    let fixture = common::bash_fixture();
    let mut request = LaunchRequest::bare(&workspace, LaunchPurpose::Execute);
    request.readable = vec![fixture.clone()];

    let launch = provider.open(&request).await.unwrap();
    // A package, `--`, then its own arguments.
    let command = [package, "--".to_owned(), fixture.display().to_string()];
    let mut agent = common::Agent::spawn_command(launch.prefix(), &command);
    agent.turn(&workspace).await;

    // The agent worked in the workspace, mapped at its own path.
    assert_eq!(
        std::fs::read_to_string(workspace.join("hello.txt")).unwrap(),
        "written by the sandboxed agent\n"
    );
    agent.close().await;
    launch.finish().await.unwrap();
    println!("LIVE-WASMER-ROUND-TRIP-COMPLETE");
}
