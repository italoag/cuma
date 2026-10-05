//! An ACP agent in a real CubeSandbox (or E2B) sandbox, through the e2b
//! provider and CUMA's stdio bridge.
//!
//! Opt-in: set `CUMA_LIVE_E2B_API_URL`, `CUMA_LIVE_E2B_DOMAIN` and
//! `CUMA_LIVE_E2B_TEMPLATE` (a template with `sh`, `sed` and `tar`).
//! `CUMA_LIVE_E2B_KEY_REF` names the variable holding the API key;
//! `CUMA_LIVE_E2B_ENVD_SCHEME` (default `https`) and `CUMA_LIVE_E2B_USER`
//! are optional; `CUMA_LIVE_CUMA_BIN` names the `cuma` executable.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use cuma_config::sandbox::E2bSandbox;
use cuma_sandbox::e2b::E2bProvider;
use cuma_sandbox::{LaunchPurpose, LaunchRequest, SandboxProvider};

#[tokio::test]
async fn an_acp_agent_runs_in_an_e2b_sandbox_through_the_bridge() {
    let Some(api_url) = common::live("CUMA_LIVE_E2B_API_URL", "e2b") else {
        return;
    };
    let defaults = E2bSandbox::default();
    let key_ref = common::optional("CUMA_LIVE_E2B_KEY_REF");
    let provider = E2bProvider::new(
        "live-e2b",
        E2bSandbox {
            api_url: api_url.clone(),
            domain: common::optional("CUMA_LIVE_E2B_DOMAIN").expect("CUMA_LIVE_E2B_DOMAIN"),
            template: common::optional("CUMA_LIVE_E2B_TEMPLATE").expect("CUMA_LIVE_E2B_TEMPLATE"),
            api_key_ref: key_ref.clone(),
            envd_scheme: common::optional("CUMA_LIVE_E2B_ENVD_SCHEME")
                .unwrap_or(defaults.envd_scheme.clone()),
            user: common::optional("CUMA_LIVE_E2B_USER").unwrap_or(defaults.user.clone()),
            ..defaults
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
    assert_eq!(session["kind"], "e2b");
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

    // The sandbox was killed.
    let mut request =
        reqwest::Client::new().get(format!("{}/sandboxes/{id}", api_url.trim_end_matches('/')));
    if let Some(name) = &key_ref {
        request = request.header("X-API-Key", std::env::var(name).unwrap());
    }
    let status = request.send().await.unwrap().status();
    assert_eq!(
        status,
        reqwest::StatusCode::NOT_FOUND,
        "sandbox {id} is still there"
    );
    println!("LIVE-E2B-ROUND-TRIP-COMPLETE");
}
