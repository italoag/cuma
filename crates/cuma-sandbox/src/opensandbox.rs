//! OpenSandbox: a self-hosted sandbox platform with a lifecycle API and an
//! `execd` daemon in every sandbox.
//!
//! A sandbox is created from `image` (`POST /v1/sandboxes`,
//! `OPEN-SANDBOX-API-KEY`) with a keep-alive entrypoint, the workspace as a
//! host-path volume when `mount_workspace` is set, and the network allowlist
//! as its egress policy. `execd` is reached through the sandbox's endpoint
//! for its port, with the headers that endpoint names.
//!
//! `execd` runs commands and streams their output but cannot write to a
//! running process's stdin, which an ACP agent needs. So the bridge uploads a
//! small Node.js tunnel ([`TUNNEL`]), starts the agent behind it as a
//! background command, and relays stdio over the tunnel's endpoint: output as
//! server-sent events, input as ordered POSTs. The image must contain `node`.

use crate::bridge::{self, BridgeSession, Stdio, client, deliver, http_error, unb64};
use crate::sync::Snapshot;
use crate::{
    Capabilities, Isolation, LaunchRequest, SandboxLaunch, SandboxProvider, SandboxSession,
    WorkspaceAccess, failure, forwarded_env,
};
use async_trait::async_trait;
use cuma_config::sandbox::OpenSandboxSandbox;
use cuma_core::error::{MetaAgentError, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

/// The tunnel the bridge runs agents behind.
pub const TUNNEL: &str = include_str!("../assets/cuma-tunnel.mjs");
/// Where the tunnel is uploaded.
const TUNNEL_PATH: &str = "/tmp/cuma-tunnel.mjs";
/// `execd`'s port inside every sandbox.
const EXECD_PORT: u16 = 44772;

/// `kind = "opensandbox"`.
pub struct OpenSandboxProvider {
    name: String,
    settings: OpenSandboxSandbox,
    cuma: Option<PathBuf>,
}

/// A service inside a sandbox, as reachable from here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Endpoint {
    /// Its base URL.
    pub url: String,
    /// Headers every request to it must carry (`execd`'s access token).
    pub headers: BTreeMap<String, String>,
}

/// What the bridge needs to reach a sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// The lifecycle API, `…/v1`.
    pub server: String,
    /// The variable holding the API key — a handle.
    pub api_key_ref: Option<String>,
    /// The sandbox.
    pub sandbox_id: String,
    /// Its `execd`.
    pub execd: Endpoint,
    /// The tunnel's port inside the sandbox.
    pub tunnel_port: u16,
    /// Where the agent starts.
    pub cwd: Option<String>,
    /// Variables to forward, by name.
    pub env: Vec<String>,
}

/// The lifecycle API, shared by the provider and the bridge.
#[derive(Debug, Clone)]
struct Lifecycle {
    sandbox: String,
    server: String,
    api_key_ref: Option<String>,
    client: reqwest::Client,
}

impl Lifecycle {
    fn request(&self, method: reqwest::Method, path: &str) -> Result<reqwest::RequestBuilder> {
        let mut request = self.client.request(
            method,
            format!("{}/{path}", self.server.trim_end_matches('/')),
        );
        if let Some(handle) = &self.api_key_ref {
            let key = std::env::var(handle).map_err(|_| {
                MetaAgentError::Configuration(format!(
                    "sandbox {}: set {handle} to the API key (api_key_ref)",
                    self.sandbox
                ))
            })?;
            request = request.header("OPEN-SANDBOX-API-KEY", key);
        }
        Ok(request)
    }

    async fn send(
        &self,
        request: reqwest::RequestBuilder,
        what: &str,
    ) -> Result<reqwest::Response> {
        let response = request
            .send()
            .await
            .map_err(|e| failure(&self.sandbox, format!("{what}: {e}")))?;
        if response.status().is_success() {
            Ok(response)
        } else {
            Err(http_error(&self.sandbox, what, response).await)
        }
    }

    async fn json(&self, request: reqwest::RequestBuilder, what: &str) -> Result<Value> {
        self.send(request, what)
            .await?
            .json()
            .await
            .map_err(|e| failure(&self.sandbox, format!("{what}: {e}")))
    }

