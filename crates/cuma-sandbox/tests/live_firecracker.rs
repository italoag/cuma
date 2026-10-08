//! An ACP agent in a real Firecracker microVM, through the plugin provider
//! and the reference plugin in `plugins/sandbox/firecracker`.
//!
//! Opt-in: set `CUMA_LIVE_FIRECRACKER_KERNEL` and `CUMA_LIVE_FIRECRACKER_ROOTFS`
//! (a root filesystem with busybox and `/sbin/cuma-init`), on Linux with KVM,
//! e2fsprogs and python3; `CUMA_LIVE_FIRECRACKER_BIN` names `firecracker`
//! when it is not on `PATH`. `ci/sandboxes/firecracker-assets.sh` prepares
//! all three.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use cuma_config::sandbox::PluginSandbox;
use cuma_sandbox::plugin::PluginProvider;
use cuma_sandbox::{Isolation, LaunchPurpose, LaunchRequest, SandboxProvider, WorkspaceAccess};

#[tokio::test]
async fn an_acp_agent_runs_in_a_firecracker_microvm_and_its_work_comes_back() {
    let Some(kernel) = common::live("CUMA_LIVE_FIRECRACKER_KERNEL", "firecracker") else {
        return;
    };
    let rootfs =
        common::optional("CUMA_LIVE_FIRECRACKER_ROOTFS").expect("CUMA_LIVE_FIRECRACKER_ROOTFS");
    let mut options = toml::Table::new();
    options.insert("kernel".into(), toml::Value::String(kernel));
    options.insert("rootfs".into(), toml::Value::String(rootfs));
    options.insert("vcpus".into(), toml::Value::Integer(1));
    options.insert("memory_mib".into(), toml::Value::Integer(512));
    if let Some(bin) = common::optional("CUMA_LIVE_FIRECRACKER_BIN") {
        options.insert("firecracker".into(), toml::Value::String(bin));
    }
    let plugin = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../plugins/sandbox/firecracker/cuma-sandbox-firecracker"
    );
    let provider = PluginProvider::new(
        "live-firecracker",
        PluginSandbox {
            program: std::fs::canonicalize(plugin).unwrap().display().to_string(),
            options,
            ..PluginSandbox::default()
        },
    );
    // The probe boots a VM and runs `true` in it.
    provider.probe().await.unwrap();
    let capabilities = provider.capabilities();
    assert_eq!(capabilities.isolation, Isolation::MicroVm);
    assert_eq!(capabilities.workspace, WorkspaceAccess::Copied);

    let ws = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(ws.path()).unwrap();
    let fixture = common::fixture_in(&workspace);

    let launch = provider
        .open(&LaunchRequest::bare(&workspace, LaunchPurpose::Execute))
        .await
        .unwrap();
    // Strict: the boot must not reach the agent's stdout.
    let mut agent = common::Agent::spawn_command(
        launch.prefix(),
        &["sh".to_owned(), fixture.display().to_string()],
    );
    agent.turn(&workspace).await;
    agent.close().await;
    common::assert_not_yet_back(&workspace);

    launch.finish().await.unwrap();
    common::assert_work_came_back(&workspace);
    println!("LIVE-FIRECRACKER-ROUND-TRIP-COMPLETE");
}
