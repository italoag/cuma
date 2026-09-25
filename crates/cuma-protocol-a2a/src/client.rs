//! The A2A JSON-RPC client and adapter.
//!
//! ## Lifecycle
//!
//! A task sent to a peer is followed to a conclusion, not assumed finished
//! because the first response arrived:
//!
//! - a peer that advertises streaming is called with `SendStreamingMessage`
//!   and its SSE events are forwarded as they arrive;
//! - otherwise `SendMessage` is called, and a task that comes back still
//!   running is polled with `GetTask`, backing off, until it settles;
//! - a task the harness abandons — its deadline passed, or the orchestrator
//!   dropped the attempt — is cancelled with `CancelTask` rather than left
//!   running on someone else's machine.
//!
//! `INPUT_REQUIRED` and `AUTH_REQUIRED` end the attempt as failures. The peer
//! stopped to ask something, and reporting that as success would record work
//! as done when it has not been.

use crate::card::{AGENT_CARD_PATH, AgentCard, capabilities_from_card};
use crate::wire::{
    self, Dialect, SendResult, StreamEvent, TaskState, WireTask, error_code, methods,
};
use async_trait::async_trait;
use cuma_core::error::{MetaAgentError, Result};
use cuma_core::ports::{AgentAdapter, AgentDiscovery, ExecutionRequest, ExecutionUpdate};
use cuma_core::{
    AgentDescriptor, AgentId, AgentProtocol, AttemptId, ErrorClass, ExecutionOutcome, TokenUsage,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, mpsc};

/// The largest response body accepted from a peer, in bytes.
///
/// A remote agent is not trusted to bound its own output; without a cap, one
/// peer could exhaust the harness's memory by streaming indefinitely.
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// First delay between `GetTask` polls.
const POLL_INITIAL: Duration = Duration::from_millis(250);

/// Longest delay between `GetTask` polls.
const POLL_MAX: Duration = Duration::from_secs(5);

/// Per-request timeout for calls that should answer promptly.
const SHORT_CALL: Duration = Duration::from_secs(30);

/// A JSON-RPC envelope, as returned by an A2A endpoint.
#[derive(Debug, Deserialize)]
struct JsonRpcResponse {
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

impl JsonRpcError {
    /// Map a JSON-RPC error onto a failure class.
    fn class(&self) -> ErrorClass {
        match self.code {
            error_code::PARSE_ERROR
            | error_code::INVALID_REQUEST
            | error_code::METHOD_NOT_FOUND
            | error_code::INVALID_PARAMS
            | error_code::UNSUPPORTED_OPERATION => ErrorClass::ProtocolError,
            error_code::INTERNAL_ERROR | error_code::TASK_NOT_FOUND => ErrorClass::TaskFailure,
            -32006 => ErrorClass::InvalidResponse,
            _ => ErrorClass::Unknown,
        }
    }
}

/// Why a call failed.
enum CallError {
    /// The peer answered with a JSON-RPC error.
    Rpc(JsonRpcError),
    /// Anything else: transport, HTTP status, size cap, malformed envelope.
    Other(MetaAgentError),
}

/// How to reach the peer, as learned from its card.
#[derive(Debug, Clone)]
struct Peer {
    url: String,
    dialect: Dialect,
    streaming: bool,
}

/// Reaches one remote agent over A2A.
pub struct A2aAdapter {
    id: AgentId,
    endpoint: String,
    http: reqwest::Client,
    descriptor: Arc<Mutex<AgentDescriptor>>,
    peer: Arc<Mutex<Peer>>,
    /// Handle for the bearer token, resolved at call time. Never the token.
    auth_handle: Option<String>,
}

impl A2aAdapter {
    /// An adapter for the agent at `endpoint`.
    pub fn new(id: impl Into<AgentId>, endpoint: impl Into<String>) -> Result<Self> {
        let id = id.into();
        let endpoint = endpoint.into();
        ensure_acceptable_endpoint(&id, &endpoint)?;

        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .user_agent(concat!("cuma/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|err| {
                MetaAgentError::Configuration(format!("cannot build an HTTP client: {err}"))
            })?;

        let descriptor = AgentDescriptor::new(id.clone(), id.to_string(), AgentProtocol::A2A);

        Ok(Self {
            peer: Arc::new(Mutex::new(Peer {
                url: endpoint.clone(),
                dialect: Dialect::V1,
                streaming: false,
            })),
            id,
            endpoint,
            http,
            descriptor: Arc::new(Mutex::new(descriptor)),
            auth_handle: None,
        })
    }

    /// Attach a secret handle for bearer authentication.
    #[must_use]
    pub fn with_auth_handle(mut self, handle: impl Into<String>) -> Self {
        self.auth_handle = Some(handle.into());
        self
    }

    /// The endpoint this adapter was configured with.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The dialect this adapter currently speaks to its peer.
    pub async fn dialect(&self) -> Dialect {
        self.peer.lock().await.dialect
    }

