//! Secret resolution.
//!
//! The domain stores a *handle* — `AgentAuth::SecretRef { handle }` — and never
//! a secret. This module turns a handle into a value at the moment of use, and
//! nothing else in the system is permitted to hold the result for longer than
//! the call that needed it.

use async_trait::async_trait;
use cuma_core::error::{MetaAgentError, Result};
use cuma_core::ports::SecretStore;
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

/// Resolves handles from environment variables.
///
/// The baseline store, and the one that works everywhere: CI, containers, a
/// developer's shell. An OS keychain is strictly better for an interactive
/// machine and slots in behind the same port.
#[derive(Debug, Clone, Default)]
pub struct EnvSecretStore;

impl EnvSecretStore {
    /// A store reading the process environment.
    pub fn new() -> Self {
        Self
    }
}

/// Treat a blank value as unset.
///
/// `export KEY=` is a common way to *disable* something. Accepting the empty
/// string as a credential produces a baffling 401 instead of the clear "no
/// credential is available" the operator needs.
fn usable(value: String) -> Option<String> {
    (!value.trim().is_empty()).then_some(value)
}

#[async_trait]
impl SecretStore for EnvSecretStore {
    async fn get(&self, handle: &str) -> Result<Option<String>> {
        Ok(std::env::var(handle).ok().and_then(usable))
    }

    async fn set(&self, handle: &str, _value: &str) -> Result<()> {
        // Writing to the process environment would affect every child process
        // the harness spawns — including agents — and would not persist. A
        // store that cannot honour `set` should say so rather than appear to.
        Err(MetaAgentError::Configuration(format!(
            "cannot store the secret for {handle:?}: set it as an environment variable instead"
        )))
    }
}

/// An in-memory store, for tests.
///
/// Deliberately not offered as a production store: a secret that lives only in
/// this process is one the operator has to supply on every start.
#[derive(Debug, Clone, Default)]
pub struct MemorySecretStore {
    secrets: Arc<RwLock<BTreeMap<String, String>>>,
}

impl MemorySecretStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// A store pre-populated with one secret.
    pub fn with(handle: &str, value: &str) -> Self {
        let store = Self::new();
        if let Ok(mut guard) = store.secrets.write() {
            guard.insert(handle.to_owned(), value.to_owned());
        }
        store
    }
}

#[async_trait]
impl SecretStore for MemorySecretStore {
    async fn get(&self, handle: &str) -> Result<Option<String>> {
        Ok(self
            .secrets
            .read()
            .ok()
            .and_then(|guard| guard.get(handle).cloned()))
    }

    async fn set(&self, handle: &str, value: &str) -> Result<()> {
        let mut guard = self
            .secrets
            .write()
            .map_err(|_| MetaAgentError::Other("the secret store lock is poisoned".to_owned()))?;
        guard.insert(handle.to_owned(), value.to_owned());
        Ok(())
    }
}

/// Tries several stores in order.
///
/// Lets a keychain-backed store take precedence while the environment stays
/// available as a fallback, which is what CI needs.
pub struct LayeredSecretStore {
    layers: Vec<Arc<dyn SecretStore>>,
}

impl LayeredSecretStore {
    /// A store consulting `layers` in order; the first hit wins.
    pub fn new(layers: Vec<Arc<dyn SecretStore>>) -> Self {
        Self { layers }
    }
}

#[async_trait]
impl SecretStore for LayeredSecretStore {
    async fn get(&self, handle: &str) -> Result<Option<String>> {
        for layer in &self.layers {
            // A failing layer must not hide a later one that would have
            // succeeded: one broken keychain should not lock the operator out.
            match layer.get(handle).await {
                Ok(Some(secret)) => return Ok(Some(secret)),
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(handle, error = %err, "a secret store layer failed");
                }
            }
        }
        Ok(None)
    }

    async fn set(&self, handle: &str, value: &str) -> Result<()> {
        for layer in &self.layers {
            if layer.set(handle, value).await.is_ok() {
                return Ok(());
            }
        }

        Err(MetaAgentError::Configuration(format!(
            "no configured secret store can store {handle:?}"
        )))
    }
}

