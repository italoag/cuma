//! The E2B API: CubeSandbox, and E2B itself.
//!
//! A sandbox is created from a template through the lifecycle API
//! (`POST /sandboxes`, `X-API-Key`) and reached at
//! `<scheme>://<port>-<sandbox>.<domain>`, where `envd` runs. `envd` speaks
//! Connect: `process.Process/Start` is a server stream of start, output and
//! end events; `SendInput` and `CloseStdin` feed the process's stdin; files
//! move through `/files`. Requests carry the sandbox's access token and the
//! user they act as.
//!
//! The workspace is copied: uploaded as an archive and unpacked as root
//! (agents' default user cannot create the workspace's absolute path), then
//! archived and downloaded for the merge. The agent runs through CUMA's
//! bridge.

use crate::bridge::{self, BridgeSession, Stdio, b64, client, deliver, http_error, unb64};
use crate::sync::Snapshot;
use crate::{
    Capabilities, Isolation, LaunchRequest, SandboxLaunch, SandboxProvider, SandboxSession,
    WorkspaceAccess, failure, forwarded_env,
};
use async_trait::async_trait;
use cuma_config::sandbox::E2bSandbox;
use cuma_core::error::{MetaAgentError, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

/// `kind = "e2b"`.
pub struct E2bProvider {
    name: String,
    settings: E2bSandbox,
    cuma: Option<PathBuf>,
    envd_override: Option<String>,
}

/// What the bridge needs to reach a sandbox's `envd`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// `envd`'s base URL.
    pub envd: String,
    /// The access token `envd` requires, if the sandbox is secured.
    pub token: Option<String>,
    /// The sandbox.
    pub sandbox_id: String,
    /// `envd`'s port.
    pub port: u16,
    /// The user processes run as.
    pub user: String,
    /// Where the agent starts.
    pub cwd: Option<String>,
    /// Variables to forward, by name.
    pub env: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Created {
    #[serde(rename = "sandboxID")]
    sandbox_id: String,
    #[serde(rename = "envdAccessToken", default)]
    envd_access_token: Option<String>,
    #[serde(default)]
    domain: Option<String>,
}

impl E2bProvider {
    /// A provider for `[sandboxes.<name>]`; `cuma` is CUMA's executable,
    /// which agents are launched through.
    pub fn new(name: impl Into<String>, settings: E2bSandbox, cuma: Option<PathBuf>) -> Self {
        Self {
            name: name.into(),
            settings,
            cuma,
            envd_override: None,
        }
    }

    /// Reach `envd` at `url` rather than through the domain — for tests.
    #[must_use]
    pub fn with_envd_url(mut self, url: impl Into<String>) -> Self {
        self.envd_override = Some(url.into());
        self
    }

    fn api_key(&self) -> Result<Option<String>> {
        let Some(handle) = &self.settings.api_key_ref else {
            return Ok(None);
        };
        std::env::var(handle).map(Some).map_err(|_| {
            MetaAgentError::Configuration(format!(
                "sandbox {}: set {handle} to the API key (api_key_ref)",
                self.name
            ))
        })
    }

    fn api(&self, path: &str) -> String {
        format!("{}/{path}", self.settings.api_url.trim_end_matches('/'))
    }

    /// Where `envd` answers for a sandbox.
    fn envd_url(&self, created: &Created) -> String {
        if let Some(url) = &self.envd_override {
            return url.clone();
        }
        let domain = created
            .domain
            .as_deref()
            .filter(|d| !d.is_empty())
            .unwrap_or(&self.settings.domain);
        format!(
            "{}://{}-{}.{domain}",
            self.settings.envd_scheme, self.settings.envd_port, created.sandbox_id
        )
    }

    async fn create(&self, client: &reqwest::Client) -> Result<Created> {
        let mut request = client.post(self.api("sandboxes")).json(&json!({
            "templateID": self.settings.template,
            "timeout": self.settings.timeout_secs,
            "secure": true,
            "allow_internet_access": self.settings.internet,
            "metadata": { "cuma.sandbox": self.name },
        }));
        if let Some(key) = self.api_key()? {
            request = request.header("X-API-Key", key);
        }
        let response = request
            .send()
            .await
            .map_err(|e| failure(&self.name, format!("creating a sandbox: {e}")))?;
        if !response.status().is_success() {
            return Err(http_error(&self.name, "creating a sandbox", response).await);
        }
        response
            .json()
            .await
            .map_err(|e| failure(&self.name, format!("the create reply: {e}")))
    }

    async fn kill(&self, client: &reqwest::Client, id: &str) -> Result<()> {
        let mut request = client.delete(self.api(&format!("sandboxes/{id}")));
        if let Some(key) = self.api_key()? {
            request = request.header("X-API-Key", key);
        }
        let response = request
            .send()
            .await
            .map_err(|e| failure(&self.name, format!("killing sandbox {id}: {e}")))?;
        if response.status().is_success() || response.status() == reqwest::StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(http_error(&self.name, "killing the sandbox", response).await)
        }
    }

    fn clone_handle(&self) -> Self {
        Self {
            name: self.name.clone(),
            settings: self.settings.clone(),
            cuma: self.cuma.clone(),
            envd_override: self.envd_override.clone(),
        }
    }
}