    /// Where an Agent Card for this endpoint may be found, most likely first.
    ///
    /// RFC 8615 puts well-known documents at the origin's root; plenty of
    /// deployments serve them under the endpoint's own path instead.
    fn card_urls(&self) -> Vec<String> {
        let mut urls = Vec::new();
        if let Ok(parsed) = reqwest::Url::parse(&self.endpoint)
            && let Ok(root) = parsed.join(AGENT_CARD_PATH)
        {
            urls.push(root.to_string());
        }
        let under_path = format!("{}{AGENT_CARD_PATH}", self.endpoint.trim_end_matches('/'));
        if !urls.contains(&under_path) {
            urls.push(under_path);
        }
        urls
    }

    /// Fetch and parse the agent's card.
    pub async fn fetch_card(&self) -> Result<AgentCard> {
        let mut last_error = None;

        for url in self.card_urls() {
            let response = match self
                .authorize(self.http.get(&url).timeout(SHORT_CALL))
                .send()
                .await
            {
                Ok(response) => response,
                Err(err) => {
                    last_error = Some(format!("cannot fetch {url}: {err}"));
                    continue;
                }
            };

            if !response.status().is_success() {
                last_error = Some(format!("{url} returned {}", response.status()));
                continue;
            }

            let body = self.read_bounded(response).await?;
            return serde_json::from_str(&body).map_err(|err| {
                MetaAgentError::protocol_msg(
                    "a2a",
                    format!("{url} is not a valid Agent Card: {err}"),
                )
            });
        }

        Err(MetaAgentError::protocol_msg(
            "a2a",
            last_error.unwrap_or_else(|| "no Agent Card location to try".to_owned()),
        ))
    }

    /// Attach the bearer token, if one is configured and available.
    fn authorize(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let Some(handle) = &self.auth_handle else {
            return request;
        };

        // Resolved from the environment here as the baseline secret store. A
        // keychain-backed store plugs in behind the same handle without the
        // token ever being written to config or logs.
        match std::env::var(handle) {
            Ok(token) => request.bearer_auth(token),
            Err(_) => {
                tracing::warn!(
                    agent = %self.id,
                    "no secret is available for the configured handle; calling unauthenticated"
                );
                request
            }
        }
    }

    /// Read a response body, refusing anything over the size cap.
    async fn read_bounded(&self, mut response: reqwest::Response) -> Result<String> {
        if let Some(length) = response.content_length()
            && length > MAX_RESPONSE_BYTES as u64
        {
            return Err(self.oversized(length as usize));
        }

        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|err| {
            MetaAgentError::protocol_msg("a2a", format!("cannot read the response body: {err}"))
        })? {
            body.extend_from_slice(&chunk);
            if body.len() > MAX_RESPONSE_BYTES {
                return Err(self.oversized(body.len()));
            }
        }

        String::from_utf8(body).map_err(|_| {
            MetaAgentError::protocol_msg("a2a", "the response body is not valid UTF-8")
        })
    }

    fn oversized(&self, bytes: usize) -> MetaAgentError {
        MetaAgentError::Security(format!(
            "agent {}: response of {bytes} bytes exceeds the {MAX_RESPONSE_BYTES} byte cap",
            self.id
        ))
    }