/// Resolve a handle, or fail with a message naming what to set.
///
/// The error deliberately names the handle and not the value, and is phrased
/// as an instruction — an operator hitting this needs to know what to do, not
/// that something was `None`.
pub async fn require(secrets: &dyn SecretStore, handle: &str, provider: &str) -> Result<String> {
    secrets
        .get(handle)
        .await?
        .ok_or_else(|| MetaAgentError::Authentication {
            target: provider.to_owned(),
            message: format!("no credential is available for {handle:?}; set it and retry"),
        })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[tokio::test]
    async fn an_unset_handle_resolves_to_nothing_rather_than_an_error() {
        let store = EnvSecretStore::new();
        assert_eq!(
            store.get("CUMA_DEFINITELY_UNSET_9F3A2B").await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn the_environment_store_refuses_to_pretend_it_can_persist() {
        // Appearing to store a secret that vanishes on restart is worse than
        // refusing.
        let err = EnvSecretStore::new()
            .set("HANDLE", "value")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("environment variable"));
    }

    #[tokio::test]
    async fn an_in_memory_secret_round_trips() {
        let store = MemorySecretStore::new();
        store.set("KEY", "sk-test").await.unwrap();

        assert_eq!(store.get("KEY").await.unwrap().as_deref(), Some("sk-test"));
        assert_eq!(store.get("OTHER").await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_layered_store_returns_the_first_hit() {
        let layered = LayeredSecretStore::new(vec![
            Arc::new(MemorySecretStore::with("KEY", "from-first")),
            Arc::new(MemorySecretStore::with("KEY", "from-second")),
        ]);

        assert_eq!(
            layered.get("KEY").await.unwrap().as_deref(),
            Some("from-first")
        );
    }

    #[tokio::test]
    async fn a_layered_store_falls_through_to_a_later_layer() {
        let layered = LayeredSecretStore::new(vec![
            Arc::new(MemorySecretStore::new()),
            Arc::new(MemorySecretStore::with("KEY", "found")),
        ]);

        assert_eq!(layered.get("KEY").await.unwrap().as_deref(), Some("found"));
    }

    /// A store that always fails, to prove one broken layer is survivable.
    struct BrokenStore;

    #[async_trait]
    impl SecretStore for BrokenStore {
        async fn get(&self, _handle: &str) -> Result<Option<String>> {
            Err(MetaAgentError::Other("keychain is locked".to_owned()))
        }
        async fn set(&self, _handle: &str, _value: &str) -> Result<()> {
            Err(MetaAgentError::Other("keychain is locked".to_owned()))
        }
    }

    #[tokio::test]
    async fn a_broken_layer_does_not_hide_a_working_one() {
        let layered = LayeredSecretStore::new(vec![
            Arc::new(BrokenStore),
            Arc::new(MemorySecretStore::with("KEY", "found")),
        ]);

        assert_eq!(
            layered.get("KEY").await.unwrap().as_deref(),
            Some("found"),
            "one locked keychain must not lock the operator out"
        );
    }

    #[tokio::test]
    async fn a_missing_credential_produces_an_actionable_error() {
        let err = require(&MemorySecretStore::new(), "MY_KEY", "anthropic")
            .await
            .unwrap_err();

        assert_eq!(err.class(), cuma_core::ErrorClass::AuthenticationFailure);
        assert!(
            err.to_string().contains("MY_KEY"),
            "it must name what to set"
        );
    }

    #[tokio::test]
    async fn a_present_credential_is_returned() {
        let store = MemorySecretStore::with("MY_KEY", "sk-abc");
        assert_eq!(
            require(&store, "MY_KEY", "anthropic").await.unwrap(),
            "sk-abc"
        );
    }

    #[test]
    fn a_blank_environment_value_counts_as_unset() {
        // Tested through the filter rather than by mutating the process
        // environment: `set_var` is unsafe in edition 2024, and the workspace
        // forbids unsafe outright.
        assert_eq!(usable(String::new()), None);
        assert_eq!(usable("   ".to_owned()), None);
        assert_eq!(usable("\n\t".to_owned()), None);
        assert_eq!(usable("sk-real".to_owned()), Some("sk-real".to_owned()));
    }
}
