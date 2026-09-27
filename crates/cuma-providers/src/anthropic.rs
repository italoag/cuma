//! The Anthropic Messages API, behind [`LlmProvider`].
//!
//! Rust has no official Anthropic SDK, so this speaks the documented HTTP API
//! directly. The request shape is deliberately minimal — CUMA uses this for
//! single completions (planning, classification, summarization), not for
//! agentic loops, which is what ACP agents are for.
//!
//! [`LlmProvider`]: cuma_core::ports::LlmProvider

use crate::secrets;
use async_trait::async_trait;
use cuma_core::error::{MetaAgentError, Result};
use cuma_core::ports::{LlmProvider, SecretStore};
use cuma_core::{AgentId, CostProfile, Known, ModelDescriptor, ModelId};
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;

const API_URL: &str = "https://api.anthropic.com/v1/messages";

/// The API version header. Pinned, not tracked: an unpinned version means a
/// server-side change can alter behaviour without a deploy here.
const API_VERSION: &str = "2023-06-01";

/// Output cap for a harness completion.
///
/// The harness's own reasoning produces plans and summaries, not documents.
/// A generous cap here would only widen the blast radius of a runaway
/// response; anything longer belongs to a coding agent, not to this path.
const MAX_TOKENS: u32 = 8_192;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// The default model for harness reasoning.
const DEFAULT_MODEL: &str = "claude-opus-5";

/// Models this provider exposes, with their published pricing.
///
/// Pricing is recorded so the router can score cost and the usage ledger can
/// report spend. It is marked [`Known::Reported`] because it comes from the
/// provider's published rates rather than from a guess — but it is a *static*
/// table, so it will drift; `cuma models list` shows what is in effect.
const MODELS: &[(&str, &str, u64, f64, f64)] = &[
    // (id, display name, context window, $/Mtok in, $/Mtok out)
    ("claude-opus-5", "Claude Opus 5", 1_000_000, 5.0, 25.0),
    ("claude-sonnet-5", "Claude Sonnet 5", 1_000_000, 2.0, 10.0),
    ("claude-haiku-4-5", "Claude Haiku 4.5", 200_000, 1.0, 5.0),
];

/// A completion response.
#[derive(Debug, Deserialize)]
struct MessagesResponse {
    #[serde(default)]
    content: Vec<ContentBlock>,
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    /// Thinking blocks arrive when adaptive thinking is on. They are not the
    /// answer, so they are parsed and discarded rather than concatenated into
    /// the caller's result.
    #[serde(rename = "thinking")]
    Thinking {
        #[serde(default)]
        #[allow(dead_code)]
        thinking: String,
    },
    /// Anything else the API adds later.
    #[serde(other)]
    Other,
}

/// An API error body.
#[derive(Debug, Deserialize)]
struct ApiError {
    error: ApiErrorDetail,
}

#[derive(Debug, Deserialize)]
struct ApiErrorDetail {
    #[serde(rename = "type")]
    kind: String,
    message: String,
}

/// Direct access to Anthropic models.
pub struct AnthropicProvider {
    http: reqwest::Client,
    secret_handle: String,
    secrets: Arc<dyn SecretStore>,
    default_model: ModelId,
}