impl Session {
    /// Headers every `envd` request carries.
    fn authorize(&self, request: reqwest::RequestBuilder, user: &str) -> reqwest::RequestBuilder {
        let mut request = request
            .header("E2b-Sandbox-Id", &self.sandbox_id)
            .header("E2b-Sandbox-Port", self.port.to_string())
            .header(
                "Authorization",
                format!("Basic {}", b64(format!("{user}:").as_bytes())),
            );
        if let Some(token) = &self.token {
            request = request.header("X-Access-Token", token);
        }
        request
    }

    fn rpc(&self, method: &str) -> String {
        format!(
            "{}/process.Process/{method}",
            self.envd.trim_end_matches('/')
        )
    }

    fn files(&self, path: &str, user: &str) -> Result<reqwest::Url> {
        bridge::with_query(
            &format!("{}/files", self.envd.trim_end_matches('/')),
            &[("path", path), ("username", user)],
        )
    }

    /// Upload `bytes` to `path` in the sandbox.
    async fn upload(
        &self,
        client: &reqwest::Client,
        path: &str,
        bytes: Vec<u8>,
        user: &str,
    ) -> Result<()> {
        let response = self
            .authorize(client.post(self.files(path, user)?), user)
            .header("Content-Type", "application/octet-stream")
            .body(bytes)
            .send()
            .await
            .map_err(|e| failure(&self.sandbox_id, format!("uploading {path}: {e}")))?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(http_error(&self.sandbox_id, &format!("uploading {path}"), response).await)
        }
    }

    /// Download `path` from the sandbox into `dest`.
    async fn download(
        &self,
        client: &reqwest::Client,
        path: &str,
        dest: &Path,
        user: &str,
    ) -> Result<()> {
        let mut response = self
            .authorize(client.get(self.files(path, user)?), user)
            .send()
            .await
            .map_err(|e| failure(&self.sandbox_id, format!("downloading {path}: {e}")))?;
        if !response.status().is_success() {
            return Err(
                http_error(&self.sandbox_id, &format!("downloading {path}"), response).await,
            );
        }
        let mut file = tokio::fs::File::create(dest)
            .await
            .map_err(|e| failure(&self.sandbox_id, format!("{}: {e}", dest.display())))?;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| failure(&self.sandbox_id, format!("downloading {path}: {e}")))?
        {
            file.write_all(&chunk)
                .await
                .map_err(|e| failure(&self.sandbox_id, format!("{}: {e}", dest.display())))?;
        }
        file.flush()
            .await
            .map_err(|e| failure(&self.sandbox_id, format!("{}: {e}", dest.display())))
    }

    /// Start `argv` as `user`; the response is the event stream.
    async fn start(
        &self,
        client: &reqwest::Client,
        argv: &[String],
        user: &str,
        stdin: bool,
        envs: Value,
    ) -> Result<Frames> {
        let Some((cmd, args)) = argv.split_first() else {
            return Err(failure(&self.sandbox_id, "no command to start"));
        };
        let mut process = json!({ "cmd": cmd, "args": args, "envs": envs });
        if let Some(cwd) = &self.cwd {
            process["cwd"] = json!(cwd);
        }
        let body = envelope(&json!({ "process": process, "stdin": stdin }))?;
        let response = self
            .authorize(client.post(self.rpc("Start")), user)
            .header("Content-Type", "application/connect+json")
            .header("Connect-Protocol-Version", "1")
            .header("Keepalive-Ping-Interval", "50")
            .body(body)
            .send()
            .await
            .map_err(|e| failure(&self.sandbox_id, format!("starting {cmd}: {e}")))?;
        if !response.status().is_success() {
            return Err(http_error(&self.sandbox_id, &format!("starting {cmd}"), response).await);
        }
        Ok(Frames {
            response,
            buffer: Vec::new(),
        })
    }

    async fn unary(&self, client: &reqwest::Client, method: &str, body: &Value) -> Result<()> {
        let response = self
            .authorize(client.post(self.rpc(method)), &self.user)
            .header("Connect-Protocol-Version", "1")
            .json(body)
            .send()
            .await
            .map_err(|e| failure(&self.sandbox_id, format!("{method}: {e}")))?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(http_error(&self.sandbox_id, method, response).await)
        }
    }

    /// Run `argv` as `user` to completion; an error carries its stderr.
    async fn run(&self, client: &reqwest::Client, argv: &[String], user: &str) -> Result<()> {
        let mut frames = self.start(client, argv, user, false, json!({})).await?;
        let mut stderr = Vec::new();
        loop {
            match frames.next(&self.sandbox_id).await? {
                Some(Frame::Message(message)) => match Event::of(&message)? {
                    Event::Stderr(bytes) => stderr.extend(bytes),
                    Event::End(0) => return Ok(()),
                    Event::End(code) => {
                        return Err(failure(
                            &self.sandbox_id,
                            format!(
                                "{} exited with {code}: {}",
                                argv.join(" "),
                                String::from_utf8_lossy(&stderr).trim()
                            ),
                        ));
                    }
                    _ => {}
                },
                Some(Frame::End) | None => {
                    return Err(failure(
                        &self.sandbox_id,
                        "the process stream ended without an exit",
                    ));
                }
            }
        }
    }
}

