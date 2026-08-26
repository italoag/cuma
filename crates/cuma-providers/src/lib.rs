//! Direct model access, behind the [`LlmProvider`] port.
//!
//! ## What this is for, and what it is not
//!
//! CUMA talks to *coding agents* over ACP and A2A. Those agents manage their
//! own credentials and run their own loops, which is why the harness needs no
//! API key to drive them (see ADR-002).
//!
//! This crate is for the harness's **own** reasoning — planning, classifying,
//! summarizing — where there is no agent to delegate to and a single
//! completion is the whole job. It is never a substitute for ACP or A2A.
//!
//! Everything here sits behind [`LlmProvider`], so no provider SDK call
//! escapes this crate. That is the rule the design exists to protect.
//!
//! ## Credentials
//!
//! API keys are resolved from a [`SecretStore`] at the point of use. Only a
//! *handle* is ever stored in configuration or in a descriptor.
//!
//! [`LlmProvider`]: cuma_core::ports::LlmProvider
//! [`SecretStore`]: cuma_core::ports::SecretStore

mod anthropic;
mod openai_compatible;
mod secrets;

pub use anthropic::AnthropicProvider;
pub use openai_compatible::OpenAiCompatibleProvider;
pub use secrets::{EnvSecretStore, LayeredSecretStore, MemorySecretStore};

use cuma_core::ports::{LlmProvider, SecretStore};
use std::sync::Arc;

/// Build the providers configuration asks for.
///
/// Returns an empty list when none are configured, which is the normal case:
/// the harness's default planner is heuristic and needs no model at all.
pub fn from_config(
    config: &cuma_config::Config,
    secrets: Arc<dyn SecretStore>,
) -> Vec<Arc<dyn LlmProvider>> {
    let mut providers: Vec<Arc<dyn LlmProvider>> = Vec::new();

    for (id, agent) in &config.agents {
        if !agent.enabled || !agent.protocol.eq_ignore_ascii_case("native") {
            continue;
        }

        let Some(kind) = agent.metadata.get("provider") else {
            continue;
        };

        let handle = agent
            .auth_secret_ref
            .clone()
            .unwrap_or_else(|| default_handle(kind));

        match kind.to_ascii_lowercase().as_str() {
            "anthropic" => {
                providers.push(Arc::new(AnthropicProvider::new(
                    handle,
                    Arc::clone(&secrets),
                )));
            }
            "openai" | "openai-compatible" | "openrouter" => {
                let endpoint = agent
                    .endpoint
                    .clone()
                    .unwrap_or_else(|| openai_compatible::DEFAULT_ENDPOINT.to_owned());

                providers.push(Arc::new(OpenAiCompatibleProvider::new(
                    id.clone(),
                    endpoint,
                    handle,
                    Arc::clone(&secrets),
                )));
            }
            other => {
                tracing::warn!(
                    agent = id,
                    provider = other,
                    "unknown provider kind; skipping"
                );
            }
        }
    }

    providers
}

/// The environment variable a provider conventionally reads.
fn default_handle(kind: &str) -> String {
    match kind.to_ascii_lowercase().as_str() {
        "anthropic" => "ANTHROPIC_API_KEY",
        "openrouter" => "OPENROUTER_API_KEY",
        _ => "OPENAI_API_KEY",
    }
    .to_owned()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn no_configured_providers_is_the_normal_case() {
        // The default planner is heuristic and needs no model at all.
        let providers = from_config(
            &cuma_config::Config::default(),
            Arc::new(EnvSecretStore::new()),
        );
        assert!(providers.is_empty());
    }

    #[test]
    fn a_configured_anthropic_provider_is_built() {
        let config = cuma_config::Config::from_toml(
            r#"
            [agents.planner]
            protocol = "native"
            metadata = { provider = "anthropic" }
            "#,
        )
        .unwrap();

        let providers = from_config(&config, Arc::new(EnvSecretStore::new()));
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].name(), "anthropic");
    }

    #[test]
    fn an_unknown_provider_kind_is_skipped_rather_than_guessed_at() {
        let config = cuma_config::Config::from_toml(
            r#"
            [agents.mystery]
            protocol = "native"
            metadata = { provider = "some-new-vendor" }
            "#,
        )
        .unwrap();

        assert!(from_config(&config, Arc::new(EnvSecretStore::new())).is_empty());
    }

    #[test]
    fn acp_and_a2a_agents_are_not_treated_as_providers() {
        // Direct provider access must never shadow the protocol path.
        let config = cuma_config::Config::from_toml(
            r#"
            [agents.codex]
            protocol = "acp"
            metadata = { provider = "openai" }
            "#,
        )
        .unwrap();

        assert!(from_config(&config, Arc::new(EnvSecretStore::new())).is_empty());
    }

    #[test]
    fn each_provider_kind_has_a_conventional_credential_handle() {
        assert_eq!(default_handle("anthropic"), "ANTHROPIC_API_KEY");
        assert_eq!(default_handle("openrouter"), "OPENROUTER_API_KEY");
        assert_eq!(default_handle("openai"), "OPENAI_API_KEY");
    }
}
