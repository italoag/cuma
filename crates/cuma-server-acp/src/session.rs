//! ACP session tracking.
//!
//! ACP sessions are the client's unit of conversation. CUMA keeps only what it
//! needs to honour the protocol — the working directory the client chose, the
//! turns exchanged so a reconnecting client can be shown them again, and a
//! handle on the prompt currently running so it can be cancelled.
//!
//! Deliberately *not* kept here: the task graph, routing state or usage. Those
//! belong to the orchestrator and the runtime database. Duplicating them into a
//! protocol adapter is how a second, diverging source of truth gets created.
//!
//! ## Persistence
//!
//! A registry built with [`SessionRegistry::persistent`] writes each session to
//! `<dir>/<id>.json` after every turn, which is what lets `session/load`
//! restore a conversation after CUMA restarts. Loading restores the
//! *conversation*, not a plan in flight: a prompt interrupted by a restart is
//! not resumed, and the client is shown only the turns that finished.

use agent_client_protocol::schema::v1::SessionId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

/// How many past turns a session remembers.
///
/// Bounded because an editor may keep one session open for hours; the full
/// record lives in the runtime database.
const MAX_HISTORY: usize = 50;

/// The longest text kept per side of a turn.
const MAX_TURN_CHARS: usize = 16_000;

/// One prompt and what came of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Turn {
    /// What the user asked.
    pub prompt: String,
    /// What CUMA answered.
    pub response: String,
}

/// One ACP session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    /// The working directory the client chose.
    pub workspace: PathBuf,
    /// Past turns, oldest first.
    pub turns: Vec<Turn>,
    /// When the session was created.
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// The prompt currently running, if any.
    #[serde(skip)]
    pub running: Option<tokio::task::AbortHandle>,
}

impl SessionState {
    /// A new session rooted at `workspace`.
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            turns: Vec::new(),
            created_at: chrono::Utc::now(),
            running: None,
        }
    }

    /// Record a turn, dropping the oldest when full.
    pub fn record(&mut self, prompt: &str, response: &str) {
        self.turns.push(Turn {
            prompt: clip(prompt),
            response: clip(response),
        });
        if self.turns.len() > MAX_HISTORY {
            let excess = self.turns.len() - MAX_HISTORY;
            self.turns.drain(0..excess);
        }
    }

    /// Summaries of past turns, oldest first.
    pub fn history(&self) -> Vec<String> {
        self.turns.iter().map(|t| t.response.clone()).collect()
    }
}

fn clip(text: &str) -> String {
    if text.chars().count() <= MAX_TURN_CHARS {
        text.to_owned()
    } else {
        let kept: String = text.chars().take(MAX_TURN_CHARS).collect();
        format!("{kept}\n[… truncated]")
    }
}

/// The sessions an ACP client has open.
#[derive(Debug, Clone, Default)]
pub struct SessionRegistry {
    sessions: Arc<RwLock<BTreeMap<String, SessionState>>>,
    /// Where sessions are persisted, when they are.
    directory: Option<PathBuf>,
}

impl SessionRegistry {
    /// An in-memory registry. Sessions do not survive a restart.
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry that persists sessions under `directory`.
    pub fn persistent(directory: impl Into<PathBuf>) -> Self {
        Self {
            sessions: Arc::default(),
            directory: Some(directory.into()),
        }
    }

    /// Whether sessions outlive the process, which is what makes
    /// `session/load` worth advertising.
    pub fn is_persistent(&self) -> bool {
        self.directory.is_some()
    }

    /// Create a session and return its id.
    pub async fn create(&self, workspace: PathBuf) -> SessionId {
        let id = SessionId::new(format!("cuma-{}", uuid_like()));
        let state = SessionState::new(workspace);
        self.persist(&id.to_string(), &state).await;
        self.sessions.write().await.insert(id.to_string(), state);
        id
    }

    /// Restore a session, from memory or from disk.
    ///
    /// `None` when there is no such session — including when the id is not
    /// one CUMA could have issued, which is checked before it is turned into
    /// a path.
    pub async fn load(&self, id: &SessionId, workspace: PathBuf) -> Option<SessionState> {
        let key = id.to_string();
        if let Some(state) = self.sessions.read().await.get(&key) {
            return Some(state.clone());
        }

        let path = self.path_for(&key)?;
        let raw = tokio::fs::read_to_string(&path).await.ok()?;
        let mut state: SessionState = match serde_json::from_str(&raw) {
            Ok(state) => state,
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "ignoring an unreadable session file");
                return None;
            }
        };

        // The client decides where the session works now.
        state.workspace = workspace;
        self.sessions.write().await.insert(key, state.clone());
        Some(state)
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
    pub async fn record(&self, id: &SessionId, prompt: &str, response: &str) {
        let key = id.to_string();
        let snapshot = {
            let mut sessions = self.sessions.write().await;
            let Some(state) = sessions.get_mut(&key) else {
                return;
            };
            state.record(prompt, response);
            state.clone()
        };
        self.persist(&key, &snapshot).await;
    }

    /// Mark a prompt as running, returning `false` if one already is.
    pub async fn start(&self, id: &SessionId, handle: tokio::task::AbortHandle) -> bool {
        let mut sessions = self.sessions.write().await;
        let state = sessions
            .entry(id.to_string())
            .or_insert_with(|| SessionState::new(std::env::current_dir().unwrap_or_default()));
        if state.running.as_ref().is_some_and(|h| !h.is_finished()) {
            return false;
        }
        state.running = Some(handle);
        true
    }

    /// Clear the running prompt.
    pub async fn finish(&self, id: &SessionId) {
        if let Some(state) = self.sessions.write().await.get_mut(&id.to_string()) {
            state.running = None;
        }
    }

    /// Cancel the running prompt, if any. Returns whether one was running.
    pub async fn cancel(&self, id: &SessionId) -> bool {
        let sessions = self.sessions.read().await;
        match sessions
            .get(&id.to_string())
            .and_then(|s| s.running.as_ref())
        {
            Some(handle) if !handle.is_finished() => {
                handle.abort();
                true
            }
            _ => false,
        }
    }

    /// Fetch a session.
    pub async fn get(&self, id: &SessionId) -> Option<SessionState> {
        self.sessions.read().await.get(&id.to_string()).cloned()
    }

    /// Forget a session.
    pub async fn remove(&self, id: &SessionId) -> bool {
        let key = id.to_string();
        if let Some(path) = self.path_for(&key) {
            let _ = tokio::fs::remove_file(path).await;
        }
        self.sessions.write().await.remove(&key).is_some()
    }

    /// How many sessions are open.
    pub async fn len(&self) -> usize {
        self.sessions.read().await.len()
    }

    /// Whether no sessions are open.
    pub async fn is_empty(&self) -> bool {
        self.sessions.read().await.is_empty()
    }

    /// The file a session lives in, if it is persisted and its id is safe.
    fn path_for(&self, key: &str) -> Option<PathBuf> {
        let directory = self.directory.as_ref()?;
        is_plausible_id(key).then(|| directory.join(format!("{key}.json")))
    }

    /// Write a session to disk. Best effort: failing to persist degrades
    /// `session/load`, it does not fail the turn.
    async fn persist(&self, key: &str, state: &SessionState) {
        let Some(path) = self.path_for(key) else {
            return;
        };
        if let Err(err) = write_atomically(&path, state).await {
            tracing::warn!(path = %path.display(), error = %err, "could not persist an ACP session");
        }
    }
}