    /// Wait until sandbox `id` is running.
    async fn running(&self, id: &str, timeout: Duration) -> Result<()> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let sandbox = self
                .json(
                    self.request(reqwest::Method::GET, &format!("sandboxes/{id}"))?,
                    "reading the sandbox",
                )
                .await?;
            match sandbox["status"]["state"].as_str() {
                Some("Running") => return Ok(()),
                Some("Failed" | "Terminated" | "Stopping") => {
                    return Err(failure(
                        &self.sandbox,
                        format!("sandbox {id} failed: {}", sandbox["status"]["message"]),
                    ));
                }
                _ if std::time::Instant::now() > deadline => {
                    return Err(failure(
                        &self.sandbox,
                        format!("sandbox {id} did not start in time"),
                    ));
                }
                _ => tokio::time::sleep(Duration::from_millis(500)).await,
            }
        }
    }

    /// The endpoint for `port` inside sandbox `id`, proxied by the server.
    async fn endpoint(&self, id: &str, port: u16) -> Result<Endpoint> {
        let reply = self
            .json(
                self.request(
                    reqwest::Method::GET,
                    &format!("sandboxes/{id}/endpoints/{port}?use_server_proxy=true"),
                )?,
                "finding an endpoint",
            )
            .await?;
        let endpoint = reply["endpoint"]
            .as_str()
            .ok_or_else(|| failure(&self.sandbox, "the endpoint reply has no endpoint"))?;
        let url = if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
            endpoint.to_owned()
        } else {
            let scheme = if self.server.starts_with("https://") {
                "https"
            } else {
                "http"
            };
            format!("{scheme}://{endpoint}")
        };
        let headers = reply["headers"]
            .as_object()
            .map(|h| {
                h.iter()
                    .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_owned())))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Endpoint { url, headers })
    }

    async fn delete(&self, id: &str) -> Result<()> {
        let request = self.request(reqwest::Method::DELETE, &format!("sandboxes/{id}"))?;
        match request.send().await {
            Ok(response) if response.status().is_success() || response.status() == 404 => Ok(()),
            Ok(response) => Err(http_error(&self.sandbox, "deleting the sandbox", response).await),
            Err(e) => Err(failure(&self.sandbox, format!("deleting the sandbox: {e}"))),
        }
    }
}

impl Endpoint {
    fn request(
        &self,
        client: &reqwest::Client,
        method: reqwest::Method,
        path: &str,
    ) -> reqwest::RequestBuilder {
        let mut request =
            client.request(method, format!("{}/{path}", self.url.trim_end_matches('/')));
        for (name, value) in &self.headers {
            request = request.header(name, value);
        }
        request
    }
}

/// `execd`, through its endpoint.
struct Execd<'a> {
    sandbox: &'a str,
    endpoint: &'a Endpoint,
    client: &'a reqwest::Client,
}