    /// Send one JSON-RPC request and return the HTTP response.
    async fn post(
        &self,
        url: &str,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
        streaming: bool,
    ) -> std::result::Result<reqwest::Response, CallError> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": uuid::Uuid::new_v4().to_string(),
            "method": method,
            "params": params,
        });

        let mut request = self.authorize(self.http.post(url).json(&body));
        if let Some(timeout) = timeout {
            request = request.timeout(timeout);
        }
        if streaming {
            request = request.header(reqwest::header::ACCEPT, "text/event-stream");
        }

        let response = request.send().await.map_err(|err| {
            let class = if err.is_timeout() {
                "timed out"
            } else if err.is_connect() {
                "could not connect"
            } else {
                "failed"
            };
            CallError::Other(MetaAgentError::protocol_msg(
                "a2a",
                format!("{method} {class}: {err}"),
            ))
        })?;

        let status = response.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after_ms = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(|secs| secs.saturating_mul(1000));

            return Err(CallError::Other(MetaAgentError::RateLimit {
                agent: self.id.clone(),
                retry_after_ms,
            }));
        }

        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(CallError::Other(MetaAgentError::Authentication {
                target: self.id.to_string(),
                message: format!("{method} returned {status}"),
            }));
        }

        if !status.is_success() {
            return Err(CallError::Other(MetaAgentError::protocol_msg(
                "a2a",
                format!("{method} returned HTTP {status}"),
            )));
        }

        Ok(response)
    }

    /// Issue one JSON-RPC call and unwrap its result.
    async fn call_raw(
        &self,
        url: &str,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
    ) -> std::result::Result<Value, CallError> {
        let response = self.post(url, method, params, timeout, false).await?;
        let body = self
            .read_bounded(response)
            .await
            .map_err(CallError::Other)?;
        parse_envelope(method, &body)
    }

    /// Issue a call in the peer's dialect, falling back to 0.3 once if a 1.0
    /// method name is not recognised.
    async fn call(&self, method: &str, params: Value, timeout: Option<Duration>) -> Result<Value> {
        let peer = self.peer.lock().await.clone();
        let name = wire_name(method, peer.dialect);

        match self
            .call_raw(&peer.url, name, params.clone(), timeout)
            .await
        {
            Ok(result) => Ok(result),
            Err(CallError::Rpc(error))
                if error.code == error_code::METHOD_NOT_FOUND
                    && peer.dialect == Dialect::V1
                    && methods::legacy_name(method).is_some() =>
            {
                tracing::info!(
                    agent = %self.id,
                    method,
                    "peer does not recognise the A2A 1.0 method; retrying in the 0.3 dialect"
                );
                self.peer.lock().await.dialect = Dialect::Legacy;
                let legacy = wire_name(method, Dialect::Legacy);
                let params = relegacy_params(method, params);
                self.call_raw(&peer.url, legacy, params, timeout)
                    .await
                    .map_err(|err| self.lift(legacy, err))
            }
            Err(err) => Err(self.lift(name, err)),
        }
    }

    /// Turn a call failure into a domain error.
    fn lift(&self, method: &str, err: CallError) -> MetaAgentError {
        match err {
            CallError::Rpc(error) => MetaAgentError::agent(
                self.id.clone(),
                format!("{method}: {} (code {})", error.message, error.code),
                error.class(),
            ),
            CallError::Other(err) => err,
        }
    }

    /// Fetch the card and fold what it says into the descriptor.
    pub async fn refresh_from_card(&self) -> Result<AgentDescriptor> {
        let card = self.fetch_card().await?;

        {
            let mut peer = self.peer.lock().await;
            peer.streaming = card.capabilities.streaming;
            if let Some((url, dialect)) = card.jsonrpc_endpoint() {
                peer.dialect = dialect;
                // The card is remote-controlled. It may point calls elsewhere
                // — that is what the field is for — but not at cleartext.
                match ensure_acceptable_endpoint(&self.id, &url) {
                    Ok(()) => peer.url = url,
                    Err(err) => tracing::warn!(
                        agent = %self.id,
                        error = %err,
                        "ignoring the endpoint the Agent Card names; keeping the configured one"
                    ),
                }
            }
        }

        let mut descriptor = self.descriptor.lock().await;
        descriptor.name = card.name.clone();
        descriptor.capabilities = capabilities_from_card(&card);
        descriptor
            .metadata
            .insert("endpoint".to_owned(), self.endpoint.clone());
        if let Some(version) = card.protocol_version() {
            descriptor
                .metadata
                .insert("a2a_version".to_owned(), version);
        }
        descriptor.metadata.insert(
            "a2a_streaming".to_owned(),
            card.capabilities.streaming.to_string(),
        );

        Ok(descriptor.clone())
    }

    /// Send the message and wait for the task to settle, polling if needed.
    async fn run_blocking(
        &self,
        prompt: &str,
        guard: &mut CancelGuard,
        updates: &mpsc::Sender<ExecutionUpdate>,
    ) -> Result<Settled> {
        let dialect = self.dialect().await;
        let params = send_params(prompt, dialect);
        let result = self.call(methods::SEND_MESSAGE, params, None).await?;

        let task = match wire::parse_send_result(&result) {
            Some(SendResult::Message(text)) => {
                forward(updates, &text).await;
                return Ok(Settled::Message(text));
            }
            Some(SendResult::Task(task)) => task,
            None => {
                return Err(MetaAgentError::agent(
                    self.id.clone(),
                    "SendMessage returned neither a task nor a message",
                    ErrorClass::InvalidResponse,
                ));
            }
        };

        guard.task_id = Some(task.id.clone());
        let task = self.poll_until_settled(task).await?;
        forward(updates, &task.text()).await;
        Ok(Settled::Task(task))
    }

    /// Poll `GetTask` until the task is terminal or interrupted.
    async fn poll_until_settled(&self, mut task: WireTask) -> Result<WireTask> {
        let mut delay = POLL_INITIAL;

        while !settled(task.state) {
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(POLL_MAX);

            let result = self
                .call(
                    methods::GET_TASK,
                    json!({ "id": task.id }),
                    Some(SHORT_CALL),
                )
                .await?;

            // 1.0 returns the task bare; some peers wrap it as a send result.
            task = wire::parse_task(&result)
                .or_else(|| match wire::parse_send_result(&result) {
                    Some(SendResult::Task(task)) => Some(task),
                    _ => None,
                })
                .ok_or_else(|| {
                    MetaAgentError::agent(
                        self.id.clone(),
                        "GetTask returned something that is not a task",
                        ErrorClass::InvalidResponse,
                    )
                })?;
        }

        Ok(task)
    }

    /// Send the message over SSE and follow the stream.
    ///
    /// Returns `Ok(None)` when the peer turns out not to support streaming
    /// after all, so the caller can fall back to the blocking path.
    async fn run_streaming(
        &self,
        prompt: &str,
        guard: &mut CancelGuard,
        updates: &mpsc::Sender<ExecutionUpdate>,
    ) -> Result<Option<Settled>> {
        let peer = self.peer.lock().await.clone();
        let method = wire_name(methods::SEND_STREAMING_MESSAGE, peer.dialect);

        let response = match self
            .post(
                &peer.url,
                method,
                send_params(prompt, peer.dialect),
                None,
                true,
            )
            .await
        {
            Ok(response) => response,
            Err(err) => return Err(self.lift(method, err)),
        };

        let is_sse = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/event-stream"));

        if !is_sse {
            // A plain JSON answer to a streaming call is almost always an
            // error envelope; a peer that cannot stream says so here.
            let body = self.read_bounded(response).await?;
            return match parse_envelope(method, &body) {
                Err(CallError::Rpc(error))
                    if matches!(
                        error.code,
                        error_code::METHOD_NOT_FOUND | error_code::UNSUPPORTED_OPERATION
                    ) =>
                {
                    tracing::info!(
                        agent = %self.id,
                        "peer advertised streaming but refused it; falling back to polling"
                    );
                    self.peer.lock().await.streaming = false;
                    Ok(None)
                }
                Err(err) => Err(self.lift(method, err)),
                Ok(result) => match stream_result(&result) {
                    Some(StreamEvent::Message(text)) => {
                        forward(updates, &text).await;
                        Ok(Some(Settled::Message(text)))
                    }
                    Some(StreamEvent::Task(task)) => {
                        guard.task_id = Some(task.id.clone());
                        let task = self.poll_until_settled(task).await?;
                        forward(updates, &task.text()).await;
                        Ok(Some(Settled::Task(task)))
                    }
                    _ => Err(MetaAgentError::agent(
                        self.id.clone(),
                        "a streaming call returned an unrecognised payload",
                        ErrorClass::InvalidResponse,
                    )),
                },
            };
        }

        let mut follower = StreamFollower::default();
        let mut reader = SseReader::default();
        let mut response = response;
        let mut received = 0usize;

        'stream: while let Some(chunk) = response.chunk().await.map_err(|err| {
            MetaAgentError::agent(
                self.id.clone(),
                format!("the event stream broke: {err}"),
                ErrorClass::ConnectionFailure,
            )
        })? {
            received += chunk.len();
            if received > MAX_RESPONSE_BYTES {
                return Err(self.oversized(received));
            }

            for data in reader.feed(&chunk) {
                let result = parse_envelope(method, &data).map_err(|err| self.lift(method, err))?;
                let Some(event) = stream_result(&result) else {
                    tracing::debug!(agent = %self.id, "ignoring an unrecognised stream event");
                    continue;
                };

                if let Some(text) = follower.apply(event) {
                    forward(updates, &text).await;
                }
                if guard.task_id.is_none() {
                    guard.task_id.clone_from(&follower.task_id);
                }
                if follower.finished() {
                    break 'stream;
                }
            }
        }

        if let Some(text) = follower.message {
            return Ok(Some(Settled::Message(text)));
        }

        let task = follower.into_task();
        if settled(task.state) || task.id.is_empty() {
            return Ok(Some(Settled::Task(task)));
        }

        // The stream closed before the task settled. Not a failure in
        // itself: fall back to asking.
        let task = self.poll_until_settled(task).await?;
        Ok(Some(Settled::Task(task)))
    }

    /// Build the outcome for a settled attempt.
    fn outcome(
        &self,
        request: &ExecutionRequest,
        settled: Settled,
        started: Instant,
    ) -> ExecutionOutcome {
        let (state, output) = match settled {
            Settled::Message(text) => (None, text),
            Settled::Task(task) => (Some(task.state), task.text()),
        };

        let failure = state.and_then(|state| failure_for(state, &output));
        let tokens = TokenUsage::estimate_from_text(&request.prompt, &output);

        #[allow(clippy::cast_possible_truncation)]
        let latency_ms = started.elapsed().as_millis() as u64;

        ExecutionOutcome {
            attempt_id: AttemptId::generate(),
            agent_id: self.id.clone(),
            model_id: request.model.clone(),
            success: failure.is_none(),
            output,
            changed_files: Vec::new(),
            // A2A reports no token counts, so these are estimated from the
            // text exchanged — and marked as such.
            tokens,
            latency_ms,
            failure_class: failure.as_ref().map(|(class, _)| *class),
            failure_reason: failure.map(|(_, reason)| reason),
            reported_cost_usd: None,
        }
    }
}

