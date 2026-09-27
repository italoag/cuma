//! An OpenAI-compatible chat-completions provider.
//!
//! Covers OpenAI itself, OpenRouter, and the many local servers that speak the
//! same `/chat/completions` shape (llama.cpp, vLLM, Ollama's compatibility
//! endpoint). One adapter rather than one per vendor, because the wire format
//! is the same and the differences are the endpoint and the credential.
//!
//! Model pricing is **not** hardcoded here. Unlike a first-party provider, an
//! OpenAI-compatible endpoint may be anything — a local model that costs
//! nothing, or a gateway with its own rates — so pricing is left
//! [`Known::Unknown`] and the router scores it neutrally rather than
//! pretending a local model is expensive or a gateway is free.

use crate::secrets;
use async_trait::async_trait;
use cuma_core::error::{MetaAgentError, Result};
use cuma_core::ports::{LlmProvider, SecretStore};
use cuma_core::{AgentId, ModelDescriptor, ModelId};
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;

/// Where OpenAI itself lives.
pub const DEFAULT_ENDPOINT: &str = "https://api.openai.com/v1/chat/completions";

const MAX_TOKENS: u32 = 8_192;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    #[serde(default)]
    message: Option<ChatMessage>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatMessage {
    #[serde(default)]
    content: Option<String>,
}

/// A provider speaking the OpenAI chat-completions shape.
pub struct OpenAiCompatibleProvider {
    name: String,
    endpoint: String,
    http: reqwest::Client,
    secret_handle: String,
    secrets: Arc<dyn SecretStore>,
    default_model: ModelId,
    /// Models the operator declared, since an arbitrary endpoint cannot be
    /// asked what it serves in a uniform way.
    declared_models: Vec<ModelId>,
}

impl OpenAiCompatibleProvider {
    /// A provider named `name`, talking to `endpoint`.
    pub fn new(
        name: impl Into<String>,
        endpoint: impl Into<String>,
        secret_handle: impl Into<String>,
        secrets: Arc<dyn SecretStore>,
    ) -> Self {
        Self {
            name: name.into(),
            endpoint: endpoint.into(),
            http: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .user_agent("cuma/0.1")
                .build()
                .unwrap_or_default(),
            secret_handle: secret_handle.into(),
            secrets,
            default_model: ModelId::new("gpt-4o-mini"),
            declared_models: Vec::new(),
        }
    }

    /// Declare which models this endpoint serves.
    #[must_use]
    pub fn with_models(mut self, models: Vec<ModelId>) -> Self {
        if let Some(first) = models.first() {
            self.default_model = first.clone();
        }
        self.declared_models = models;
        self
    }

    /// The endpoint this provider talks to.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn classify(&self, status: reqwest::StatusCode, body: &str) -> MetaAgentError {
        let detail: String = body.chars().take(300).collect();
        let agent = AgentId::new(self.name.clone());

        match status.as_u16() {
            401 | 403 => MetaAgentError::Authentication {
                target: self.name.clone(),
                message: detail,
            },
            429 => MetaAgentError::RateLimit {
                agent,
                retry_after_ms: None,
            },
            500..=599 => {
                MetaAgentError::agent(agent, detail, cuma_core::ErrorClass::ModelUnavailable)
            }
            413 => MetaAgentError::agent(agent, detail, cuma_core::ErrorClass::ContextOverflow),
            _ => MetaAgentError::agent(agent, detail, cuma_core::ErrorClass::InvalidResponse),
        }
    }
}

#[async_trait]
impl LlmProvider for OpenAiCompatibleProvider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn models(&self) -> Result<Vec<ModelDescriptor>> {
        let agent_id = AgentId::new(self.name.clone());