/// A Connect envelope around one JSON message.
fn envelope(message: &Value) -> Result<Vec<u8>> {
    let json = serde_json::to_vec(message).map_err(|e| failure("e2b", e))?;
    let length = u32::try_from(json.len()).map_err(|_| failure("e2b", "message too large"))?;
    let mut framed = Vec::with_capacity(json.len() + 5);
    framed.push(0);
    framed.extend_from_slice(&length.to_be_bytes());
    framed.extend_from_slice(&json);
    Ok(framed)
}

/// One message of a Connect server stream.
enum Frame {
    Message(Value),
    /// The end-of-stream message, without an error.
    End,
}

/// Reads Connect envelopes off a streaming response.
struct Frames {
    response: reqwest::Response,
    buffer: Vec<u8>,
}

impl Frames {
    async fn next(&mut self, sandbox: &str) -> Result<Option<Frame>> {
        loop {
            if self.buffer.len() >= 5 {
                let flags = self.buffer[0];
                let length = u32::from_be_bytes([
                    self.buffer[1],
                    self.buffer[2],
                    self.buffer[3],
                    self.buffer[4],
                ]) as usize;
                if self.buffer.len() >= 5 + length {
                    let body: Vec<u8> = self.buffer.drain(..5 + length).skip(5).collect();
                    if flags & 0x01 != 0 {
                        return Err(failure(
                            sandbox,
                            "a compressed message, which was not asked for",
                        ));
                    }
                    let value: Value = if body.is_empty() {
                        json!({})
                    } else {
                        serde_json::from_slice(&body)
                            .map_err(|e| failure(sandbox, format!("a malformed message: {e}")))?
                    };
                    if flags & 0x02 != 0 {
                        if let Some(error) = value.get("error") {
                            return Err(failure(
                                sandbox,
                                format!("the process stream failed: {error}"),
                            ));
                        }
                        return Ok(Some(Frame::End));
                    }
                    return Ok(Some(Frame::Message(value)));
                }
            }
            match self
                .response
                .chunk()
                .await
                .map_err(|e| failure(sandbox, format!("reading the process stream: {e}")))?
            {
                Some(chunk) => self.buffer.extend_from_slice(&chunk),
                None if self.buffer.is_empty() => return Ok(None),
                None => return Err(failure(sandbox, "the process stream ended mid-message")),
            }
        }
    }
}

