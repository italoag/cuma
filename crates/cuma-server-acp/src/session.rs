//! ACP session tracking.
//!
//! ACP sessions are the client's unit of conversation. CUMA keeps only what it
//! needs to honour the protocol — the working directory the client chose, and a
//! short history of what each prompt produced.
//!
//! Deliberately *not* kept here: the task graph, routing state or usage. Those
//! belong to the orchestrator and the runtime database. Duplicating them into a
//! protocol adapter is how a second, diverging source of truth gets created.

use agent_client_protocol::schema::v1::SessionId;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

/// How many past turns a session remembers.
///
/// Bounded because an editor may keep one session open for hours; the full
/// record lives in the runtime database.
const MAX_HISTORY: usize = 50;

/// One ACP session.
#[derive(Debug, Clone)]
pub struct SessionState {
    /// The working directory the client chose.
    pub workspace: PathBuf,
    /// Summaries of past turns, oldest first.
    pub history: Vec<String>,
    /// When the session was created.
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl SessionState {
    /// A new session rooted at `workspace`.
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            history: Vec::new(),
            created_at: chrono::Utc::now(),
        }
    }

    /// Record what a turn produced, dropping the oldest entry when full.
    pub fn record(&mut self, summary: impl Into<String>) {
        self.history.push(summary.into());
        if self.history.len() > MAX_HISTORY {
            let excess = self.history.len() - MAX_HISTORY;
            self.history.drain(0..excess);
        }
    }
}

/// The sessions an ACP client has open.
#[derive(Debug, Clone, Default)]
pub struct SessionRegistry {
    sessions: Arc<RwLock<BTreeMap<String, SessionState>>>,
}

impl SessionRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a session and return its id.
    pub async fn create(&self, workspace: PathBuf) -> SessionId {
        let id = SessionId::new(format!("cuma-{}", uuid_like()));
        self.sessions
            .write()
            .await
            .insert(id.to_string(), SessionState::new(workspace));
        id
    }

    /// The workspace a session was created in.
    ///
    /// `None` for an unknown session — a client that prompts against a session
    /// it never created gets the process's own directory rather than an error,
    /// because failing the turn over bookkeeping helps nobody.
    pub async fn workspace(&self, id: &SessionId) -> Option<PathBuf> {
        self.sessions
            .read()
            .await
            .get(&id.to_string())
            .map(|state| state.workspace.clone())
    }

    /// Record what a turn produced.
    pub async fn record(&self, id: &SessionId, summary: &str) {
        if let Some(state) = self.sessions.write().await.get_mut(&id.to_string()) {
            state.record(summary);
        }
    }

    /// Fetch a session.
    pub async fn get(&self, id: &SessionId) -> Option<SessionState> {
        self.sessions.read().await.get(&id.to_string()).cloned()
    }

    /// Forget a session.
    pub async fn remove(&self, id: &SessionId) -> bool {
        self.sessions
            .write()
            .await
            .remove(&id.to_string())
            .is_some()
    }

    /// How many sessions are open.
    pub async fn len(&self) -> usize {
        self.sessions.read().await.len()
    }

    /// Whether no sessions are open.
    pub async fn is_empty(&self) -> bool {
        self.sessions.read().await.is_empty()
    }
}

/// A session identifier.
///
/// Uses the same UUID machinery as the rest of the domain rather than a
/// counter, so ids stay unique across restarts — an editor reconnecting must
/// not be handed an id that means something else.
fn uuid_like() -> String {
    cuma_core::SessionId::generate()
        .as_str()
        .trim_start_matches("session_")
        .to_owned()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[tokio::test]
    async fn creating_a_session_records_its_workspace() {
        let registry = SessionRegistry::new();
        assert!(registry.is_empty().await);

        let id = registry.create(PathBuf::from("/projects/app")).await;

        assert_eq!(registry.len().await, 1);
        assert_eq!(
            registry.workspace(&id).await,
            Some(PathBuf::from("/projects/app"))
        );
    }

    #[tokio::test]
    async fn session_ids_are_unique() {
        let registry = SessionRegistry::new();
        let a = registry.create(PathBuf::from(".")).await;
        let b = registry.create(PathBuf::from(".")).await;

        assert_ne!(a.to_string(), b.to_string());
        assert_eq!(registry.len().await, 2);
    }

    #[tokio::test]
    async fn an_unknown_session_has_no_workspace_rather_than_erroring() {
        let registry = SessionRegistry::new();
        let unknown = SessionId::new("never-created");
        assert_eq!(registry.workspace(&unknown).await, None);
    }

    #[tokio::test]
    async fn turns_accumulate_into_a_session_history() {
        let registry = SessionRegistry::new();
        let id = registry.create(PathBuf::from(".")).await;

        registry.record(&id, "4/4 tasks completed").await;
        registry.record(&id, "2/3 tasks completed, 1 failed").await;

        let state = registry.get(&id).await.unwrap();
        assert_eq!(state.history.len(), 2);
        assert!(state.history[1].contains("failed"));
    }

    #[tokio::test]
    async fn recording_against_an_unknown_session_is_a_no_op() {
        let registry = SessionRegistry::new();
        registry.record(&SessionId::new("ghost"), "something").await;
        assert!(registry.is_empty().await);
    }

    #[test]
    fn history_is_bounded_so_a_long_lived_session_cannot_grow_without_limit() {
        let mut state = SessionState::new(PathBuf::from("."));

        for i in 0..(MAX_HISTORY + 20) {
            state.record(format!("turn {i}"));
        }

        assert_eq!(state.history.len(), MAX_HISTORY);
        assert!(
            state
                .history
                .last()
                .unwrap()
                .contains(&(MAX_HISTORY + 19).to_string()),
            "the newest turns must be the ones kept"
        );
    }

    #[tokio::test]
    async fn a_session_can_be_removed() {
        let registry = SessionRegistry::new();
        let id = registry.create(PathBuf::from(".")).await;

        assert!(registry.remove(&id).await);
        assert!(registry.is_empty().await);
        assert!(!registry.remove(&id).await);
    }
}
