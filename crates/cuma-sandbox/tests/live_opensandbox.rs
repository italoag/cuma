//! An ACP agent in a real OpenSandbox sandbox, through the opensandbox
//! provider and CUMA's stdio bridge.
//!
//! Opt-in: set `CUMA_LIVE_OPENSANDBOX_URL` to a running `opensandbox-server`
//! and `CUMA_LIVE_OPENSANDBOX_IMAGE` to an image with `sh`, `sed`, `tar` and
//! `node` it can run. `CUMA_LIVE_OPENSANDBOX_KEY_REF` names the variable
//! holding the server's API key, if it has one; `CUMA_LIVE_CUMA_BIN` names
//! the `cuma` executable (default: `target/debug/cuma`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use cuma_config::sandbox::OpenSandboxSandbox;
use cuma_sandbox::opensandbox::OpenSandboxProvider;
use cuma_sandbox::{LaunchPurpose, LaunchRequest, SandboxProvider};

#[tokio::test]
async fn an_acp_agent_runs_in_an_opensandbox_sandbox_through_the_bridge() {
    let Some(server) = common::live("CUMA_LIVE_OPENSANDBOX_URL", "opensandbox") else {
        return;
    };
    let image =
        common::optional("CUMA_LIVE_OPENSANDBOX_IMAGE").expect("CUMA_LIVE_OPENSANDBOX_IMAGE");
    let key_ref = common::optional("CUMA_LIVE_OPENSANDBOX_KEY_REF");
    let provider = OpenSandboxProvider::new(
        "live-osb",
        OpenSandboxSandbox {
            server_url: server.clone(),
            image,
            api_key_ref: key_ref.clone(),
            ..OpenSandboxSandbox::default()
        },
        Some(common::cuma_bin()),
    );
    provider.probe().await.unwrap();

    let ws = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(ws.path()).unwrap();
    let fixture = common::fixture_in(&workspace);

    let launch = provider
        .open(&LaunchRequest::bare(&workspace, LaunchPurpose::Execute))
        .await
        .unwrap();
    let session = common::bridge_session(launch.prefix());
    assert_eq!(session["kind"], "opensandbox");
    let id = session["sandbox_id"]
        .as_str()
        .expect("the session names its sandbox")
        .to_owned();
    let mut agent = common::Agent::spawn_command(
        launch.prefix(),
        &["sh".to_owned(), fixture.display().to_string()],
    );
    agent.turn(&workspace).await;
    agent.close().await;
    common::assert_not_yet_back(&workspace);

    launch.finish().await.unwrap();
    common::assert_work_came_back(&workspace);

    // The sandbox is gone, or on its way out.
    let mut request = reqwest::Client::new().get(format!(
        "{}/v1/sandboxes/{id}",
        server.trim_end_matches('/')
    ));
    if let Some(name) = &key_ref {
        request = request.header("OPEN-SANDBOX-API-KEY", std::env::var(name).unwrap());
    }
    let response = request.send().await.unwrap();
    if response.status().is_success() {
        let body: serde_json::Value = response.json().await.unwrap();
        let state = body["status"]["state"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(
            ["Stopping", "Terminated", "Failed"].contains(&state.as_str()),
            "sandbox {id} is still {state}"
        );
    } else {
        assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
    }
    println!("LIVE-OPENSANDBOX-ROUND-TRIP-COMPLETE");
}