/// What one `ProcessEvent` says.
enum Event {
    Start(u32),
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    End(i64),
    Other,
}

impl Event {
    fn of(message: &Value) -> Result<Self> {
        let event = &message["event"];
        if let Some(start) = event.get("start") {
            let pid = start["pid"]
                .as_u64()
                .and_then(|p| u32::try_from(p).ok())
                .unwrap_or(0);
            return Ok(Self::Start(pid));
        }
        if let Some(data) = event.get("data") {
            if let Some(out) = data
                .get("stdout")
                .or_else(|| data.get("pty"))
                .and_then(Value::as_str)
            {
                return Ok(Self::Stdout(unb64(out)?));
            }
            if let Some(err) = data.get("stderr").and_then(Value::as_str) {
                return Ok(Self::Stderr(unb64(err)?));
            }
        }
        if let Some(end) = event.get("end") {
            // proto3 JSON leaves out a zero exit code.
            return Ok(Self::End(
                end.get("exitCode").and_then(Value::as_i64).unwrap_or(0),
            ));
        }
        Ok(Self::Other)
    }
}

/// Start `argv` in the sandbox and relay `io` to it until it exits.
pub async fn relay<I, O, E>(session: &Session, argv: &[String], io: Stdio<I, O, E>) -> Result<i32>
where
    I: AsyncRead + Unpin + Send + 'static,
    O: AsyncWrite + Unpin,
    E: AsyncWrite + Unpin,
{
    let client = client()?;
    let envs: serde_json::Map<String, Value> = session
        .env
        .iter()
        .filter_map(|name| std::env::var(name).ok().map(|v| (name.clone(), json!(v))))
        .collect();
    let mut frames = session
        .start(&client, argv, &session.user, true, Value::Object(envs))
        .await?;
    let Stdio {
        input,
        mut output,
        mut errors,
    } = io;
    let mut input = Some(input);
    let mut feeding: Option<tokio::task::JoinHandle<Result<()>>> = None;
    let outcome = loop {
        match frames.next(&session.sandbox_id).await {
            Ok(Some(Frame::Message(message))) => match Event::of(&message) {
                Ok(Event::Start(pid)) => {
                    if let Some(input) = input.take() {
                        let (client, session) = (client.clone(), session.clone());
                        feeding = Some(tokio::spawn(async move {
                            feed(client, session, pid, input).await
                        }));
                    }
                }
                Ok(Event::Stdout(bytes)) => {
                    if let Err(err) = deliver(&mut output, &bytes).await {
                        break Err(err);
                    }
                }
                Ok(Event::Stderr(bytes)) => {
                    if let Err(err) = deliver(&mut errors, &bytes).await {
                        break Err(err);
                    }
                }
                Ok(Event::End(code)) => break Ok(i32::try_from(code).unwrap_or(1)),
                Ok(Event::Other) => {}
                Err(err) => break Err(err),
            },
            Ok(Some(Frame::End) | None) => {
                break Err(failure(
                    &session.sandbox_id,
                    "the process stream ended without an exit",
                ));
            }
            Err(err) => break Err(err),
        }
    };
    if let Some(feeding) = feeding {
        feeding.abort();
    }
    outcome
}