async fn write_atomically(path: &Path, state: &SessionState) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let json = serde_json::to_vec_pretty(state).map_err(std::io::Error::other)?;
    let temporary = path.with_extension("json.tmp");
    tokio::fs::write(&temporary, json).await?;
    tokio::fs::rename(&temporary, path).await
}

/// Whether an id is one CUMA could have issued.
///
/// Session ids arrive from the client, and become file names; anything with a
/// separator, a dot or an unexpected length is refused before it gets there.
fn is_plausible_id(id: &str) -> bool {
    id.starts_with("cuma-")
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
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

        registry
            .record(&id, "add auth", "4/4 tasks completed")
            .await;
        registry
            .record(&id, "fix tests", "2/3 tasks completed, 1 failed")
            .await;

        let state = registry.get(&id).await.unwrap();
        assert_eq!(state.turns.len(), 2);
        assert_eq!(state.turns[1].prompt, "fix tests");
        assert!(state.history()[1].contains("failed"));
    }

    #[tokio::test]
    async fn recording_against_an_unknown_session_is_a_no_op() {
        let registry = SessionRegistry::new();
        registry
            .record(&SessionId::new("ghost"), "p", "something")
            .await;
        assert!(registry.is_empty().await);
    }

    #[test]
    fn history_is_bounded_so_a_long_lived_session_cannot_grow_without_limit() {
        let mut state = SessionState::new(PathBuf::from("."));

        for i in 0..(MAX_HISTORY + 20) {
            state.record("p", &format!("turn {i}"));
        }

        assert_eq!(state.turns.len(), MAX_HISTORY);
        assert!(
            state
                .history()
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

    #[tokio::test]
    async fn a_persisted_session_survives_a_new_registry() {
        let directory = tempfile::tempdir().unwrap();
        let first = SessionRegistry::persistent(directory.path());
        let id = first.create(PathBuf::from("/old")).await;
        first.record(&id, "add auth", "done").await;

        let second = SessionRegistry::persistent(directory.path());
        let state = second.load(&id, PathBuf::from("/new")).await.unwrap();

        assert_eq!(
            state.turns,
            vec![Turn {
                prompt: "add auth".into(),
                response: "done".into()
            }]
        );
        assert_eq!(
            state.workspace,
            PathBuf::from("/new"),
            "the client picks the workspace now"
        );
    }

    #[tokio::test]
    async fn a_session_id_cannot_name_a_file_outside_the_session_directory() {
        let directory = tempfile::tempdir().unwrap();
        let outside = directory.path().join("secret.json");
        std::fs::write(
            &outside,
            r#"{"workspace":"/","turns":[],"created_at":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();

        let registry = SessionRegistry::persistent(directory.path().join("sessions"));
        for hostile in ["../secret", "cuma-../../secret", "cuma-a/b", "cuma-.."] {
            assert!(
                registry
                    .load(&SessionId::new(hostile), PathBuf::from("."))
                    .await
                    .is_none(),
                "{hostile}"
            );
        }
    }

    #[tokio::test]
    async fn an_unknown_session_cannot_be_loaded() {
        let directory = tempfile::tempdir().unwrap();
        let registry = SessionRegistry::persistent(directory.path());
        assert!(
            registry
                .load(&SessionId::new("cuma-nope"), PathBuf::from("."))
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn only_one_prompt_runs_per_session_and_it_can_be_cancelled() {
        let registry = SessionRegistry::new();
        let id = registry.create(PathBuf::from(".")).await;

        let running = tokio::spawn(std::future::pending::<()>());
        assert!(registry.start(&id, running.abort_handle()).await);

        let second = tokio::spawn(std::future::pending::<()>());
        assert!(
            !registry.start(&id, second.abort_handle()).await,
            "one prompt at a time"
        );
        second.abort();

        assert!(registry.cancel(&id).await);
        assert!(running.await.unwrap_err().is_cancelled());
    }
}