/// Whether a task needs no further polling.
fn settled(state: TaskState) -> bool {
    state.is_terminal() || state.is_interrupted()
}

/// The failure, if any, a final task state represents.
fn failure_for(state: TaskState, output: &str) -> Option<(ErrorClass, String)> {
    let said = || {
        let excerpt: String = output.chars().take(300).collect();
        if excerpt.is_empty() {
            String::new()
        } else {
            format!(": {excerpt}")
        }
    };

    match state {
        TaskState::Completed => None,
        TaskState::Canceled => Some((
            ErrorClass::Cancelled,
            "the remote task was cancelled".to_owned(),
        )),
        TaskState::AuthRequired => Some((
            ErrorClass::AuthenticationFailure,
            format!("the remote agent requires authentication{}", said()),
        )),
        TaskState::InputRequired => Some((
            ErrorClass::TaskFailure,
            format!("the remote agent stopped to ask for input{}", said()),
        )),
        TaskState::Rejected => Some((
            ErrorClass::TaskFailure,
            format!("the remote agent rejected the task{}", said()),
        )),
        TaskState::Failed => Some((
            ErrorClass::TaskFailure,
            format!("the remote task failed{}", said()),
        )),
        TaskState::Submitted | TaskState::Working | TaskState::Unknown => Some((
            ErrorClass::InvalidResponse,
            format!("the remote task ended without settling (state {state:?})"),
        )),
    }
}