impl Execd<'_> {
    /// Upload `bytes` as `path`.
    async fn upload(&self, path: &str, bytes: &[u8], mode: u32) -> Result<()> {
        let boundary = format!("cuma-{}", uuid::Uuid::new_v4().simple());
        let metadata = json!({ "path": path, "mode": mode }).to_string();
        let mut body = Vec::with_capacity(bytes.len() + 512);
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"metadata\"\r\n\
                 Content-Type: application/json\r\n\r\n{metadata}\r\n\
                 --{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"upload\"\r\n\
                 Content-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let response = self
            .endpoint
            .request(self.client, reqwest::Method::POST, "files/upload")
            .header(
                "Content-Type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(body)
            .send()
            .await
            .map_err(|e| failure(self.sandbox, format!("uploading {path}: {e}")))?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(http_error(self.sandbox, &format!("uploading {path}"), response).await)
        }
    }

    /// Download `path` into `dest`.
    async fn download(&self, path: &str, dest: &Path) -> Result<()> {
        let url = bridge::with_query(
            &format!("{}/files/download", self.endpoint.url.trim_end_matches('/')),
            &[("path", path)],
        )?;
        let mut request = self.client.get(url);
        for (name, value) in &self.endpoint.headers {
            request = request.header(name, value);
        }
        let mut response = request
            .send()
            .await
            .map_err(|e| failure(self.sandbox, format!("downloading {path}: {e}")))?;
        if !response.status().is_success() {
            return Err(http_error(self.sandbox, &format!("downloading {path}"), response).await);
        }
        let mut file = tokio::fs::File::create(dest)
            .await
            .map_err(|e| failure(self.sandbox, format!("{}: {e}", dest.display())))?;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| failure(self.sandbox, format!("downloading {path}: {e}")))?
        {
            file.write_all(&chunk)
                .await
                .map_err(|e| failure(self.sandbox, format!("{}: {e}", dest.display())))?;
        }
        Ok(())
    }

    /// Run `command`. In the foreground it is awaited and must succeed; in
    /// the background, once execd has accepted it.
    async fn command(
        &self,
        command: &str,
        cwd: Option<&str>,
        background: bool,
        envs: Value,
    ) -> Result<()> {
        let mut request = json!({ "command": command, "background": background, "envs": envs });
        if let Some(cwd) = cwd {
            request["cwd"] = json!(cwd);
        }
        let mut response = self
            .endpoint
            .request(self.client, reqwest::Method::POST, "command")
            .json(&request)
            .send()
            .await
            .map_err(|e| failure(self.sandbox, format!("running a command: {e}")))?;
        if !response.status().is_success() {
            return Err(http_error(self.sandbox, "running a command", response).await);
        }
        let mut text = String::new();
        let mut id = None;
        let mut errors = Vec::new();
        let mut stderr = String::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| failure(self.sandbox, format!("reading command output: {e}")))?
        {
            text.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(newline) = text.find('\n') {
                let line: String = text.drain(..=newline).collect();
                let Some(event) = command_event(&line) else {
                    continue;
                };
                match event["type"].as_str() {
                    Some("init") => id = event["text"].as_str().map(str::to_owned),
                    Some("stderr") => stderr.push_str(event["text"].as_str().unwrap_or_default()),
                    Some("error") => errors.push(event["error"].to_string()),
                    _ => {}
                }
            }
            if background && id.is_some() {
                return Ok(());
            }
        }
        if !errors.is_empty() {
            return Err(failure(
                self.sandbox,
                format!(
                    "`{command}` failed: {} {}",
                    errors.join("; "),
                    stderr.trim()
                ),
            ));
        }
        let Some(id) = id else {
            return if background {
                Ok(())
            } else {
                Err(failure(
                    self.sandbox,
                    format!("`{command}`: execd reported no command id"),
                ))
            };
        };
        if background {
            return Ok(());
        }
        let status: Value = self
            .endpoint
            .request(
                self.client,
                reqwest::Method::GET,
                &format!("command/status/{id}"),
            )
            .send()
            .await
            .map_err(|e| failure(self.sandbox, format!("reading command status: {e}")))?
            .json()
            .await
            .map_err(|e| failure(self.sandbox, format!("reading command status: {e}")))?;
        match status["exit_code"].as_i64() {
            Some(0) | None => Ok(()),
            Some(code) => Err(failure(
                self.sandbox,
                format!("`{command}` exited with {code}: {}", stderr.trim()),
            )),
        }
    }
}

/// One event from execd's command stream: an SSE `data:` line or a bare
/// JSON line.
fn command_event(line: &str) -> Option<Value> {
    let line = line.trim();
    let payload = line.strip_prefix("data:").map_or(line, str::trim);
    if payload.is_empty() {
        return None;
    }
    serde_json::from_str(payload).ok()
}

impl OpenSandboxProvider {
    /// A provider for `[sandboxes.<name>]`; `cuma` is CUMA's executable.
    pub fn new(
        name: impl Into<String>,
        settings: OpenSandboxSandbox,
        cuma: Option<PathBuf>,
    ) -> Self {
        Self {
            name: name.into(),
            settings,
            cuma,
        }
    }

    fn lifecycle(&self) -> Result<Lifecycle> {
        Ok(Lifecycle {
            sandbox: self.name.clone(),
            server: format!("{}/v1", self.settings.server_url.trim_end_matches('/')),
            api_key_ref: self.settings.api_key_ref.clone(),
            client: client()?,
        })
    }

    /// The create request for one launch.
    pub fn create_body(&self, request: &LaunchRequest) -> Value {
        let s = &self.settings;
        let mut body = json!({
            "image": { "uri": s.image },
            "entrypoint": ["tail", "-f", "/dev/null"],
            "resourceLimits": { "cpu": s.cpu, "memory": s.memory },
            "timeout": s.timeout_secs,
            // Metadata values are labels: letters, digits, `-`, `_`, `.`.
            "metadata": { "managed-by": "cuma", "cuma-sandbox": self.name },
        });
        if s.mount_workspace {
            let workspace = crate::canonical(&request.workspace);
            let mut volumes = vec![json!({
                "name": "workspace",
                "host": { "path": workspace },
                "mountPath": workspace,
            })];
            if let Some(common) = cuma_workspace::confine::git_common_dir(&request.workspace) {
                let common = common.display().to_string();
                volumes.push(
                    json!({ "name": "git", "host": { "path": common }, "mountPath": common }),
                );
            }
            body["volumes"] = json!(volumes);
        }
        if !request.allowed_hosts.is_empty() {
            let egress: Vec<Value> = request
                .allowed_hosts
                .iter()
                .map(|host| json!({ "action": "allow", "target": host }))
                .collect();
            body["networkPolicy"] = json!({ "defaultAction": "deny", "egress": egress });
        }
        body
    }
}

