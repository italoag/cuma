//! An ACP agent in a real agentOS VM, through the plugin provider and the
//! reference plugin in `plugins/sandbox/agentos`.
//!
//! Opt-in: set `CUMA_LIVE_AGENTOS_MODULES` to the `node_modules` directory
//! holding `@rivet-dev/agentos-core`, and have `node` installed. Without it the
//! test passes without doing anything.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use cuma_config::sandbox::PluginSandbox;
use cuma_sandbox::plugin::PluginProvider;
use cuma_sandbox::{LaunchPurpose, LaunchRequest, SandboxProvider, WorkspaceAccess};

#[tokio::test]
async fn an_acp_agent_runs_in_an_agentos_vm_through_the_reference_plugin() {
    let Ok(modules) = std::env::var("CUMA_LIVE_AGENTOS_MODULES") else {
        eprintln!("CUMA_LIVE_AGENTOS_MODULES is not set; skipping the live agentOS test");
        return;
    };
    let plugin = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../plugins/sandbox/agentos/cuma-sandbox-agentos.mjs"
    );
    let mut options = toml::Table::new();
    options.insert("module".into(), toml::Value::String(modules));
    let provider = PluginProvider::new(
        "live-agentos",
        PluginSandbox {
            program: std::fs::canonicalize(plugin).unwrap().display().to_string(),
            options,
            ..PluginSandbox::default()
        },
    );
    provider.probe().await.unwrap();
    assert_eq!(provider.capabilities().workspace, WorkspaceAccess::Mounted);

    let ws = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(ws.path()).unwrap();
    let mut request = LaunchRequest::bare(&workspace, LaunchPurpose::Execute);
    request.readable = vec![common::fixture()];

    let launch = provider.open(&request).await.unwrap();
    let mut agent = common::Agent::spawn(launch.prefix());
    agent.turn(&workspace).await;

    // The agent worked in the workspace, mounted writable at its own path,
    // and what it wrote belongs to this user.
    let hello = workspace.join("hello.txt");
    assert_eq!(
        std::fs::read_to_string(&hello).unwrap(),
        "written by the sandboxed agent\n"
    );
    use std::os::unix::fs::MetadataExt as _;
    assert_eq!(
        std::fs::metadata(&hello).unwrap().uid(),
        std::fs::metadata(&workspace).unwrap().uid()
    );
    agent.close().await;
    launch.finish().await.unwrap();
    println!("LIVE-AGENTOS-ROUND-TRIP-COMPLETE");
}
