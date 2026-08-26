//! The A2A JSON-RPC client and adapter.

use crate::card::{AGENT_CARD_PATH, AgentCard, capabilities_from_card};
use async_trait::async_trait;
use cuma_core::error::{MetaAgentError, Result};
use cuma_core::ports::{AgentAdapter, AgentDiscovery, ExecutionRequest, ExecutionUpdate};
use cuma_core::{
    AgentDescriptor, AgentId, AgentProtocol, AttemptId, ErrorClass, ExecutionOutcome, TokenUsage,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};

/// The largest response body accepted from a peer, in bytes.
///
/// A remote agent is not trusted to bound its own output; without a cap, one
/// peer could exhaust the harness's memory by streaming indefinitely.
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// A JSON-RPC envelope, as returned by an A2A endpoint.
#[derive(Debug, Deserialize)]
struct JsonRpcResponse {
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

impl JsonRpcError {
    /// Map a JSON-RPC error onto a failure class.
    ///
    /// A2A layers its errors over JSON-RPC's reserved codes; anything outside
    /// the reserved range is a peer-specific code, classified from its message.
    fn class(&self) -> ErrorClass {
        match self.code {
            -32700 | -32600 | -32601 | -32602 => ErrorClass::ProtocolError,
            -32603 => ErrorClass::TaskFailure,
            -32001 => ErrorClass::TaskFailure,
            _ => cuma_core::ErrorClass::Unknown,
        }
    }
}

/// Reaches one remote agent over A2A.
pub struct A2aAdapter {
    id: AgentId,
    endpoint: String,
    http: reqwest::Client,
    descriptor: Arc<Mutex<AgentDescriptor>>,
    /// Handle for the bearer token, resolved at call time. Never the token.
    auth_handle: Option<String>,
}

impl A2aAdapter {
    /// An adapter for the agent at `endpoint`.
    pub fn new(id: impl Into<AgentId>, endpoint: impl Into<String>) -> Result<Self> {
        let id = id.into();
        let endpoint = endpoint.into();

        // Reject anything but HTTPS unless it is plainly a local endpoint.
        // Sending a task — and any context it carries — over cleartext to a
        // remote host is not something to do by accident.
        if !endpoint.starts_with("https://") && !is_local_endpoint(&endpoint) {
            return Err(MetaAgentError::Security(format!(
                "agent {id}: refusing a non-HTTPS A2A endpoint {endpoint:?}"
            )));
        }

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .user_agent("cuma/0.1")
            .build()
            .map_err(|err| {
                MetaAgentError::Configuration(format!("cannot build an HTTP client: {err}"))
            })?;

        let descriptor = AgentDescriptor::new(id.clone(), id.to_string(), AgentProtocol::A2A);

        Ok(Self {
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

    /// The endpoint this adapter talks to.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Fetch and parse the agent's card.
    pub async fn fetch_card(&self) -> Result<AgentCard> {
        let base = self.endpoint.trim_end_matches('/');
        let url = format!("{base}{AGENT_CARD_PATH}");

        let response = self.http.get(&url).send().await.map_err(|err| {
            MetaAgentError::protocol_msg("a2a", format!("cannot fetch {url}: {err}"))
        })?;

        if !response.status().is_success() {
            return Err(MetaAgentError::protocol_msg(
                "a2a",
                format!("{url} returned {}", response.status()),
            ));
        }

        let body = self.read_bounded(response).await?;

        serde_json::from_str(&body).map_err(|err| {
            MetaAgentError::protocol_msg("a2a", format!("{url} is not a valid Agent Card: {err}"))
        })
    }

    /// Read a response body, refusing anything over the size cap.
    async fn read_bounded(&self, response: reqwest::Response) -> Result<String> {
        if let Some(length) = response.content_length()
            && length > MAX_RESPONSE_BYTES as u64
        {
            return Err(MetaAgentError::Security(format!(
                "agent {}: response of {length} bytes exceeds the {MAX_RESPONSE_BYTES} byte cap",
                self.id
            )));
        }

        let body = response.text().await.map_err(|err| {
            MetaAgentError::protocol_msg("a2a", format!("cannot read the response body: {err}"))
        })?;

        if body.len() > MAX_RESPONSE_BYTES {
            return Err(MetaAgentError::Security(format!(
                "agent {}: response of {} bytes exceeds the {MAX_RESPONSE_BYTES} byte cap",
                self.id,
                body.len()
            )));
        }

        Ok(body)
    }

    /// Issue one JSON-RPC call.
    async fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": uuid::Uuid::new_v4().to_string(),
            "method": method,
            "params": params,
        });

        let mut request = self.http.post(&self.endpoint).json(&body);

        if let Some(handle) = &self.auth_handle {
            // Resolved from the environment here as the baseline secret store.
            // A keychain-backed store plugs in behind the same handle without
            // the token ever being written to config or logs.
            if let Ok(token) = std::env::var(handle) {
                request = request.bearer_auth(token);
            } else {
                tracing::warn!(
                    agent = %self.id,
                    handle,
                    "no secret is available for this handle; calling unauthenticated"
                );
            }
        }

        let response = request.send().await.map_err(|err| {
            let class = if err.is_timeout() {
                "timed out"
            } else if err.is_connect() {
                "connection refused"
            } else {
                "request failed"
            };
            MetaAgentError::protocol_msg("a2a", format!("{method} {class}: {err}"))
        })?;

        let status = response.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after_ms = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(|secs| secs.saturating_mul(1000));

            return Err(MetaAgentError::RateLimit {
                agent: self.id.clone(),
                retry_after_ms,
            });
        }

        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(MetaAgentError::Authentication {
                target: self.id.to_string(),
                message: format!("{method} returned {status}"),
            });
        }