#[async_trait]
impl SandboxProvider for OpenSandboxProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> &'static str {
        "opensandbox"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            isolation: Isolation::Container,
            workspace: if self.settings.mount_workspace {
                WorkspaceAccess::Mounted
            } else {
                WorkspaceAccess::Copied
            },
            network_allowlist: true,
            secrets_outside: false,
        }
    }

    fn program(&self) -> String {
        self.cuma
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "cuma".to_owned())
    }

    async fn probe(&self) -> Result<()> {
        let lifecycle = self.lifecycle()?;
        let here = std::env::current_dir().unwrap_or_default();
        let mut body = self.create_body(&LaunchRequest::bare(
            here,
            cuma_core::ports::LaunchPurpose::Negotiate,
        ));
        body.as_object_mut().map(|b| b.remove("volumes"));
        let created = lifecycle
            .json(
                lifecycle
                    .request(reqwest::Method::POST, "sandboxes")?
                    .json(&body),
                "creating a sandbox",
            )
            .await?;
        let id = created["id"].as_str().unwrap_or_default().to_owned();
        let ran = async {
            lifecycle.running(&id, Duration::from_secs(300)).await?;
            let execd = lifecycle.endpoint(&id, EXECD_PORT).await?;
            Execd {
                sandbox: &self.name,
                endpoint: &execd,
                client: &lifecycle.client,
            }
            .command("node --version", None, false, json!({}))
            .await
        }
        .await;
        let deleted = lifecycle.delete(&id).await;
        ran?;
        deleted
    }

    async fn open(&self, request: &LaunchRequest) -> Result<SandboxLaunch> {
        let Some(cuma) = self.cuma.clone() else {
            return Err(MetaAgentError::Configuration(format!(
                "sandbox {}: agents in an OpenSandbox sandbox are launched through cuma itself, whose path is unknown",
                self.name
            )));
        };
        let lifecycle = self.lifecycle()?;
        let created = lifecycle
            .json(
                lifecycle
                    .request(reqwest::Method::POST, "sandboxes")?
                    .json(&self.create_body(request)),
                "creating a sandbox",
            )
            .await?;
        let id = created["id"]
            .as_str()
            .ok_or_else(|| failure(&self.name, "the create reply has no id"))?
            .to_owned();
        let workspace = crate::canonical(&request.workspace);
        let work = tempfile::Builder::new()
            .prefix("cuma-osb-")
            .tempdir()
            .map_err(|e| failure(&self.name, e))?;
        let keep = request
            .workspace
            .join(".cuma")
            .join("sandbox-results")
            .join(&id);

        let populated = async {
            lifecycle.running(&id, Duration::from_secs(300)).await?;
            let execd_endpoint = lifecycle.endpoint(&id, EXECD_PORT).await?;
            let execd = Execd { sandbox: &self.name, endpoint: &execd_endpoint, client: &lifecycle.client };
            execd.upload(TUNNEL_PATH, TUNNEL.as_bytes(), 0o644).await?;
            let copied = request.collects() && !self.settings.mount_workspace;
            let snapshot = if copied {
                let snapshot = Snapshot::take_async(&request.workspace, work.path()).await?;
                if let Some(archive) = snapshot.archive() {
                    let bytes = tokio::fs::read(archive)
                        .await
                        .map_err(|e| failure(&self.name, format!("reading the archive: {e}")))?;
                    execd.upload("/tmp/cuma-ws.tar", &bytes, 0o600).await?;
                }
                execd
                    .command(
                        &format!(
                            "mkdir -p {ws} && tar -xf /tmp/cuma-ws.tar -C {ws} && rm -f /tmp/cuma-ws.tar",
                            ws = shell_words::quote(&workspace)
                        ),
                        None,
                        false,
                        json!({}),
                    )
                    .await?;
                Some(snapshot)
            } else {
                None
            };
            let session = Session {
                server: lifecycle.server.clone(),
                api_key_ref: lifecycle.api_key_ref.clone(),
                sandbox_id: id.clone(),
                execd: execd_endpoint.clone(),
                tunnel_port: self.settings.tunnel_port,
                cwd: (request.collects()).then(|| workspace.clone()),
                env: forwarded_env(request),
            };
            let file = bridge::write_session(work.path(), &BridgeSession::Opensandbox(session.clone()))?;
            Ok::<_, MetaAgentError>((snapshot, file, session))
        }
        .await;

        match populated {
            Ok((snapshot, file, session)) => Ok(SandboxLaunch::with_session(
                bridge::prefix(&cuma, &file),
                Arc::new(Remote {
                    name: self.name.clone(),
                    lifecycle,
                    session,
                    workspace,
                    snapshot,
                    work,
                    keep,
                }),
            )),
            Err(err) => {
                if let Err(delete) = lifecycle.delete(&id).await {
                    tracing::warn!(sandbox = %id, error = %delete, "deleting the sandbox failed");
                }
                Err(err)
            }
        }
    }
}