/// How an attempt ended.
enum Settled {
    /// The peer answered directly.
    Message(String),
    /// The peer ran a task.
    Task(WireTask),
}

/// Parse a JSON-RPC envelope.
fn parse_envelope(method: &str, body: &str) -> std::result::Result<Value, CallError> {
    let envelope: JsonRpcResponse = serde_json::from_str(body).map_err(|err| {
        CallError::Other(MetaAgentError::protocol_msg(
            "a2a",
            format!("{method} returned invalid JSON-RPC: {err}"),
        ))
    })?;

    if let Some(error) = envelope.error {
        return Err(CallError::Rpc(error));
    }

    envelope.result.ok_or_else(|| {
        CallError::Other(MetaAgentError::protocol_msg(
            "a2a",
            format!("{method} returned neither result nor error"),
        ))
    })
}

/// A stream event, from a streamed `result`.
fn stream_result(result: &Value) -> Option<StreamEvent> {
    wire::parse_stream_event(result)
}

/// The method name to put on the wire.
fn wire_name(method: &str, dialect: Dialect) -> &str {
    match dialect {
        Dialect::V1 => method,
        Dialect::Legacy => methods::legacy_name(method).unwrap_or(method),
    }
}

/// `SendMessage` params in `dialect`.
fn send_params(prompt: &str, dialect: Dialect) -> Value {
    let message = wire::message_json(prompt, "user", dialect);
    match dialect {
        Dialect::V1 => json!({
            "message": message,
            "configuration": {
                "acceptedOutputModes": ["text/plain"],
                "returnImmediately": false,
            },
        }),
        Dialect::Legacy => json!({
            "message": message,
            "configuration": {
                "acceptedOutputModes": ["text/plain"],
                "blocking": true,
            },
        }),
    }
}

/// Rewrite 1.0 params for a 0.3 retry.
fn relegacy_params(method: &str, params: Value) -> Value {
    if method == methods::SEND_MESSAGE || method == methods::SEND_STREAMING_MESSAGE {
        let prompt = wire::parts_text(params.pointer("/message/parts"));
        return send_params(&prompt, Dialect::Legacy);
    }
    params
}

/// Forward text to the update channel, if there is any.
async fn forward(updates: &mpsc::Sender<ExecutionUpdate>, text: &str) {
    if !text.trim().is_empty() {
        let _ = updates
            .send(ExecutionUpdate::Text {
                content: text.to_owned(),
            })
            .await;
    }
}

/// Splits an SSE byte stream into `data` payloads.
#[derive(Default)]
struct SseReader {
    buffer: String,
}

impl SseReader {
    /// Feed bytes; returns every complete event's data.
    fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buffer.push_str(&String::from_utf8_lossy(bytes));
        // Normalise line endings so one separator rule covers every peer.
        if self.buffer.contains('\r') {
            self.buffer = self.buffer.replace("\r\n", "\n").replace('\r', "\n");
        }

        let mut events = Vec::new();
        while let Some(end) = self.buffer.find("\n\n") {
            let block: String = self.buffer.drain(..end + 2).collect();
            let data: Vec<&str> = block
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(|d| d.strip_prefix(' ').unwrap_or(d))
                .collect();
            if !data.is_empty() {
                events.push(data.join("\n"));
            }
        }
        events
    }
}

/// Accumulates a task from stream events.
#[derive(Default)]
struct StreamFollower {
    task_id: Option<String>,
    context_id: String,
    state: Option<TaskState>,
    status_text: Option<String>,
    artifact_text: String,
    message: Option<String>,
}