        let body = self.read_bounded(response).await?;

        let envelope: JsonRpcResponse = serde_json::from_str(&body).map_err(|err| {
            MetaAgentError::protocol_msg(
                "a2a",
                format!("{method} returned invalid JSON-RPC: {err}"),
            )
        })?;

        if let Some(error) = envelope.error {
            return Err(MetaAgentError::agent(
                self.id.clone(),
                format!("{method}: {} (code {})", error.message, error.code),
                error.class(),
            ));
        }

        envelope.result.ok_or_else(|| {
            MetaAgentError::protocol_msg(
                "a2a",
                format!("{method} returned neither result nor error"),
            )
        })
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

/// Whether an endpoint is unambiguously on this machine or a private network.
fn is_local_endpoint(endpoint: &str) -> bool {
    let Some(rest) = endpoint.strip_prefix("http://") else {
        return false;
    };
    let host = rest
        .split('/')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("");

    host == "localhost" || host == "127.0.0.1" || host == "::1" || host == "[::1]"
}

/// Pull the assistant text out of an A2A result payload.
///
/// A2A responses vary by peer and by spec revision; rather than demand one
/// exact shape, this walks the documented locations in order and falls back to
/// the raw JSON, so a slightly-off peer degrades to verbose rather than broken.
fn extract_text(result: &serde_json::Value) -> String {
    let candidates = [
        result.pointer("/artifacts"),
        result.pointer("/status/message/parts"),
        result.pointer("/parts"),
        result.pointer("/message/parts"),
    ];

    for candidate in candidates.into_iter().flatten() {
        let mut collected = String::new();
        collect_text(candidate, &mut collected);
        if !collected.trim().is_empty() {
            return collected;
        }
    }

    result
        .get("result")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| result.to_string())
}

/// Recursively gather every `text` field in a JSON value.
fn collect_text(value: &serde_json::Value, out: &mut String) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::String(text)) = map.get("text") {
                out.push_str(text);
                out.push('\n');
            }
            for nested in map.values() {
                collect_text(nested, out);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_text(item, out);
            }
        }
        _ => {}
    }
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
        let started = std::time::Instant::now();

        let params = json!({
            "message": {
                "role": "user",
                "messageId": uuid::Uuid::new_v4().to_string(),
                "parts": [{ "kind": "text", "text": request.prompt }],
            },
        });

        let result = self.call("message/send", params).await?;
        let output = extract_text(&result);

        // Text arrives in one piece over this transport; forward it so the
        // event stream looks the same as it does for a streaming agent.
        let _ = updates
            .send(ExecutionUpdate::Text {
                content: output.clone(),
            })
            .await;

        // A peer may report the task as failed while the call itself succeeded.
        let state = result
            .pointer("/status/state")
            .and_then(|v| v.as_str())
            .unwrap_or("completed");

        let success = matches!(state, "completed" | "succeeded" | "input-required");

        #[allow(clippy::cast_possible_truncation)]
        let latency_ms = started.elapsed().as_millis() as u64;

        Ok(ExecutionOutcome {
            attempt_id: AttemptId::generate(),
            agent_id: self.id.clone(),
            model_id: request.model,
            success,
            output,
            changed_files: Vec::new(),
            // A2A does not report tokens; saying so beats inventing a number.
            tokens: TokenUsage::estimated(0, 0),
            latency_ms,
            failure_class: (!success).then_some(ErrorClass::TaskFailure),
            failure_reason: (!success).then(|| format!("remote task ended in state {state:?}")),
        })
    }

    async fn health_check(&self) -> Result<()> {
        self.fetch_card().await.map(|_| ())
    }
}

impl A2aAdapter {
    /// Fetch the card and fold what it says into the descriptor.
    pub async fn refresh_from_card(&self) -> Result<AgentDescriptor> {
        let card = self.fetch_card().await?;

        let mut descriptor = self.descriptor.lock().await;
        descriptor.name = card.name.clone();
        descriptor.capabilities = capabilities_from_card(&card);
        descriptor
            .metadata
            .insert("endpoint".to_owned(), self.endpoint.clone());
        descriptor
            .metadata
            .insert("a2a_version".to_owned(), card.version.clone());

        Ok(descriptor.clone())
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
    fn text_is_extracted_from_an_artifact_payload() {
        let result = json!({
            "artifacts": [{
                "parts": [
                    { "kind": "text", "text": "the answer is 42" }
                ]
            }]
        });
        assert!(extract_text(&result).contains("the answer is 42"));
    }

    #[test]
    fn text_is_extracted_from_a_status_message_payload() {
        let result = json!({
            "status": {
                "state": "completed",
                "message": {
                    "parts": [{ "kind": "text", "text": "done" }]
                }
            }
        });
        assert_eq!(extract_text(&result).trim(), "done");
    }

    #[test]
    fn an_unrecognised_payload_shape_degrades_to_raw_json_rather_than_empty() {
        let result = json!({ "somethingElse": true });
        let text = extract_text(&result);
        assert!(!text.is_empty());
        assert!(text.contains("somethingElse"));
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