/// One launch's sandbox.
struct Remote {
    name: String,
    lifecycle: Lifecycle,
    session: Session,
    workspace: String,
    snapshot: Option<Arc<Snapshot>>,
    work: tempfile::TempDir,
    keep: PathBuf,
}

impl Remote {
    async fn collect(&self, snapshot: &Arc<Snapshot>) -> Result<()> {
        let execd = Execd {
            sandbox: &self.name,
            endpoint: &self.session.execd,
            client: &self.lifecycle.client,
        };
        execd
            .command(
                &format!(
                    "tar -cf /tmp/cuma-out.tar -C {} .",
                    shell_words::quote(&self.workspace)
                ),
                None,
                false,
                json!({}),
            )
            .await?;
        let local = self.work.path().join("out.tar");
        execd.download("/tmp/cuma-out.tar", &local).await?;
        let report = Arc::clone(snapshot)
            .merge_archive_async(local, self.work.path().join("result"), self.keep.clone())
            .await?;
        tracing::info!(sandbox = %self.name, %report, "brought the agent's work back");
        Ok(())
    }

    async fn delete(&self) {
        if let Err(err) = self.lifecycle.delete(&self.session.sandbox_id).await {
            tracing::warn!(sandbox = %self.session.sandbox_id, error = %err, "deleting the sandbox failed");
        }
    }
}

#[async_trait]
impl SandboxSession for Remote {
    async fn finish(&self) -> Result<()> {
        let collected = match &self.snapshot {
            Some(snapshot) => self.collect(snapshot).await,
            None => Ok(()),
        };
        self.delete().await;
        collected
    }

    async fn abort(&self) {
        self.delete().await;
    }
}

/// Start `argv` behind the tunnel and relay `io` to it until it exits.
pub async fn relay<I, O, E>(session: &Session, argv: &[String], io: Stdio<I, O, E>) -> Result<i32>
where
    I: AsyncRead + Unpin + Send + 'static,
    O: AsyncWrite + Unpin,
    E: AsyncWrite + Unpin,
{
    let lifecycle = Lifecycle {
        sandbox: session.sandbox_id.clone(),
        server: session.server.clone(),
        api_key_ref: session.api_key_ref.clone(),
        client: client()?,
    };
    let execd = Execd {
        sandbox: &session.sandbox_id,
        endpoint: &session.execd,
        client: &lifecycle.client,
    };
    let envs: serde_json::Map<String, Value> = session
        .env
        .iter()
        .filter_map(|name| std::env::var(name).ok().map(|v| (name.clone(), json!(v))))
        .collect();
    let command = format!(
        "exec node {TUNNEL_PATH} {} -- {}",
        session.tunnel_port,
        shell_words::join(argv)
    );
    execd
        .command(&command, session.cwd.as_deref(), true, Value::Object(envs))
        .await?;

    // The tunnel needs a moment to listen.
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let tunnel = loop {
        if let Ok(endpoint) = lifecycle
            .endpoint(&session.sandbox_id, session.tunnel_port)
            .await
            && endpoint
                .request(&lifecycle.client, reqwest::Method::GET, "health")
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
        {
            break endpoint;
        }
        if std::time::Instant::now() > deadline {
            return Err(failure(
                &session.sandbox_id,
                "the stdio tunnel did not start; is node in the image?",
            ));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };

    let mut stream = tunnel
        .request(&lifecycle.client, reqwest::Method::GET, "stdout")
        .send()
        .await
        .map_err(|e| failure(&session.sandbox_id, format!("attaching to the agent: {e}")))?;
    if !stream.status().is_success() {
        return Err(http_error(&session.sandbox_id, "attaching to the agent", stream).await);
    }

    let Stdio {
        input,
        mut output,
        errors: _,
    } = io;
    let (client, endpoint) = (lifecycle.client.clone(), tunnel.clone());
    let feeding = tokio::spawn(async move {
        let sequence = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let (c1, e1, s1) = (client.clone(), endpoint.clone(), Arc::clone(&sequence));
        bridge::pump(
            input,
            move |chunk| {
                let (client, endpoint, sequence) = (c1.clone(), e1.clone(), Arc::clone(&s1));
                async move { push(&client, &endpoint, &sequence, chunk, false).await }
            },
            move || async move { push(&client, &endpoint, &sequence, Vec::new(), true).await },
        )
        .await
    });

    let mut text = String::new();
    let outcome = loop {
        let chunk = match stream.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => {
                break Err(failure(
                    &session.sandbox_id,
                    "the agent's output ended without an exit",
                ));
            }
            Err(e) => {
                break Err(failure(
                    &session.sandbox_id,
                    format!("reading the agent's output: {e}"),
                ));
            }
        };
        text.push_str(&String::from_utf8_lossy(&chunk));
        let mut exit = None;
        while let Some(end) = text.find("\n\n") {
            let block: String = text.drain(..end + 2).collect();
            let (mut event, mut data) = ("message", String::new());
            for line in block.lines() {
                if let Some(value) = line.strip_prefix("event:") {
                    event = if value.trim() == "exit" {
                        "exit"
                    } else {
                        "data"
                    };
                } else if let Some(value) = line.strip_prefix("data:") {
                    data.push_str(value.trim());
                }
            }
            if event == "exit" {
                exit = Some(data.parse::<i32>().unwrap_or(1));
                break;
            }
            // Comments and keep-alives carry no data.
            if data.is_empty() {
                continue;
            }
            let delivered = match unb64(&data) {
                Ok(bytes) => deliver(&mut output, &bytes).await,
                Err(err) => Err(err),
            };
            if let Err(err) = delivered {
                feeding.abort();
                return Err(err);
            }
        }
        if let Some(code) = exit {
            break Ok(code);
        }
    };
    feeding.abort();
    outcome
}

