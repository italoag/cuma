//! External sandbox plugins.
//!
//! A plugin is any executable. CUMA runs it with one argument — the
//! operation — writes a JSON request to its stdin and reads a JSON reply from
//! its stdout; exit status 0 means success.
//!
//! | Operation | Request | Reply |
//! |---|---|---|
//! | `probe` | `{options}` | `{isolation, workspace, network_allowlist, secrets_outside}` |
//! | `open` | `{workspace, purpose, keep_env, readable, state, allowed_hosts, options}` | `{prefix, session}` |
//! | `close` | `{session, collect}` | `{}` or `{result_dir}` |
//! | `release` | `{session}` | `{}` |
//! | `abort` | `{session}` | `{}` |
//!
//! A plugin that copies the workspace says so from `probe`
//! (`"workspace": "copied"`) and returns the sandbox's final copy from
//! `close`; CUMA records the workspace before `open` and does the three-way
//! merge itself, so every plugin gets the same conflict handling. `release`
//! follows the merge, for the plugin to remove that copy: CUMA never deletes
//! a path a plugin named.

use crate::process::{Input, Output, run};
use crate::sync::Snapshot;
use crate::{
    Capabilities, Isolation, LaunchRequest, SandboxLaunch, SandboxProvider, SandboxSession,
    WorkspaceAccess, failure,
};
use async_trait::async_trait;
use cuma_config::sandbox::PluginSandbox;
use cuma_core::error::Result;
use cuma_core::ports::LaunchPurpose;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// `kind = "plugin"`.
pub struct PluginProvider {
    name: String,
    settings: PluginSandbox,
    probed: OnceLock<Capabilities>,
}

#[derive(Debug, Deserialize)]
struct ProbeReply {
    #[serde(default)]
    isolation: Option<String>,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    network_allowlist: bool,
    #[serde(default)]
    secrets_outside: bool,
}

#[derive(Debug, Deserialize)]
struct OpenReply {
    prefix: Vec<String>,
    #[serde(default)]
    session: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct CloseReply {
    #[serde(default)]
    result_dir: Option<PathBuf>,
}

/// Calls into one plugin program.
#[derive(Clone)]
struct Program {
    sandbox: String,
    program: String,
    timeout: Duration,
}

impl Program {
    async fn call(&self, operation: &str, request: &Value) -> Result<Value> {
        let input = serde_json::to_vec(request).map_err(|e| {
            failure(
                &self.sandbox,
                format!("encoding the {operation} request: {e}"),
            )
        })?;
        let reply = run(
            &self.sandbox,
            &self.program,
            &[operation.to_owned()],
            Input::Bytes(&input),
            Output::Capture,
            self.timeout,
        )
        .await?;
        if reply.trim().is_empty() {
            return Ok(json!({}));
        }
        serde_json::from_str(&reply).map_err(|e| {
            failure(
                &self.sandbox,
                format!("the plugin's {operation} reply is not JSON: {e}"),
            )
        })
    }

    async fn parsed<T: for<'de> Deserialize<'de>>(
        &self,
        operation: &str,
        request: &Value,
    ) -> Result<T> {
        let reply = self.call(operation, request).await?;
        serde_json::from_value(reply).map_err(|e| {
            failure(
                &self.sandbox,
                format!("the plugin's {operation} reply: {e}"),
            )
        })
    }
}

impl PluginProvider {
    /// A provider for `[sandboxes.<name>]`.
    pub fn new(name: impl Into<String>, settings: PluginSandbox) -> Self {
        Self {
            name: name.into(),
            settings,
            probed: OnceLock::new(),
        }
    }

    fn program_handle(&self) -> Program {
        Program {
            sandbox: self.name.clone(),
            program: self.settings.program.clone(),
            timeout: Duration::from_secs(self.settings.timeout_secs),
        }
    }

    fn options(&self) -> Value {
        serde_json::to_value(&self.settings.options).unwrap_or_else(|_| json!({}))
    }