/// Feed `input` to process `pid`, in order, then close its stdin.
async fn feed<I: AsyncRead + Unpin>(
    client: reqwest::Client,
    session: Session,
    pid: u32,
    input: I,
) -> Result<()> {
    let (c1, s1) = (client.clone(), session.clone());
    bridge::pump(
        input,
        move |chunk| {
            let (client, session) = (c1.clone(), s1.clone());
            async move {
                session
                    .unary(
                        &client,
                        "SendInput",
                        &json!({ "process": { "pid": pid }, "input": { "stdin": b64(&chunk) } }),
                    )
                    .await
            }
        },
        move || async move {
            // Older envd builds lack CloseStdin; an agent then sees EOF when
            // its sandbox goes away instead.
            if let Err(err) = session
                .unary(&client, "CloseStdin", &json!({ "process": { "pid": pid } }))
                .await
            {
                tracing::debug!(error = %err, "CloseStdin failed");
            }
            Ok(())
        },
    )
    .await
}

#[async_trait]
impl SandboxProvider for E2bProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> &'static str {
        "e2b"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            isolation: Isolation::MicroVm,
            workspace: WorkspaceAccess::Copied,
            network_allowlist: false,
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
        let client = client()?;
        let created = self.create(&client).await?;
        let session = Session {
            envd: self.envd_url(&created),
            token: created.envd_access_token.clone(),
            sandbox_id: created.sandbox_id.clone(),
            port: self.settings.envd_port,
            user: self.settings.user.clone(),
            cwd: None,
            env: Vec::new(),
        };
        let ran = session
            .run(&client, &["true".to_owned()], &self.settings.user)
            .await;
        let killed = self.kill(&client, &created.sandbox_id).await;
        ran?;
        killed
    }

    async fn open(&self, request: &LaunchRequest) -> Result<SandboxLaunch> {
        let Some(cuma) = self.cuma.clone() else {
            return Err(MetaAgentError::Configuration(format!(
                "sandbox {}: agents in an e2b sandbox are launched through cuma itself, whose path is unknown",
                self.name
            )));
        };
        let client = client()?;
        let created = self.create(&client).await?;
        let workspace = crate::canonical(&request.workspace);
        let session = Session {
            envd: self.envd_url(&created),
            token: created.envd_access_token.clone(),
            sandbox_id: created.sandbox_id.clone(),
            port: self.settings.envd_port,
            user: self.settings.user.clone(),
            cwd: request.collects().then(|| workspace.clone()),
            env: forwarded_env(request),
        };
        let remote = Remote {
            provider: self.clone_handle(),
            client,
            session,
            workspace,
            snapshot: None,
            work: tempfile::Builder::new()
                .prefix("cuma-e2b-")
                .tempdir()
                .map_err(|e| failure(&self.name, e))?,
            keep: request
                .workspace
                .join(".cuma")
                .join("sandbox-results")
                .join(&created.sandbox_id),
        };
        match remote.populate(request).await {
            Ok((snapshot, file)) => Ok(SandboxLaunch::with_session(
                bridge::prefix(&cuma, &file),
                Arc::new(Remote { snapshot, ..remote }),
            )),
            Err(err) => {
                remote.kill().await;
                Err(err)
            }
        }
    }
}

/// One launch's remote sandbox.
struct Remote {
    provider: E2bProvider,
    client: reqwest::Client,
    session: Session,
    workspace: String,
    snapshot: Option<Arc<Snapshot>>,
    work: tempfile::TempDir,
    keep: PathBuf,
}

impl Remote {
    async fn populate(&self, request: &LaunchRequest) -> Result<(Option<Arc<Snapshot>>, PathBuf)> {
        let snapshot = if request.collects() {
            let snapshot = Snapshot::take_async(&request.workspace, self.work.path()).await?;
            if let Some(archive) = snapshot.archive() {
                let bytes = tokio::fs::read(archive).await.map_err(|e| {
                    failure(&self.provider.name, format!("reading the archive: {e}"))
                })?;
                self.session
                    .upload(&self.client, "/tmp/cuma-ws.tar", bytes, "root")
                    .await?;
            }
            let unpack = format!(
                "mkdir -p \"$1\" && tar -xf /tmp/cuma-ws.tar -C \"$1\" && rm -f /tmp/cuma-ws.tar && chown -R {} \"$1\"",
                self.session.user
            );
            self.session
                .run(
                    &self.client,
                    &argv(&["sh", "-c", &unpack, "sh", &self.workspace]),
                    "root",
                )
                .await?;
            Some(snapshot)
        } else {
            None
        };
        let file =
            bridge::write_session(self.work.path(), &BridgeSession::E2b(self.session.clone()))?;
        Ok((snapshot, file))
    }

