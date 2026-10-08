//! The Firecracker plugin's relay, against a stand-in for the VM: the boot
//! is discarded, nothing CUMA writes reaches the guest before it is ready,
//! the agent's JSON-RPC goes both ways, and the end of stdin stops the VM.
//! Needs python3; skipped without it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use base64::Engine as _;
use std::os::unix::fs::PermissionsExt as _;

/// What Firecracker would boot, in Python: it records how it was called,
/// prints kernel noise, notes any input that arrives before the guest is
/// ready, says it is ready, then runs the agent's command in the workspace —
/// as cuma-init does.
const FAKE_FIRECRACKER: &str = r#"#!/usr/bin/env python3
import base64, json, os, select, sys, time
args = sys.argv[1:]
config_path = args[args.index("--config-file") + 1]
config = json.load(open(config_path))
record = os.path.join(os.path.dirname(config_path), "fake.json")
json.dump({"args": args, "config": config}, open(record, "w"))
boot = dict(w.split("=", 1) for w in config["boot-source"]["boot_args"].split() if "=" in w)
workspace = base64.b64decode(boot["cuma.ws"]).decode()
argv = [base64.b64decode(w).decode() for w in boot["cuma.cmd"].split(",")]
out = sys.stdout.buffer
out.write(b"[    0.000000] Linux version 6.1.0 (stand-in) #1 SMP\r\n{not json: boot noise}\r\n")
out.flush()
time.sleep(0.5)
if select.select([0], [], [], 0)[0]:
    open(record + ".early", "w").write("input arrived before the guest was ready")
# The sign and its line's end in separate reads, as a console delivers them
# when it pleases: the end must not reach the agent as an empty line.
out.write(b"CUMA-INIT-READY")
out.flush()
time.sleep(0.3)
out.write(b"\r\n")
out.flush()
os.chdir(workspace)
os.execvp(argv[0], argv)
"#;

#[tokio::test]
async fn the_firecracker_relay_hides_the_boot_and_carries_a_whole_turn() {
    if which::which("python3").is_err() {
        eprintln!("python3 is not installed; skipping the Firecracker relay test");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let fake = dir.path().join("firecracker");
    std::fs::write(&fake, FAKE_FIRECRACKER).unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

    let ws = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(ws.path()).unwrap();
    let fixture = common::fixture_in(&workspace);

    // What the plugin's `open` leaves behind, without the ext4 image it
    // needs e2fsprogs for.
    let session = dir.path().join("session");
    std::fs::create_dir(&session).unwrap();
    let saved = serde_json::json!({
        "workspace": workspace,
        "image": session.join("workspace.ext4"),
        "options": {
            "kernel": "/boot/vmlinux",
            "rootfs": "/var/lib/cuma/rootfs.ext4",
            "vcpus": 1,
            "memory_mib": 512,
            "firecracker": fake,
        },
    });
    std::fs::write(session.join("session.json"), saved.to_string()).unwrap();

    let plugin = std::fs::canonicalize(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../plugins/sandbox/firecracker/cuma-sandbox-firecracker"
    ))
    .unwrap();
    let prefix = [
        "python3".to_owned(),
        plugin.display().to_string(),
        "exec".to_owned(),
        session.display().to_string(),
        "--".to_owned(),
    ];
    // Strict: a line of boot noise on stdout fails the turn.
    let mut agent =
        common::Agent::spawn_command(&prefix, &["sh".to_owned(), fixture.display().to_string()]);
    agent.turn(&workspace).await;
    // The stand-in, like a VM, never sees the end of stdin: the relay stops it.
    agent.close().await;
    common::assert_work_came_back(&workspace);

    assert!(
        !session.join("fake.json.early").exists(),
        "input reached the guest before it was ready"
    );
    let record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(session.join("fake.json")).unwrap()).unwrap();
    let args: Vec<&str> = record["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    assert_eq!(args[0], "--no-api");
    assert!(
        args.contains(&"--log-path"),
        "Firecracker's own log stays off the console: {args:?}"
    );
    let config = &record["config"];
    let boot = config["boot-source"]["boot_args"].as_str().unwrap();
    assert!(
        boot.contains("console=ttyS0") && boot.contains("init=/sbin/cuma-init"),
        "{boot}"
    );
    let decode = |word: &str| {
        String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(word)
                .unwrap(),
        )
        .unwrap()
    };
    let field = |key: &str| {
        boot.split_whitespace()
            .find_map(|w| w.strip_prefix(&format!("{key}=")))
            .unwrap()
            .to_owned()
    };
    assert_eq!(decode(&field("cuma.ws")), workspace.display().to_string());
    let command: Vec<String> = field("cuma.cmd").split(',').map(decode).collect();
    assert_eq!(command, ["sh".to_owned(), fixture.display().to_string()]);
    assert_eq!(
        config["drives"][0]["is_read_only"], true,
        "the root filesystem is shared and read-only"
    );
    assert_eq!(config["drives"][1]["is_read_only"], false);
    assert_eq!(config["machine-config"]["vcpu_count"], 1);
}