    /// What the plugin says about itself, asked once.
    async fn learn(&self) -> Result<Capabilities> {
        if let Some(capabilities) = self.probed.get() {
            return Ok(*capabilities);
        }
        let reply: ProbeReply = self
            .program_handle()
            .parsed("probe", &json!({ "options": self.options() }))
            .await?;
        let capabilities = Capabilities {
            isolation: reply
                .isolation
                .as_deref()
                .map_or(Isolation::Unknown, Isolation::from_name),
            workspace: match reply.workspace.as_deref() {
                Some("copied") => WorkspaceAccess::Copied,
                _ => WorkspaceAccess::Mounted,
            },
            network_allowlist: reply.network_allowlist,
            secrets_outside: reply.secrets_outside,
        };
        Ok(*self.probed.get_or_init(|| capabilities))
    }
}

#[async_trait]
impl SandboxProvider for PluginProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> &'static str {
        "plugin"
    }

    fn capabilities(&self) -> Capabilities {
        self.probed.get().copied().unwrap_or(Capabilities {
            isolation: Isolation::Unknown,
            workspace: WorkspaceAccess::Mounted,
            network_allowlist: false,
            secrets_outside: false,
        })
    }

    fn program(&self) -> String {
        self.settings.program.clone()
    }

    async fn probe(&self) -> Result<()> {
        self.learn().await.map(|_| ())
    }

    async fn open(&self, request: &LaunchRequest) -> Result<SandboxLaunch> {
        let capabilities = self.learn().await?;
        // Recorded before the plugin copies anything, so a file changed here
        // meanwhile is a conflict rather than silently reverted.
        let snapshot = if capabilities.workspace == WorkspaceAccess::Copied && request.collects() {
            Some(Snapshot::record_async(&request.workspace).await?)
        } else {
            None
        };

        let program = self.program_handle();
        let reply: OpenReply = program
            .parsed(
                "open",
                &json!({
                    "workspace": request.workspace,
                    "purpose": match request.purpose {
                        LaunchPurpose::Negotiate => "negotiate",
                        LaunchPurpose::Execute => "execute",
                    },
                    "keep_env": request.keep_env,
                    "readable": request.readable,
                    "state": request.state,
                    "allowed_hosts": request.allowed_hosts,
                    "options": self.options(),
                }),
            )
            .await?;
        if reply.prefix.first().is_none_or(|word| word.is_empty()) {
            return Err(failure(
                &self.name,
                "the plugin's open reply has an empty prefix",
            ));
        }
        let Some(session) = reply.session else {
            return Ok(SandboxLaunch::new(reply.prefix));
        };
        let keep = request
            .workspace
            .join(".cuma")
            .join("sandbox-results")
            .join(sanitize(&session));
        Ok(SandboxLaunch::with_session(
            reply.prefix,
            Arc::new(PluginSession {
                program,
                session,
                snapshot,
                keep,
            }),
        ))
    }
}

/// A session id, usable as a directory name.
fn sanitize(session: &str) -> String {
    let clean: String = session
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    if clean.is_empty() {
        "session".to_owned()
    } else {
        clean
    }
}

struct PluginSession {
    program: Program,
    session: String,
    snapshot: Option<Arc<Snapshot>>,
    keep: PathBuf,
}

#[async_trait]
impl SandboxSession for PluginSession {
    async fn finish(&self) -> Result<()> {
        let reply: CloseReply = self
            .program
            .parsed(
                "close",
                &json!({ "session": self.session, "collect": self.snapshot.is_some() }),
            )
            .await?;
        let Some(snapshot) = &self.snapshot else {
            return Ok(());
        };
        let result = reply.result_dir.ok_or_else(|| {
            failure(
                &self.program.sandbox,
                "the plugin copies the workspace but its close reply has no result_dir",
            )
        })?;
        let merged = Arc::clone(snapshot)
            .merge_async(result, self.keep.clone())
            .await;
        // The plugin removes its copy; CUMA never deletes a path a plugin
        // named.
        if let Err(err) = self
            .program
            .call("release", &json!({ "session": self.session }))
            .await
        {
            tracing::warn!(sandbox = %self.program.sandbox, error = %err, "the plugin's release failed");
        }
        let report = merged?;
        tracing::info!(sandbox = %self.program.sandbox, %report, "brought the agent's work back");
        Ok(())
    }