impl StreamFollower {
    /// Apply one event; returns text worth showing now.
    fn apply(&mut self, event: StreamEvent) -> Option<String> {
        match event {
            StreamEvent::Message(text) => {
                self.message = Some(text.clone());
                Some(text)
            }
            StreamEvent::Task(task) => {
                self.task_id = Some(task.id.clone());
                self.context_id.clone_from(&task.context_id);
                self.state = Some(task.state);
                let artifacts: Vec<&str> = task.artifacts.iter().map(|(_, t)| t.as_str()).collect();
                if !artifacts.is_empty() {
                    self.artifact_text = artifacts.join("\n\n");
                }
                if task.status_text.is_some() {
                    self.status_text.clone_from(&task.status_text);
                }
                None
            }
            StreamEvent::Status { state, text } => {
                self.state = Some(state);
                if text.is_some() {
                    self.status_text.clone_from(&text);
                }
                text
            }
            StreamEvent::Artifact { text } => {
                if !self.artifact_text.is_empty() && !text.is_empty() {
                    self.artifact_text.push('\n');
                }
                self.artifact_text.push_str(&text);
                (!text.is_empty()).then_some(text)
            }
        }
    }

    /// Whether the stream has said all it will.
    fn finished(&self) -> bool {
        self.message.is_some() || self.state.is_some_and(settled)
    }

    fn into_task(self) -> WireTask {
        WireTask {
            id: self.task_id.unwrap_or_default(),
            context_id: self.context_id,
            state: self.state.unwrap_or(TaskState::Unknown),
            status_text: self.status_text,
            timestamp: None,
            artifacts: if self.artifact_text.is_empty() {
                Vec::new()
            } else {
                vec![("stream".to_owned(), self.artifact_text)]
            },
        }
    }
}

/// Cancels a remote task if the attempt is abandoned before it settles.
///
/// The orchestrator enforces deadlines by dropping the attempt's future; this
/// is what turns that drop into a `CancelTask` instead of an orphaned task
/// still consuming the peer's resources.
struct CancelGuard {
    http: reqwest::Client,
    peer: Arc<Mutex<Peer>>,
    token: Option<String>,
    task_id: Option<String>,
    armed: bool,
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        let (true, Some(task_id)) = (self.armed, self.task_id.take()) else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        // The dialect may have changed mid-attempt; use what it settled on.
        let Ok(peer) = self.peer.try_lock().map(|p| p.clone()) else {
            return;
        };

        let body = json!({
            "jsonrpc": "2.0",
            "id": uuid::Uuid::new_v4().to_string(),
            "method": wire_name(methods::CANCEL_TASK, peer.dialect),
            "params": { "id": task_id },
        });
        let mut request = self.http.post(&peer.url).json(&body).timeout(SHORT_CALL);
        if let Some(token) = self.token.take() {
            request = request.bearer_auth(token);
        }

        runtime.spawn(async move {
            match request.send().await {
                Ok(_) => tracing::info!(task = task_id, "cancelled an abandoned remote task"),
                Err(err) => tracing::warn!(
                    task = task_id,
                    error = %err,
                    "could not cancel an abandoned remote task"
                ),
            }
        });
    }
}

impl std::fmt::Debug for A2aAdapter {
    /// Redacts the auth handle.
    ///
    /// The handle is a *reference* to a secret rather than the secret itself,
    /// but it names an environment variable, and naming it in a log line is
    /// one step closer to leaking it than is worth the debugging convenience.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("A2aAdapter")
            .field("id", &self.id)
            .field("endpoint", &self.endpoint)
            .field(
                "auth",
                &if self.auth_handle.is_some() {
                    "<configured>"
                } else {
                    "<none>"
                },
            )
            .finish()
    }
}

/// Refuse cleartext to anything but this machine.
///
/// Sending a task — and any context it carries — over cleartext to a remote
/// host is not something to do by accident.
fn ensure_acceptable_endpoint(id: &AgentId, endpoint: &str) -> Result<()> {
    if endpoint.starts_with("https://") || is_local_endpoint(endpoint) {
        Ok(())
    } else {
        Err(MetaAgentError::Security(format!(
            "agent {id}: refusing a non-HTTPS A2A endpoint {endpoint:?}"
        )))
    }
}

/// Whether an endpoint is unambiguously on this machine.
fn is_local_endpoint(endpoint: &str) -> bool {
    let Some(rest) = endpoint.strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or("");
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        bracketed.split(']').next().unwrap_or("")
    } else {
        authority.split(':').next().unwrap_or("")
    };

    host == "localhost" || host == "127.0.0.1" || host == "::1"
}

#[async_trait]
impl AgentAdapter for A2aAdapter {
    fn agent_id(&self) -> &AgentId {
        &self.id
    }

    async fn describe(&self) -> Result<AgentDescriptor> {
        Ok(self.descriptor.lock().await.clone())
    }

