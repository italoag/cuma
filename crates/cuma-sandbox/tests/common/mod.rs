//! Speaking ACP to the shell fixture agent through a sandbox's prefix.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, dead_code)]

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};

/// The fixture agent: POSIX sh and sed, nothing else.
pub fn fixture() -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/acp_agent.sh"
    ))
}

/// The same agent in bash builtins only, for shells without `sed`.
pub fn bash_fixture() -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/acp_agent.bash"
    ))
}

/// The fixture, copied into `workspace`: a sandbox that copies the
/// workspace sees nothing else of this machine.
pub fn fixture_in(workspace: &Path) -> PathBuf {
    let copy = workspace.join("acp_agent.sh");
    std::fs::copy(fixture(), &copy).unwrap();
    copy
}

/// A required variable, or `None` with the skip notice printed.
pub fn live(variable: &str, what: &str) -> Option<String> {
    let value = std::env::var(variable).ok().filter(|v| !v.is_empty());
    if value.is_none() {
        eprintln!("{variable} is not set; skipping the live {what} test");
    }
    value
}

/// An optional variable.
pub fn optional(variable: &str) -> Option<String> {
    std::env::var(variable).ok().filter(|v| !v.is_empty())
}

/// CUMA's executable, which agents in HTTP-reached sandboxes are launched
/// through (`cuma sandbox exec`).
pub fn cuma_bin() -> PathBuf {
    let bin = optional("CUMA_LIVE_CUMA_BIN").map_or_else(
        || {
            PathBuf::from(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../target/debug/cuma"
            ))
        },
        PathBuf::from,
    );
    assert!(
        bin.exists(),
        "{} does not exist: build it (cargo build -p cuma-cli) or set CUMA_LIVE_CUMA_BIN",
        bin.display()
    );
    std::fs::canonicalize(bin).unwrap()
}

/// What `cuma sandbox exec` would read: the session file a bridged launch's
/// prefix names.
pub fn bridge_session(prefix: &[String]) -> serde_json::Value {
    let at = prefix.iter().position(|w| w == "--session").unwrap();
    serde_json::from_str(&std::fs::read_to_string(&prefix[at + 1]).unwrap()).unwrap()
}

/// The work of a copied sandbox arrives only when the launch finishes.
pub fn assert_not_yet_back(workspace: &Path) {
    assert!(
        !workspace.join("hello.txt").exists(),
        "the sandbox works on a copy; nothing should be here before the launch finishes"
    );
}

/// What the fixture wrote, once back.
pub fn assert_work_came_back(workspace: &Path) {
    assert_eq!(
        std::fs::read_to_string(workspace.join("hello.txt")).unwrap(),
        "written by the sandboxed agent\n"
    );
}

/// The fixture running under `prefix`.
pub struct Agent {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Lines<BufReader<ChildStdout>>,
}

impl Agent {
    /// `sh acp_agent.sh` under `prefix`.
    pub fn spawn(prefix: &[String]) -> Self {
        Self::spawn_command(prefix, &["sh".to_owned(), fixture().display().to_string()])
    }

    /// `command` under `prefix`.
    pub fn spawn_command(prefix: &[String], command: &[String]) -> Self {
        let mut words: Vec<String> = prefix.to_vec();
        words.extend(command.iter().cloned());
        let mut child = tokio::process::Command::new(&words[0])
            .args(&words[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().unwrap()).lines();
        Self {
            child,
            stdin,
            stdout,
        }
    }

    /// Send one request and read until its response; notifications seen on
    /// the way are returned with it.
    pub async fn call(&mut self, id: u64, method: &str, params: Value) -> (Value, Vec<Value>) {
        let request = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let stdin = self.stdin.as_mut().unwrap();
        stdin
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        stdin.flush().await.unwrap();
        let mut notifications = Vec::new();
        loop {
            let line = tokio::time::timeout(Duration::from_secs(120), self.stdout.next_line())
                .await
                .expect("the agent answered in time")
                .unwrap()
                .expect("the agent is still running");
            let message: Value = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("not JSON-RPC from the sandbox: {line:?}: {e}"));
            if message["id"] == id {
                return (message, notifications);
            }
            notifications.push(message);
        }
    }

    /// One whole ACP turn in `workspace`: the fixture writes hello.txt.
    pub async fn turn(&mut self, workspace: &Path) {
        let (initialized, _) = self
            .call(1, "initialize", json!({ "protocolVersion": 1 }))
            .await;
        assert_eq!(initialized["result"]["protocolVersion"], 1);
        let (session, _) = self
            .call(
                2,
                "session/new",
                json!({ "cwd": workspace, "mcpServers": [] }),
            )
            .await;
        assert_eq!(session["result"]["sessionId"], "s1");
        let (prompted, updates) = self
            .call(
                3,
                "session/prompt",
                json!({ "sessionId": "s1", "prompt": [{ "type": "text", "text": "write hello" }] }),
            )
            .await;
        assert_eq!(prompted["result"]["stopReason"], "end_turn");
        assert_eq!(updates[0]["params"]["update"]["content"]["text"], "done");
    }

    /// As an ACP client does at the end of a turn: close stdin, wait.
    pub async fn close(mut self) {
        drop(self.stdin.take());
        let status = tokio::time::timeout(Duration::from_secs(60), self.child.wait())
            .await
            .expect("the agent exits once its stdin closes")
            .unwrap();
        assert!(status.success(), "{status}");
    }
}