impl AnthropicProvider {
    /// A provider whose credential is resolved from `secret_handle`.
    pub fn new(secret_handle: impl Into<String>, secrets: Arc<dyn SecretStore>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .user_agent("cuma/0.1")
                .build()
                .unwrap_or_default(),
            secret_handle: secret_handle.into(),
            secrets,
            default_model: ModelId::new(DEFAULT_MODEL),
        }
    }

    /// Override the model used when a caller names none.
    #[must_use]
    pub fn with_default_model(mut self, model: impl Into<ModelId>) -> Self {
        self.default_model = model.into();
        self
    }

    /// Map an HTTP status onto a failure class.
    ///
    /// This is what lets the resilience layer treat a 429 as transient and a
    /// 401 as terminal, rather than retrying both identically.
    fn classify(status: reqwest::StatusCode, body: &str) -> MetaAgentError {
        let detail = serde_json::from_str::<ApiError>(body)
            .map(|e| format!("{}: {}", e.error.kind, e.error.message))
            .unwrap_or_else(|_| body.chars().take(300).collect());

        match status.as_u16() {
            401 | 403 => MetaAgentError::Authentication {
                target: "anthropic".to_owned(),
                message: detail,
            },
            429 => MetaAgentError::RateLimit {
                agent: AgentId::new("anthropic"),
                retry_after_ms: None,
            },
            // 529 is Anthropic's "overloaded"; it is retryable like a 5xx.
            500..=599 => MetaAgentError::agent(
                AgentId::new("anthropic"),
                detail,
                cuma_core::ErrorClass::ModelUnavailable,
            ),
            413 => MetaAgentError::agent(
                AgentId::new("anthropic"),
                detail,
                cuma_core::ErrorClass::ContextOverflow,
            ),
            _ => MetaAgentError::agent(
                AgentId::new("anthropic"),
                detail,
                cuma_core::ErrorClass::InvalidResponse,
            ),
        }
    }

    /// Concatenate the text blocks of a response.
    ///
    /// Thinking blocks are dropped: they are reasoning, not the answer, and
    /// concatenating them would put the model's scratchpad into a plan.
    fn extract_text(response: &MessagesResponse) -> String {
        response
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    async fn models(&self) -> Result<Vec<ModelDescriptor>> {
        let agent_id = AgentId::new("anthropic");

        Ok(MODELS
            .iter()
            .map(|(id, name, window, input, output)| {
                let mut model = ModelDescriptor::minimal(agent_id.clone(), *id, *name);
                model.provider = Some("anthropic".to_owned());
                model.context_window = Known::Reported(*window);
                model.cost = CostProfile {
                    input_per_mtok: Known::Reported(*input),
                    output_per_mtok: Known::Reported(*output),
                    cache_read_per_mtok: Known::Unknown,
                };
                model
            })
            .collect())
    }

    async fn complete(&self, system: &str, user: &str, model: Option<&ModelId>) -> Result<String> {
        let key = secrets::require(self.secrets.as_ref(), &self.secret_handle, "anthropic").await?;
        let model = model.unwrap_or(&self.default_model);

        let body = serde_json::json!({
            "model": model.as_str(),
            "max_tokens": MAX_TOKENS,
            "system": system,
            "messages": [{ "role": "user", "content": user }],
        });

        let response = self
            .http
            .post(API_URL)
            .header("x-api-key", key)
            .header("anthropic-version", API_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|err| {
                if err.is_timeout() {
                    MetaAgentError::Timeout {
                        operation: "anthropic completion".to_owned(),
                        elapsed_ms: REQUEST_TIMEOUT.as_millis() as u64,
                    }
                } else {
                    MetaAgentError::agent(
                        AgentId::new("anthropic"),
                        err.to_string(),
                        cuma_core::ErrorClass::ConnectionFailure,
                    )
                }
            })?;

        let status = response.status();
        let text = response.text().await.map_err(|err| {
            MetaAgentError::protocol_msg("http", format!("cannot read the response body: {err}"))
        })?;

        if !status.is_success() {
            return Err(Self::classify(status, &text));
        }

        let parsed: MessagesResponse = serde_json::from_str(&text).map_err(|err| {
            MetaAgentError::agent(
                AgentId::new("anthropic"),
                format!("unparseable response: {err}"),
                cuma_core::ErrorClass::InvalidResponse,
            )
        })?;

        // Hitting the output cap means the reply is cut off mid-thought.
        // Returning it as though it were complete would put a truncated plan
        // into the orchestrator.
        if parsed.stop_reason.as_deref() == Some("max_tokens") {
            return Err(MetaAgentError::agent(
                AgentId::new("anthropic"),
                "the reply hit the output limit and is incomplete".to_owned(),
                cuma_core::ErrorClass::ContextOverflow,
            ));
        }

        let output = Self::extract_text(&parsed);
        if output.trim().is_empty() {
            return Err(MetaAgentError::agent(
                AgentId::new("anthropic"),
                "the reply contained no text".to_owned(),
                cuma_core::ErrorClass::InvalidResponse,
            ));
        }

        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::secrets::MemorySecretStore;
    use cuma_core::ErrorClass;

    fn provider() -> AnthropicProvider {
        AnthropicProvider::new("KEY", Arc::new(MemorySecretStore::with("KEY", "sk-test")))
    }

    #[tokio::test]
    async fn the_provider_reports_its_models_with_published_pricing() {
        let models = provider().models().await.unwrap();

        assert!(!models.is_empty());
        let opus = models
            .iter()
            .find(|m| m.id == ModelId::new("claude-opus-5"))
            .expect("the default model should be listed");

        assert_eq!(opus.cost.input_per_mtok.value(), Some(5.0));
        assert_eq!(opus.cost.output_per_mtok.value(), Some(25.0));
        assert_eq!(opus.context_window.value(), Some(1_000_000));
    }

    #[tokio::test]
    async fn a_missing_credential_fails_before_any_request_is_made() {
        let provider = AnthropicProvider::new("KEY", Arc::new(MemorySecretStore::new()));

        let err = provider.complete("system", "user", None).await.unwrap_err();
        assert_eq!(err.class(), ErrorClass::AuthenticationFailure);
        assert!(err.to_string().contains("KEY"));
    }

    #[test]
    fn http_statuses_map_onto_the_classes_resilience_branches_on() {
        use reqwest::StatusCode;

        // The distinction that matters: 429 backs off, 401 gives up.
        assert_eq!(
            AnthropicProvider::classify(StatusCode::TOO_MANY_REQUESTS, "{}").class(),
            ErrorClass::RateLimit
        );
        assert_eq!(
            AnthropicProvider::classify(StatusCode::UNAUTHORIZED, "{}").class(),
            ErrorClass::AuthenticationFailure
        );
        assert_eq!(
            AnthropicProvider::classify(StatusCode::INTERNAL_SERVER_ERROR, "{}").class(),
            ErrorClass::ModelUnavailable
        );
        assert_eq!(
            AnthropicProvider::classify(StatusCode::PAYLOAD_TOO_LARGE, "{}").class(),
            ErrorClass::ContextOverflow
        );
    }

    #[test]
    fn a_structured_api_error_is_surfaced_rather_than_raw_json() {
        let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"messages: roles must alternate"}}"#;
        let err = AnthropicProvider::classify(reqwest::StatusCode::BAD_REQUEST, body);

        assert!(err.to_string().contains("invalid_request_error"));
        assert!(err.to_string().contains("roles must alternate"));
    }

    #[test]
    fn an_unparseable_error_body_still_produces_a_bounded_message() {
        let err =
            AnthropicProvider::classify(reqwest::StatusCode::BAD_GATEWAY, &"<html>".repeat(1000));
        assert!(
            err.to_string().len() < 500,
            "an HTML error page must not flood the log"
        );
    }

    #[test]
    fn text_blocks_are_concatenated() {
        let response: MessagesResponse = serde_json::from_str(
            r#"{"content":[{"type":"text","text":"one "},{"type":"text","text":"two"}]}"#,
        )
        .unwrap();

        assert_eq!(AnthropicProvider::extract_text(&response), "one two");
    }

    #[test]
    fn thinking_blocks_are_dropped_rather_than_treated_as_the_answer() {
        // Concatenating the model's scratchpad into a plan would be worse than
        // returning nothing.
        let response: MessagesResponse = serde_json::from_str(
            r#"{"content":[
                {"type":"thinking","thinking":"let me consider..."},
                {"type":"text","text":"the plan"}
            ]}"#,
        )
        .unwrap();

        assert_eq!(AnthropicProvider::extract_text(&response), "the plan");
    }

    #[test]
    fn an_unrecognised_block_type_does_not_break_parsing() {
        let response: MessagesResponse = serde_json::from_str(
            r#"{"content":[{"type":"something_new","data":1},{"type":"text","text":"ok"}]}"#,
        )
        .unwrap();

        assert_eq!(AnthropicProvider::extract_text(&response), "ok");
    }

    #[test]
    fn the_default_model_is_the_current_flagship() {
        assert_eq!(DEFAULT_MODEL, "claude-opus-5");
        assert!(MODELS.iter().any(|(id, ..)| *id == DEFAULT_MODEL));
    }
}