    async fn collect(&self, snapshot: &Arc<Snapshot>) -> Result<()> {
        self.session
            .run(
                &self.client,
                &argv(&[
                    "sh",
                    "-c",
                    "tar -cf /tmp/cuma-out.tar -C \"$1\" .",
                    "sh",
                    &self.workspace,
                ]),
                "root",
            )
            .await?;
        let local = self.work.path().join("out.tar");
        self.session
            .download(&self.client, "/tmp/cuma-out.tar", &local, "root")
            .await?;
        let report = Arc::clone(snapshot)
            .merge_archive_async(local, self.work.path().join("result"), self.keep.clone())
            .await?;
        tracing::info!(sandbox = %self.provider.name, %report, "brought the agent's work back");
        Ok(())
    }

    async fn kill(&self) {
        if let Err(err) = self
            .provider
            .kill(&self.client, &self.session.sandbox_id)
            .await
        {
            tracing::warn!(sandbox = %self.session.sandbox_id, error = %err, "killing the sandbox failed");
        }
    }
}

fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_owned()).collect()
}

#[async_trait]
impl SandboxSession for Remote {
    async fn finish(&self) -> Result<()> {
        let collected = match &self.snapshot {
            Some(snapshot) => self.collect(snapshot).await,
            None => Ok(()),
        };
        self.kill().await;
        collected
    }

    async fn abort(&self) {
        self.kill().await;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use axum::body::{Body, Bytes};
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode, Uri};
    use axum::routing::{delete, post};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use tokio::sync::mpsc;

    fn query(uri: &Uri) -> HashMap<String, String> {
        reqwest::Url::parse(&format!("http://fake{uri}"))
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect()
    }

    type Shared<T> = Arc<Mutex<T>>;
    /// Stdin chunks for the running fake process; `None` closes it.
    type Feed = mpsc::UnboundedSender<Option<Vec<u8>>>;
    /// Each create request, with the API key it carried.
    type Creates = Vec<(Value, Option<String>)>;

    /// A stand-in for the lifecycle API and `envd`, on one port.
    #[derive(Clone, Default)]
    struct Fake {
        created: Shared<Creates>,
        killed: Shared<Vec<String>>,
        files: Shared<HashMap<String, Vec<u8>>>,
        input: Shared<Option<Feed>>,
        started: Shared<Vec<(Value, HeaderMap)>>,
    }

    fn frame(message: &Value, flags: u8) -> Bytes {
        let mut framed = envelope(message).unwrap();
        framed[0] = flags;
        Bytes::from(framed)
    }