        Ok(self
            .declared_models
            .iter()
            .map(|id| {
                let mut model =
                    ModelDescriptor::minimal(agent_id.clone(), id.clone(), id.to_string());
                model.provider = Some(self.name.clone());
                // Pricing and context window stay Unknown: this endpoint could
                // be a free local model or a paid gateway, and guessing either
                // way would mislead the router.
                model
            })
            .collect())
    }

    async fn complete(&self, system: &str, user: &str, model: Option<&ModelId>) -> Result<String> {
        let key = secrets::require(self.secrets.as_ref(), &self.secret_handle, &self.name).await?;
        let model = model.unwrap_or(&self.default_model);

        let body = serde_json::json!({
            "model": model.as_str(),
            "max_tokens": MAX_TOKENS,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user },
            ],
        });

        let response = self
            .http
            .post(&self.endpoint)
            .bearer_auth(key)
            .json(&body)
            .send()
            .await
            .map_err(|err| {
                if err.is_timeout() {
                    MetaAgentError::Timeout {
                        operation: format!("{} completion", self.name),
                        elapsed_ms: REQUEST_TIMEOUT.as_millis() as u64,
                    }
                } else {
                    MetaAgentError::agent(
                        AgentId::new(self.name.clone()),
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
            return Err(self.classify(status, &text));
        }

        let parsed: ChatResponse = serde_json::from_str(&text).map_err(|err| {
            MetaAgentError::agent(
                AgentId::new(self.name.clone()),
                format!("unparseable response: {err}"),
                cuma_core::ErrorClass::InvalidResponse,
            )
        })?;

        let Some(choice) = parsed.choices.first() else {
            return Err(MetaAgentError::agent(
                AgentId::new(self.name.clone()),
                "the reply contained no choices".to_owned(),
                cuma_core::ErrorClass::InvalidResponse,
            ));
        };

        // A truncated reply must not be passed off as a complete one.
        if choice.finish_reason.as_deref() == Some("length") {
            return Err(MetaAgentError::agent(
                AgentId::new(self.name.clone()),
                "the reply hit the output limit and is incomplete".to_owned(),
                cuma_core::ErrorClass::ContextOverflow,
            ));
        }

        let content = choice
            .message
            .as_ref()
            .and_then(|m| m.content.clone())
            .unwrap_or_default();

        if content.trim().is_empty() {
            return Err(MetaAgentError::agent(
                AgentId::new(self.name.clone()),
                "the reply contained no text".to_owned(),
                cuma_core::ErrorClass::InvalidResponse,
            ));
        }

        Ok(content)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::secrets::MemorySecretStore;
    use cuma_core::ErrorClass;

    fn provider() -> OpenAiCompatibleProvider {
        OpenAiCompatibleProvider::new(
            "openai",
            DEFAULT_ENDPOINT,
            "KEY",
            Arc::new(MemorySecretStore::with("KEY", "sk-test")),
        )
    }

    #[tokio::test]
    async fn a_missing_credential_fails_before_any_request_is_made() {
        let provider = OpenAiCompatibleProvider::new(
            "openai",
            DEFAULT_ENDPOINT,
            "KEY",
            Arc::new(MemorySecretStore::new()),
        );

        assert_eq!(
            provider.complete("s", "u", None).await.unwrap_err().class(),
            ErrorClass::AuthenticationFailure
        );
    }

    #[tokio::test]
    async fn pricing_is_unknown_rather_than_guessed_at() {
        // This endpoint could be a free local model or a paid gateway.
        let provider = provider().with_models(vec![ModelId::new("gpt-4o")]);
        let models = provider.models().await.unwrap();

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].cost.input_per_mtok.value(), None);
        assert_eq!(models[0].context_window.value(), None);
    }

    #[tokio::test]
    async fn declaring_models_sets_the_first_as_the_default() {
        let provider = provider().with_models(vec![
            ModelId::new("local-llama"),
            ModelId::new("local-mistral"),
        ]);
        assert_eq!(provider.default_model, ModelId::new("local-llama"));
    }

    #[tokio::test]
    async fn an_endpoint_with_no_declared_models_reports_none() {
        assert!(provider().models().await.unwrap().is_empty());
    }

    #[test]
    fn http_statuses_map_onto_the_classes_resilience_branches_on() {
        use reqwest::StatusCode;
        let provider = provider();

        assert_eq!(
            provider.classify(StatusCode::TOO_MANY_REQUESTS, "").class(),
            ErrorClass::RateLimit
        );
        assert_eq!(
            provider.classify(StatusCode::UNAUTHORIZED, "").class(),
            ErrorClass::AuthenticationFailure
        );
        assert_eq!(
            provider
                .classify(StatusCode::SERVICE_UNAVAILABLE, "")
                .class(),
            ErrorClass::ModelUnavailable
        );
    }

    #[test]
    fn a_local_endpoint_is_accepted() {
        // The common case for llama.cpp, vLLM and Ollama.
        let provider = OpenAiCompatibleProvider::new(
            "local",
            "http://localhost:8080/v1/chat/completions",
            "KEY",
            Arc::new(MemorySecretStore::new()),
        );
        assert!(provider.endpoint().starts_with("http://localhost"));
    }

    #[test]
    fn a_response_shape_missing_optional_fields_still_parses() {
        let parsed: ChatResponse = serde_json::from_str(r#"{"choices":[{"index":0}]}"#).unwrap();
        assert_eq!(parsed.choices.len(), 1);
        assert!(parsed.choices[0].message.is_none());
    }
}