    async fn execute(
        &self,
        request: ExecutionRequest,
        updates: mpsc::Sender<ExecutionUpdate>,
    ) -> Result<ExecutionOutcome> {
        let started = Instant::now();
        let peer = self.peer.lock().await.clone();

        let mut guard = CancelGuard {
            http: self.http.clone(),
            peer: Arc::clone(&self.peer),
            token: self
                .auth_handle
                .as_ref()
                .and_then(|handle| std::env::var(handle).ok()),
            task_id: None,
            armed: true,
        };

        let deadline = Duration::from_millis(request.timeout_ms.max(1));
        let attempt = async {
            if peer.streaming
                && let Some(settled) = self
                    .run_streaming(&request.prompt, &mut guard, &updates)
                    .await?
            {
                return Ok(settled);
            }
            self.run_blocking(&request.prompt, &mut guard, &updates)
                .await
        };

        let settled = match tokio::time::timeout(deadline, attempt).await {
            Ok(Ok(settled)) => settled,
            // An error mid-poll leaves the remote task running as far as
            // anyone knows; the guard cancels it as it drops.
            Ok(Err(err)) => return Err(err),
            Err(_) => {
                return Err(MetaAgentError::agent(
                    self.id.clone(),
                    format!("no result within {} ms", request.timeout_ms),
                    ErrorClass::Timeout,
                ));
            }
        };

        guard.armed = false;
        Ok(self.outcome(&request, settled, started))
    }

    async fn health_check(&self) -> Result<()> {
        self.fetch_card().await.map(|_| ())
    }
}

/// Finds A2A agents declared in configuration.
pub struct A2aDiscovery {
    config: cuma_config::Config,
}

impl A2aDiscovery {
    /// Discover from `config`.
    pub fn new(config: cuma_config::Config) -> Self {
        Self { config }
    }

    /// Build adapters for every configured A2A agent.
    pub fn adapters(&self) -> Vec<A2aAdapter> {
        let mut adapters = Vec::new();

        for (id, agent_config) in &self.config.agents {
            if !agent_config.enabled || !agent_config.protocol.eq_ignore_ascii_case("a2a") {
                continue;
            }

            let Some(endpoint) = &agent_config.endpoint else {
                tracing::warn!(agent = id, "A2A agent has no endpoint; skipping");
                continue;
            };

            match A2aAdapter::new(id.as_str(), endpoint.clone()) {
                Ok(mut adapter) => {
                    if let Some(handle) = &agent_config.auth_secret_ref {
                        adapter = adapter.with_auth_handle(handle.clone());
                    }
                    adapters.push(adapter);
                }
                Err(err) => {
                    tracing::warn!(agent = id, error = %err, "cannot build an A2A adapter");
                }
            }
        }

        adapters
    }
}

#[async_trait]
impl AgentDiscovery for A2aDiscovery {
    fn source_name(&self) -> &str {
        "a2a-config"
    }

