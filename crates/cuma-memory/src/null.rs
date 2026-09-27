//! The no-op memory store.

use async_trait::async_trait;
use cuma_core::error::Result;
use cuma_core::ports::{MemoryEntry, MemoryStore};

/// A memory store that remembers nothing.
///
/// This is the default. Running with no memory backend is a supported
/// configuration, not a degraded one: the harness works without recall, it
/// just works better with it.
#[derive(Debug, Clone, Copy, Default)]
pub struct NullMemory;

#[async_trait]
impl MemoryStore for NullMemory {
    async fn is_available(&self) -> bool {
        false
    }

    async fn recall(&self, _query: &str, _limit: usize) -> Result<Vec<MemoryEntry>> {
        Ok(Vec::new())
    }

    async fn remember(&self, _content: &str, _kind: &str) -> Result<String> {
        // Reporting success would let a caller believe a memory was stored.
        // Reporting failure would make every session log an error. Neither is
        // right, so this returns a sentinel id that says what happened.
        Ok("not-stored:memory-disabled".to_owned())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[tokio::test]
    async fn the_null_store_reports_itself_unavailable() {
        assert!(!NullMemory.is_available().await);
    }

    #[tokio::test]
    async fn recall_returns_nothing_rather_than_failing() {
        assert!(NullMemory.recall("anything", 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn remember_is_honest_about_not_having_stored_anything() {
        let id = NullMemory.remember("something", "note").await.unwrap();
        assert!(id.starts_with("not-stored:"));
    }
}