    async fn serve(fake: Fake) -> String {
        let app = axum::Router::new()
            .route(
                "/sandboxes",
                post(|State(f): State<Fake>, headers: HeaderMap, body: axum::Json<Value>| async move {
                    let key = headers.get("x-api-key").map(|v| v.to_str().unwrap().to_owned());
                    f.created.lock().unwrap().push((body.0, key));
                    axum::Json(json!({
                        "sandboxID": "sbx1", "envdAccessToken": "tok", "domain": null,
                        "templateID": "t", "clientID": "c", "envdVersion": "0.2.0"
                    }))
                }),
            )
            .route(
                "/sandboxes/{id}",
                delete(|State(f): State<Fake>, axum::extract::Path(id): axum::extract::Path<String>| async move {
                    f.killed.lock().unwrap().push(id);
                    StatusCode::NO_CONTENT
                }),
            )
            .route(
                "/files",
                post(|State(f): State<Fake>, uri: Uri, body: Bytes| async move {
                    let q = query(&uri);
                    f.files.lock().unwrap().insert(q["path"].clone(), body.to_vec());
                    axum::Json(json!([{ "path": q["path"], "type": "file" }]))
                })
                .get(|State(f): State<Fake>, uri: Uri| async move {
                    let q = query(&uri);
                    match f.files.lock().unwrap().get(&q["path"]) {
                        Some(bytes) => (StatusCode::OK, bytes.clone()),
                        None => (StatusCode::NOT_FOUND, Vec::new()),
                    }
                }),
            )
            .route(
                "/process.Process/Start",
                post(|State(f): State<Fake>, headers: HeaderMap, body: Bytes| async move {
                    let request: Value = serde_json::from_slice(&body[5..]).unwrap();
                    f.started.lock().unwrap().push((request.clone(), headers));
                    let cmd = request["process"]["cmd"].as_str().unwrap().to_owned();
                    let (tx, mut rx) = mpsc::unbounded_channel::<Option<Vec<u8>>>();
                    *f.input.lock().unwrap() = Some(tx);
                    let stream = async_stream(move |out: mpsc::UnboundedSender<Bytes>| async move {
                        let _ = out.send(frame(&json!({"event": {"start": {"pid": 7}}}), 0));
                        match cmd.as_str() {
                            // Echo stdin back as stdout until it closes.
                            "cat" => {
                                while let Some(Some(chunk)) = rx.recv().await {
                                    let _ = out.send(frame(&json!({"event": {"data": {"stdout": b64(&chunk)}}}), 0));
                                }
                                let _ = out.send(frame(&json!({"event": {"end": {"exited": true, "status": "exit status 0"}}}), 0));
                            }
                            _ => {
                                let _ = out.send(frame(&json!({"event": {"data": {"stderr": b64(b"no such file\n")}}}), 0));
                                let _ = out.send(frame(&json!({"event": {"end": {"exitCode": 3, "exited": true}}}), 0));
                            }
                        }
                        let _ = out.send(frame(&json!({}), 2));
                    });
                    Body::from_stream(stream)
                }),
            )
            .route(
                "/process.Process/SendInput",
                post(|State(f): State<Fake>, body: axum::Json<Value>| async move {
                    assert_eq!(body["process"]["pid"], 7);
                    let chunk = unb64(body["input"]["stdin"].as_str().unwrap()).unwrap();
                    if let Some(tx) = f.input.lock().unwrap().as_ref() {
                        // The process may have ended already, as a real one may.
                        let _ = tx.send(Some(chunk));
                    }
                    axum::Json(json!({}))
                }),
            )
            .route(
                "/process.Process/CloseStdin",
                post(|State(f): State<Fake>| async move {
                    if let Some(tx) = f.input.lock().unwrap().as_ref() {
                        let _ = tx.send(None);
                    }
                    axum::Json(json!({}))
                }),
            )
            .with_state(fake);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{address}")
    }

    /// A body stream fed by a producer task.
    fn async_stream<F, Fut>(
        produce: F,
    ) -> impl futures::Stream<Item = std::result::Result<Bytes, std::io::Error>>
    where
        F: FnOnce(mpsc::UnboundedSender<Bytes>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(produce(tx));
        futures::stream::unfold(
            rx,
            |mut rx| async move { rx.recv().await.map(|b| (Ok(b), rx)) },
        )
    }

    fn session(url: &str) -> Session {
        Session {
            envd: url.to_owned(),
            token: Some("tok".into()),
            sandbox_id: "sbx1".into(),
            port: 49983,
            user: "user".into(),
            cwd: Some("/work".into()),
            env: vec![],
        }
    }

    #[tokio::test]
    async fn stdio_round_trips_through_start_send_input_and_close_stdin() {
        let fake = Fake::default();
        let url = serve(fake.clone()).await;
        let input: &'static [u8] = b"{\"jsonrpc\":\"2.0\",\"id\":1}\n{\"second\":true}\n";
        let mut output = Vec::new();
        let mut errors = Vec::new();

        let code = relay(
            &session(&url),
            &argv(&["cat"]),
            Stdio {
                input,
                output: &mut output,
                errors: &mut errors,
            },
        )
        .await
        .unwrap();

        assert_eq!(code, 0, "a zero exit code is left out of the JSON");
        assert_eq!(output, input);
        let (request, headers) = fake.started.lock().unwrap()[0].clone();
        assert_eq!(request["stdin"], true);
        assert_eq!(request["process"]["cwd"], "/work");
        assert_eq!(headers["x-access-token"], "tok");
        assert_eq!(headers["content-type"], "application/connect+json");
        assert_eq!(headers["authorization"], format!("Basic {}", b64(b"user:")));
    }