/// Send one stdin chunk, numbered so the tunnel applies them in order.
async fn push(
    client: &reqwest::Client,
    endpoint: &Endpoint,
    sequence: &std::sync::atomic::AtomicU64,
    chunk: Vec<u8>,
    eof: bool,
) -> Result<()> {
    let seq = sequence.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let mut request = endpoint
        .request(client, reqwest::Method::POST, "stdin")
        .header("x-seq", seq.to_string())
        .body(chunk);
    if eof {
        request = request.header("x-eof", "1");
    }
    let response = request
        .send()
        .await
        .map_err(|e| failure("opensandbox", format!("writing to the agent: {e}")))?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(http_error("opensandbox", "writing to the agent", response).await)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use axum::body::{Body, Bytes};
    use axum::extract::{Path as UrlPath, State};
    use axum::http::{HeaderMap, Uri};
    use axum::routing::{get, post};
    use cuma_core::ports::LaunchPurpose;
    use std::collections::HashMap;
    use std::sync::Mutex;

    fn query(uri: &Uri) -> HashMap<String, String> {
        reqwest::Url::parse(&format!("http://fake{uri}"))
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect()
    }

    #[test]
    fn the_sandbox_mounts_the_workspace_and_enforces_the_allowlist() {
        let ws = tempfile::tempdir().unwrap();
        let path = crate::canonical(ws.path());
        let provider = OpenSandboxProvider::new(
            "osb",
            OpenSandboxSandbox {
                server_url: "http://localhost:8080".into(),
                image: "node:22".into(),
                mount_workspace: true,
                ..OpenSandboxSandbox::default()
            },
            None,
        );
        let mut request = LaunchRequest::bare(ws.path(), LaunchPurpose::Execute);
        request.allowed_hosts = vec!["api.anthropic.com".into()];
        let body = provider.create_body(&request);
        assert_eq!(body["image"]["uri"], "node:22");
        assert_eq!(body["entrypoint"], json!(["tail", "-f", "/dev/null"]));
        // The server holds metadata values to label syntax.
        for value in body["metadata"].as_object().unwrap().values() {
            let value = value.as_str().unwrap();
            assert!(
                value.len() <= 63
                    && value
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
                    && value.starts_with(|c: char| c.is_ascii_alphanumeric())
                    && value.ends_with(|c: char| c.is_ascii_alphanumeric()),
                "{value:?} is not a valid label value"
            );
        }
        assert_eq!(
            body["resourceLimits"],
            json!({ "cpu": "1", "memory": "2Gi" })
        );
        assert_eq!(body["volumes"][0]["host"]["path"], path);
        assert_eq!(body["volumes"][0]["mountPath"], path);
        assert_eq!(
            body["networkPolicy"],
            json!({ "defaultAction": "deny", "egress": [{ "action": "allow", "target": "api.anthropic.com" }] })
        );
        assert_eq!(provider.capabilities().workspace, WorkspaceAccess::Mounted);
    }

    #[test]
    fn command_events_are_read_from_sse_or_bare_json() {
        assert_eq!(
            command_event("data: {\"type\":\"init\",\"text\":\"c1\"}").unwrap()["text"],
            "c1"
        );
        assert_eq!(
            command_event("{\"type\":\"stdout\"}").unwrap()["type"],
            "stdout"
        );
        assert!(command_event("").is_none());
        assert!(command_event(": keep-alive").is_none());
    }

    /// A stand-in for the lifecycle API and execd that runs commands on this
    /// machine, with `/tmp/` re-rooted in its own directory — so the real
    /// tunnel script runs, under real node.
    #[derive(Clone)]
    struct Fake {
        root: PathBuf,
        tunnel_port: u16,
        deleted: Arc<Mutex<Vec<String>>>,
        exits: Arc<Mutex<HashMap<String, i32>>>,
        /// Background commands: killed when the test ends, so none outlives
        /// it holding a port, or the test runner waiting.
        background: Arc<Mutex<Vec<std::process::Child>>>,
    }

    impl Fake {
        fn stop_background(&self) {
            for mut child in self.background.lock().unwrap().drain(..) {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    fn rerooted(fake: &Fake, text: &str) -> String {
        text.replace("/tmp/", &format!("{}/tmp/", fake.root.display()))
    }

    fn multipart_parts(headers: &HeaderMap, body: &[u8]) -> HashMap<String, Vec<u8>> {
        let ct = headers["content-type"].to_str().unwrap();
        let boundary = format!("--{}", ct.split("boundary=").nth(1).unwrap());
        let text = body;
        let mut parts = HashMap::new();
        let mut rest = text;
        while let Some(start) = find(rest, boundary.as_bytes()) {
            rest = &rest[start + boundary.len()..];
            if rest.starts_with(b"--") {
                break;
            }
            let header_end = find(rest, b"\r\n\r\n").unwrap();
            let head = String::from_utf8_lossy(&rest[..header_end]).to_string();
            let content_start = header_end + 4;
            let next = find(&rest[content_start..], boundary.as_bytes()).unwrap();
            let content = &rest[content_start..content_start + next - 2];
            let name = head
                .split("name=\"")
                .nth(1)
                .unwrap()
                .split('"')
                .next()
                .unwrap()
                .to_owned();
            parts.insert(name, content.to_vec());
            rest = &rest[content_start + next..];
        }
        parts
    }

    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    async fn serve(fake: Fake) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new()
            .route("/v1/sandboxes", post(|| async { axum::Json(json!({ "id": "osb1", "status": { "state": "Running" } })) }))
            .route(
                "/v1/sandboxes/{id}",
                get(|| async { axum::Json(json!({ "id": "osb1", "status": { "state": "Running" } })) }).delete(
                    |State(f): State<Fake>, UrlPath(id): UrlPath<String>| async move {
                        f.deleted.lock().unwrap().push(id);
                        axum::http::StatusCode::NO_CONTENT
                    },
                ),
            )
            .route(
                "/v1/sandboxes/{id}/endpoints/{port}",
                get(move |State(f): State<Fake>, UrlPath((_, port)): UrlPath<(String, u16)>| async move {
                    if port == EXECD_PORT {
                        axum::Json(json!({ "endpoint": address.to_string(), "headers": { "X-EXECD-ACCESS-TOKEN": "t" } }))
                    } else {
                        assert_eq!(port, f.tunnel_port);
                        axum::Json(json!({ "endpoint": format!("127.0.0.1:{port}") }))
                    }
                }),
            )
            .route(
                "/files/upload",
                post(|State(f): State<Fake>, headers: HeaderMap, body: Bytes| async move {
                    assert_eq!(headers["x-execd-access-token"], "t");
                    let parts = multipart_parts(&headers, &body);
                    let metadata: Value = serde_json::from_slice(&parts["metadata"]).unwrap();
                    let path = rerooted(&f, metadata["path"].as_str().unwrap());
                    std::fs::create_dir_all(Path::new(&path).parent().unwrap()).unwrap();
                    std::fs::write(&path, &parts["file"]).unwrap();
                    axum::http::StatusCode::OK
                }),
            )
            .route(
                "/files/download",
                get(|State(f): State<Fake>, uri: Uri| async move {
                    std::fs::read(rerooted(&f, &query(&uri)["path"])).unwrap()
                }),
            )
            .route(
                "/command",
                post(|State(f): State<Fake>, body: axum::Json<Value>| async move {
                    let command = rerooted(&f, body["command"].as_str().unwrap());
                    let mut process = std::process::Command::new("sh");
                    process.arg("-c").arg(&command);
                    if let Some(cwd) = body["cwd"].as_str() {
                        process.current_dir(cwd);
                    }
                    let id = format!("cmd-{}", f.exits.lock().unwrap().len());
                    let mut events = format!("data: {}\n\n", json!({ "type": "init", "text": id }));
                    if body["background"] == true {
                        // Not the test's own stdio: a pipe it held would keep
                        // the test runner waiting for it.
                        let child = process
                            .stdin(std::process::Stdio::null())
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .spawn()
                            .unwrap();
                        f.background.lock().unwrap().push(child);
                        f.exits.lock().unwrap().insert(id, 0);
                    } else {
                        let out = process.output().unwrap();
                        let code = out.status.code().unwrap_or(1);
                        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
                        events.push_str(&format!("data: {}\n\n", json!({ "type": "stderr", "text": stderr })));
                        events.push_str(&format!("data: {}\n\n", json!({ "type": "execution_complete" })));
                        f.exits.lock().unwrap().insert(id, code);
                    }
                    Body::from(events)
                }),
            )
            .route(
                "/command/status/{id}",
                get(|State(f): State<Fake>, UrlPath(id): UrlPath<String>| async move {
                    axum::Json(json!({ "exit_code": f.exits.lock().unwrap()[&id] }))
                }),
            )
            .with_state(fake);
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{address}")
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    #[tokio::test]
    async fn an_agent_runs_behind_the_tunnel_and_a_copied_workspace_round_trips() {
        if which::which("node").is_err() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("tmp")).unwrap();
        let fake = Fake {
            root: root.path().to_path_buf(),
            tunnel_port: free_port(),
            deleted: Arc::default(),
            exits: Arc::default(),
            background: Arc::default(),
        };
        // Whatever happens below, nothing it started survives it.
        struct StopOnDrop(Fake);
        impl Drop for StopOnDrop {
            fn drop(&mut self) {
                self.0.stop_background();
            }
        }
        let _stop = StopOnDrop(fake.clone());
        let url = serve(fake.clone()).await;
        // The stand-in runs everything on this machine, so its workspace is
        // the real one: the agent's edit lands directly, and the test checks
        // the upload, the stdio round trip, the collection and the deletion
        // (the merge itself is `sync`'s, tested there).
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("notes.txt"), "v1\n").unwrap();
        let provider = OpenSandboxProvider::new(
            "osb",
            OpenSandboxSandbox {
                server_url: url,
                image: "node:22".into(),
                tunnel_port: fake.tunnel_port,
                ..OpenSandboxSandbox::default()
            },
            Some(PathBuf::from("/usr/local/bin/cuma")),
        );

        let launch = provider
            .open(&LaunchRequest::bare(ws.path(), LaunchPurpose::Execute))
            .await
            .unwrap();
        assert!(
            root.path().join("tmp/cuma-tunnel.mjs").exists(),
            "the tunnel was uploaded"
        );

        // What `cuma sandbox exec` does with the prefix's session file.
        let saved: BridgeSession =
            serde_json::from_str(&std::fs::read_to_string(&launch.prefix()[4]).unwrap()).unwrap();
        let BridgeSession::Opensandbox(session) = saved else {
            panic!("opensandbox")
        };
        let input: &'static [u8] = b"{\"jsonrpc\":\"2.0\",\"method\":\"initialize\"}\n";
        let mut output = Vec::new();
        let mut errors = Vec::new();
        let code = relay(
            &session,
            &[
                "sh".to_owned(),
                "-c".to_owned(),
                "cat; echo changed > notes.txt".to_owned(),
            ],
            Stdio {
                input,
                output: &mut output,
                errors: &mut errors,
            },
        )
        .await
        .unwrap();
        assert_eq!(code, 0);
        assert_eq!(output, input, "stdin came back through the tunnel");

        launch.finish().await.unwrap();
        assert_eq!(
            std::fs::read_to_string(ws.path().join("notes.txt")).unwrap(),
            "changed\n"
        );
        assert_eq!(*fake.deleted.lock().unwrap(), ["osb1"]);
    }
}