    async fn abort(&self) {
        if let Err(err) = self
            .program
            .call("abort", &json!({ "session": self.session }))
            .await
        {
            tracing::warn!(sandbox = %self.program.sandbox, error = %err, "the plugin's abort failed");
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    /// A plugin written in `sh`, logging each operation it receives.
    fn plugin(dir: &std::path::Path, body: &str) -> PathBuf {
        let path = dir.join("cuma-sandbox-test");
        std::fs::write(
            &path,
            format!("#!/bin/sh\nset -e\nlog={}/ops\n{body}", dir.display()),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn provider(program: &std::path::Path) -> PluginProvider {
        PluginProvider::new(
            "aos",
            PluginSandbox {
                program: program.display().to_string(),
                ..PluginSandbox::default()
            },
        )
    }

    #[tokio::test]
    async fn a_mounted_plugin_opens_with_its_prefix_and_is_closed_after() {
        let dir = tempfile::tempdir().unwrap();
        let program = plugin(
            dir.path(),
            r#"request=$(cat)
echo "$1 $request" >> "$log"
case "$1" in
  probe) echo '{"isolation":"wasm","workspace":"mounted"}' ;;
  open) echo '{"prefix":["my-vm","exec","s1","--"],"session":"s1"}' ;;
  close) echo '{}' ;;
esac
"#,
        );
        let p = provider(&program);
        let ws = tempfile::tempdir().unwrap();
        let mut request = LaunchRequest::bare(ws.path(), LaunchPurpose::Execute);
        request.keep_env = vec!["DEVIN_API_KEY".into()];

        let launch = p.open(&request).await.unwrap();
        assert_eq!(launch.prefix(), ["my-vm", "exec", "s1", "--"]);
        assert_eq!(p.capabilities().isolation, Isolation::Wasm);
        launch.finish().await.unwrap();

        let ops = std::fs::read_to_string(dir.path().join("ops")).unwrap();
        let lines: Vec<&str> = ops.lines().collect();
        assert!(lines[0].starts_with("probe "));
        assert!(lines[1].starts_with("open "));
        assert!(lines[1].contains("\"purpose\":\"execute\""), "{}", lines[1]);
        assert!(lines[1].contains("DEVIN_API_KEY"), "names travel");
        assert!(lines[2].contains("\"collect\":false"), "{}", lines[2]);
    }

    #[tokio::test]
    async fn a_copying_plugin_has_its_result_merged_back() {
        let dir = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("a.txt"), "before\n").unwrap();
        // The plugin "copies" the workspace into its own directory on open,
        // edits it as an agent would, and returns it on close.
        let copy = dir.path().join("copy");
        let program = plugin(
            dir.path(),
            &format!(
                r#"request=$(cat)
echo "$1" >> "$log"
case "$1" in
  probe) echo '{{"isolation":"microvm","workspace":"copied"}}' ;;
  open) mkdir -p {copy}; cp -R {ws}/. {copy}/; echo after > {copy}/a.txt
        echo '{{"prefix":["vm","--"],"session":"s/../1"}}' ;;
  close) echo '{{"result_dir":"{copy}"}}' ;;
  release) rm -rf {copy} ;;
esac
"#,
                copy = copy.display(),
                ws = ws.path().display()
            ),
        );
        let launch = provider(&program)
            .open(&LaunchRequest::bare(ws.path(), LaunchPurpose::Execute))
            .await
            .unwrap();
        launch.finish().await.unwrap();
        assert_eq!(
            std::fs::read_to_string(ws.path().join("a.txt")).unwrap(),
            "after\n"
        );
        let ops = std::fs::read_to_string(dir.path().join("ops")).unwrap();
        assert_eq!(
            ops.lines().collect::<Vec<_>>(),
            ["probe", "open", "close", "release"]
        );
        assert!(
            !copy.exists(),
            "the plugin removed its copy after the merge"
        );
    }

    #[tokio::test]
    async fn a_missing_plugin_fails_its_probe_and_a_bad_reply_is_an_error() {
        let p = provider(std::path::Path::new("/nonexistent/cuma-sandbox-x"));
        assert!(p.probe().await.is_err());

        let dir = tempfile::tempdir().unwrap();
        let program = plugin(dir.path(), "cat >/dev/null; echo 'not json'\n");
        let err = provider(&program).probe().await.unwrap_err().to_string();
        assert!(err.contains("not JSON"), "{err}");
    }

    #[test]
    fn a_session_id_cannot_climb_out_of_the_results_directory() {
        assert_eq!(sanitize("s/../1"), "s____1");
        assert_eq!(sanitize(""), "session");
    }
}