    #[tokio::test]
    async fn a_failing_command_reports_its_exit_code_and_stderr() {
        let url = serve(Fake::default()).await;
        let mut output = Vec::new();
        let mut errors = Vec::new();
        let code = relay(
            &session(&url),
            &argv(&["missing-agent"]),
            Stdio {
                input: tokio::io::empty(),
                output: &mut output,
                errors: &mut errors,
            },
        )
        .await
        .unwrap();
        assert_eq!(code, 3);
        assert_eq!(errors, b"no such file\n");

        let client = client().unwrap();
        let err = session(&url)
            .run(&client, &argv(&["tar", "-x"]), "root")
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("exited with 3") && err.contains("no such file"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn files_round_trip_through_envd() {
        let url = serve(Fake::default()).await;
        let client = client().unwrap();
        let s = session(&url);
        s.upload(
            &client,
            "/tmp/cuma-ws.tar",
            b"archive bytes".to_vec(),
            "root",
        )
        .await
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("back.tar");
        s.download(&client, "/tmp/cuma-ws.tar", &dest, "root")
            .await
            .unwrap();
        assert_eq!(std::fs::read(dest).unwrap(), b"archive bytes");
    }

    #[tokio::test]
    async fn a_sandbox_is_created_from_the_template_and_killed() {
        let fake = Fake::default();
        let url = serve(fake.clone()).await;
        let provider = E2bProvider::new(
            "cube",
            E2bSandbox {
                api_url: url.clone(),
                template: "coding-agent".into(),
                // PATH is always set: a stand-in for the key's variable.
                api_key_ref: Some("PATH".into()),
                internet: false,
                ..E2bSandbox::default()
            },
            None,
        );
        let client = client().unwrap();
        let created = provider.create(&client).await.unwrap();
        assert_eq!(created.sandbox_id, "sbx1");
        assert_eq!(provider.envd_url(&created), "https://49983-sbx1.e2b.app");
        provider.kill(&client, "sbx1").await.unwrap();

        let (body, key) = fake.created.lock().unwrap()[0].clone();
        assert_eq!(body["templateID"], "coding-agent");
        assert_eq!(body["secure"], true);
        assert_eq!(body["allow_internet_access"], false);
        assert_eq!(key.unwrap(), std::env::var("PATH").unwrap());
        assert_eq!(*fake.killed.lock().unwrap(), ["sbx1"]);
    }

    #[tokio::test]
    async fn a_negotiation_launch_goes_through_the_bridge_and_kills_the_sandbox_after() {
        let fake = Fake::default();
        let url = serve(fake.clone()).await;
        let provider = E2bProvider::new(
            "cube",
            E2bSandbox {
                api_url: url.clone(),
                template: "t".into(),
                api_key_ref: None,
                ..E2bSandbox::default()
            },
            Some(PathBuf::from("/usr/local/bin/cuma")),
        )
        .with_envd_url(&url);
        let ws = tempfile::tempdir().unwrap();
        let launch = provider
            .open(&LaunchRequest::bare(
                ws.path(),
                cuma_core::ports::LaunchPurpose::Negotiate,
            ))
            .await
            .unwrap();
        assert_eq!(
            &launch.prefix()[..3],
            ["/usr/local/bin/cuma", "sandbox", "exec"]
        );
        let file = &launch.prefix()[4];
        let saved: BridgeSession =
            serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
        let BridgeSession::E2b(saved) = saved else {
            panic!("e2b")
        };
        assert_eq!(saved.token.as_deref(), Some("tok"));
        assert_eq!(saved.cwd, None, "negotiation needs no workspace");
        launch.finish().await.unwrap();
        assert_eq!(*fake.killed.lock().unwrap(), ["sbx1"]);
    }
}