    async fn discover(&self) -> Result<Vec<AgentDescriptor>> {
        let mut descriptors = Vec::new();

        for adapter in self.adapters() {
            match adapter.refresh_from_card().await {
                Ok(descriptor) => descriptors.push(descriptor),
                Err(err) => {
                    tracing::warn!(
                        agent = %adapter.agent_id(),
                        error = %err,
                        "cannot reach an A2A agent's card; registering it as unavailable"
                    );

                    let mut descriptor = adapter.describe().await?;
                    descriptor.health.state = cuma_core::HealthState::Unavailable;
                    descriptor.health.last_error = Some(err.to_string());
                    descriptors.push(descriptor);
                }
            }
        }

        Ok(descriptors)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn a_cleartext_remote_endpoint_is_refused() {
        let err = A2aAdapter::new("remote", "http://example.invalid/a2a").unwrap_err();
        assert_eq!(err.class(), ErrorClass::SecurityViolation);
        assert!(err.to_string().contains("non-HTTPS"));
    }

    #[test]
    fn https_endpoints_are_accepted() {
        assert!(A2aAdapter::new("remote", "https://example.invalid/a2a").is_ok());
    }

    #[test]
    fn cleartext_localhost_is_allowed_for_development() {
        assert!(A2aAdapter::new("local", "http://localhost:8080/a2a").is_ok());
        assert!(A2aAdapter::new("local", "http://127.0.0.1:8080/a2a").is_ok());
    }

    #[test]
    fn a_host_that_merely_starts_with_localhost_is_not_treated_as_local() {
        // `localhost.evil.example` is a remote host with a misleading name.
        assert!(!is_local_endpoint("http://localhost.evil.example/a2a"));
        assert!(A2aAdapter::new("sneaky", "http://localhost.evil.example/a2a").is_err());
    }

    #[test]
    fn a_bracketed_ipv6_loopback_is_local() {
        assert!(is_local_endpoint("http://[::1]:8080/a2a"));
        assert!(!is_local_endpoint("http://[2001:db8::1]:8080/a2a"));
    }

    #[test]
    fn sse_events_are_split_on_blank_lines_whatever_the_line_endings() {
        let mut reader = SseReader::default();
        assert!(
            reader.feed(b"data: {\"a\":").is_empty(),
            "an incomplete event waits"
        );
        let events = reader.feed(b"1}\r\n\r\n: comment\n\ndata: x\ndata: y\n\n");
        assert_eq!(events, vec!["{\"a\":1}".to_owned(), "x\ny".to_owned()]);
    }

    #[test]
    fn a_follower_accumulates_appended_artifacts_and_stops_at_a_settled_state() {
        let mut follower = StreamFollower::default();
        follower.apply(StreamEvent::Status {
            state: TaskState::Working,
            text: None,
        });
        assert!(!follower.finished());
        follower.apply(StreamEvent::Artifact { text: "one".into() });
        follower.apply(StreamEvent::Artifact { text: "two".into() });
        follower.apply(StreamEvent::Status {
            state: TaskState::Completed,
            text: Some("done".into()),
        });
        assert!(follower.finished());
        assert_eq!(follower.into_task().text(), "one\ntwo\n\ndone");
    }

    #[test]
    fn a_follower_stops_when_the_peer_asks_for_input() {
        let mut follower = StreamFollower::default();
        follower.apply(StreamEvent::Status {
            state: TaskState::InputRequired,
            text: Some("which database?".into()),
        });
        assert!(
            follower.finished(),
            "waiting on the caller is as far as it goes"
        );
    }

    #[test]
    fn only_completed_counts_as_success() {
        assert!(failure_for(TaskState::Completed, "").is_none());
        for state in [
            TaskState::Failed,
            TaskState::Rejected,
            TaskState::Canceled,
            TaskState::InputRequired,
            TaskState::AuthRequired,
            TaskState::Working,
            TaskState::Unknown,
        ] {
            assert!(
                failure_for(state, "").is_some(),
                "{state:?} was treated as success"
            );
        }
    }

    #[test]
    fn a_request_for_input_is_reported_with_what_was_asked() {
        let (class, reason) = failure_for(TaskState::InputRequired, "which database?").unwrap();
        assert_eq!(class, ErrorClass::TaskFailure);
        assert!(reason.contains("which database?"));
    }

    #[test]
    fn an_auth_request_is_an_authentication_failure() {
        let (class, _) = failure_for(TaskState::AuthRequired, "").unwrap();
        assert_eq!(class, ErrorClass::AuthenticationFailure);
    }

    #[test]
    fn send_params_follow_the_dialect() {
        let v1 = send_params("go", Dialect::V1);
        assert_eq!(v1["message"]["role"], "ROLE_USER");
        assert_eq!(v1["configuration"]["returnImmediately"], false);

        let legacy = send_params("go", Dialect::Legacy);
        assert_eq!(legacy["message"]["role"], "user");
        assert_eq!(legacy["configuration"]["blocking"], true);
    }

    #[test]
    fn a_legacy_retry_rewrites_the_message_shape() {
        let rewritten = relegacy_params(methods::SEND_MESSAGE, send_params("go", Dialect::V1));
        assert_eq!(rewritten["message"]["parts"][0]["kind"], "text");
        assert_eq!(rewritten["message"]["parts"][0]["text"], "go");
    }

    #[test]
    fn json_rpc_errors_map_onto_failure_classes() {
        let protocol = JsonRpcError {
            code: -32601,
            message: "method not found".into(),
        };
        assert_eq!(protocol.class(), ErrorClass::ProtocolError);

        let task = JsonRpcError {
            code: -32001,
            message: "task not found".into(),
        };
        assert_eq!(task.class(), ErrorClass::TaskFailure);

        let unknown = JsonRpcError {
            code: -41000,
            message: "peer-specific".into(),
        };
        assert_eq!(unknown.class(), ErrorClass::Unknown);
    }

    #[test]
    fn discovery_skips_agents_with_no_endpoint() {
        let config =
            cuma_config::Config::from_toml("[agents.remote]\nprotocol = \"a2a\"\n").unwrap();
        assert!(A2aDiscovery::new(config).adapters().is_empty());
    }

    #[test]
    fn discovery_builds_an_adapter_for_a_configured_endpoint() {
        let config = cuma_config::Config::from_toml(
            r#"
            [agents.architect]
            protocol = "a2a"
            endpoint = "https://example.invalid/a2a"
            auth_secret_ref = "CUMA_ARCHITECT_TOKEN"
            "#,
        )
        .unwrap();

        let adapters = A2aDiscovery::new(config).adapters();
        assert_eq!(adapters.len(), 1);
        assert_eq!(adapters[0].endpoint(), "https://example.invalid/a2a");
        assert_eq!(
            adapters[0].auth_handle.as_deref(),
            Some("CUMA_ARCHITECT_TOKEN"),
            "only the handle is stored, never a token"
        );
    }

    #[tokio::test]
    async fn an_unreachable_agent_is_registered_as_unavailable_not_dropped() {
        let config = cuma_config::Config::from_toml(
            r#"
            [agents.unreachable]
            protocol = "a2a"
            endpoint = "https://127.0.0.1:1/a2a"
            "#,
        )
        .unwrap();

        let descriptors = A2aDiscovery::new(config).discover().await.unwrap();
        assert_eq!(descriptors.len(), 1);
        assert!(!descriptors[0].is_routable());
        assert!(descriptors[0].health.last_error.is_some());
    }
}
