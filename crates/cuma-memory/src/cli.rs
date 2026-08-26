//! The `ai-memory` CLI adapter.
//!
//! Talks to an external memory binary by spawning it per operation and parsing
//! its output. The CLI's exact flags vary by version, so parsing is
//! deliberately permissive: JSON is used when the backend emits it, and plain
//! lines are accepted when it does not.

use async_trait::async_trait;
use cuma_core::error::{MetaAgentError, Result};
use cuma_core::ports::{MemoryEntry, MemoryStore};
use serde::Deserialize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

/// How long a memory operation may take before it is abandoned.
///
/// Short on purpose: recall runs on the critical path of planning, and a slow
/// memory backend must degrade recall rather than stall the session.
const OPERATION_TIMEOUT: Duration = Duration::from_secs(10);

/// Cached availability, so a missing binary is probed once rather than per call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Availability {
    Unknown,
    Present,
    Missing,
}

impl Availability {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Present,
            2 => Self::Missing,
            _ => Self::Unknown,
        }
    }

    fn as_u8(self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::Present => 1,
            Self::Missing => 2,
        }
    }
}

/// One memory as the backend reports it.
///
/// Field names vary between versions, so the common spellings are aliased
/// rather than demanding one exact shape.
#[derive(Debug, Deserialize)]
struct RawMemory {
    #[serde(alias = "memory_id", alias = "uuid")]
    id: Option<String>,
    #[serde(alias = "text", alias = "body")]
    content: Option<String>,
    #[serde(alias = "type", alias = "category")]
    kind: Option<String>,
    #[serde(alias = "score", alias = "similarity")]
    relevance: Option<f64>,
}

/// Long-term memory over an external CLI.
#[derive(Debug, Clone)]
pub struct AiMemoryCli {
    command: String,
    availability: Arc<AtomicU8>,
}

impl AiMemoryCli {
    /// An adapter that runs `command`.
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            availability: Arc::new(AtomicU8::new(Availability::Unknown.as_u8())),
        }
    }

    /// The binary and its fixed arguments.
    fn parts(&self) -> Result<Vec<String>> {
        shell_words::split(&self.command).map_err(|err| {
            MetaAgentError::Configuration(format!(
                "cannot parse the memory command {:?}: {err}",
                self.command
            ))
        })
    }

    /// Run the backend with `args` and return its stdout.
    async fn run(&self, args: &[&str]) -> Result<String> {
        let parts = self.parts()?;
        let Some((binary, fixed)) = parts.split_first() else {
            return Err(MetaAgentError::Configuration(
                "the memory command is empty".to_owned(),
            ));
        };

        let mut command = tokio::process::Command::new(binary);
        command.args(fixed).args(args);
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());

        let output = tokio::time::timeout(OPERATION_TIMEOUT, command.output())
            .await
            .map_err(|_| MetaAgentError::Timeout {
                operation: format!("ai-memory {}", args.join(" ")),
                elapsed_ms: OPERATION_TIMEOUT.as_millis() as u64,
            })?
            .map_err(|err| {
                MetaAgentError::Memory(format!("cannot run the memory backend: {err}"))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(MetaAgentError::Memory(format!(
                "the memory backend exited with {}: {}",
                output.status,
                stderr.trim()
            )));
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Parse whatever the backend printed into memories.
    ///
    /// Accepts a JSON array, a JSON object with a `memories` or `results` key,
    /// newline-delimited JSON, or plain text lines. Being permissive here is
    /// the difference between "recall works across backend versions" and
    /// "recall silently returns nothing after an upgrade".
    fn parse_recall(output: &str) -> Vec<MemoryEntry> {
        let trimmed = output.trim();
        if trimmed.is_empty() {
            return Vec::new();
        }

        if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
            let array = value
                .as_array()
                .cloned()
                .or_else(|| value.get("memories").and_then(|v| v.as_array()).cloned())
                .or_else(|| value.get("results").and_then(|v| v.as_array()).cloned())
                .or_else(|| value.get("data").and_then(|v| v.as_array()).cloned());

            if let Some(array) = array {
                return array
                    .into_iter()
                    .filter_map(|item| serde_json::from_value::<RawMemory>(item).ok())
                    .filter_map(Self::into_entry)
                    .collect();
            }
        }

        // Newline-delimited JSON, then plain lines.
        let mut entries = Vec::new();
        for line in trimmed.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            if let Ok(raw) = serde_json::from_str::<RawMemory>(line) {
                if let Some(entry) = Self::into_entry(raw) {
                    entries.push(entry);
                }
                continue;
            }

            entries.push(MemoryEntry {
                id: format!("line:{}", entries.len()),
                content: line.to_owned(),
                kind: "unstructured".to_owned(),
                relevance: None,
                created_at: chrono::Utc::now(),
            });
        }

        entries
    }

    fn into_entry(raw: RawMemory) -> Option<MemoryEntry> {
        // A memory with no content is not a memory.
        let content = raw.content?;
        if content.trim().is_empty() {
            return None;
        }

        Some(MemoryEntry {
            id: raw.id.unwrap_or_else(|| "unknown".to_owned()),
            content,
            kind: raw.kind.unwrap_or_else(|| "memory".to_owned()),
            relevance: raw.relevance,
            created_at: chrono::Utc::now(),
        })
    }
}

#[async_trait]
impl MemoryStore for AiMemoryCli {
    async fn is_available(&self) -> bool {
        match Availability::from_u8(self.availability.load(Ordering::Relaxed)) {
            Availability::Present => return true,
            Availability::Missing => return false,
            Availability::Unknown => {}
        }

        let present = self
            .parts()
            .ok()
            .and_then(|parts| parts.first().cloned())
            .is_some_and(|binary| which::which(binary).is_ok());

        self.availability.store(
            if present {
                Availability::Present.as_u8()
            } else {
                Availability::Missing.as_u8()
            },
            Ordering::Relaxed,
        );

        if !present {
            tracing::info!(
                command = self.command,
                "the memory backend is not on PATH; running without long-term memory"
            );
        }

        present
    }

    async fn recall(&self, query: &str, limit: usize) -> Result<Vec<MemoryEntry>> {
        if !self.is_available().await {
            return Ok(Vec::new());
        }

        let limit = limit.to_string();
        let output = self
            .run(&["search", query, "--limit", &limit, "--json"])
            .await?;

        Ok(Self::parse_recall(&output))
    }

    async fn remember(&self, content: &str, kind: &str) -> Result<String> {
        if !self.is_available().await {
            return Ok("not-stored:backend-unavailable".to_owned());
        }

        let output = self.run(&["add", content, "--type", kind]).await?;

        let id = output.trim();
        Ok(if id.is_empty() {
            "stored:unknown-id".to_owned()
        } else {
            id.to_owned()
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[tokio::test]
    async fn a_missing_backend_reports_itself_unavailable_rather_than_erroring() {
        let memory = AiMemoryCli::new("definitely-not-a-real-binary-7f2a");
        assert!(!memory.is_available().await);
    }

    #[tokio::test]
    async fn recall_against_a_missing_backend_returns_nothing_rather_than_failing() {
        let memory = AiMemoryCli::new("definitely-not-a-real-binary-7f2a");
        // Planning calls this on its critical path; it must never be fatal.
        assert!(memory.recall("oauth", 5).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn remember_against_a_missing_backend_says_so_instead_of_claiming_success() {
        let memory = AiMemoryCli::new("definitely-not-a-real-binary-7f2a");
        let id = memory.remember("something", "note").await.unwrap();
        assert!(id.starts_with("not-stored:"));
    }

    #[tokio::test]
    async fn availability_is_probed_once_and_cached() {
        let memory = AiMemoryCli::new("definitely-not-a-real-binary-7f2a");
        assert!(!memory.is_available().await);
        assert_eq!(
            Availability::from_u8(memory.availability.load(Ordering::Relaxed)),
            Availability::Missing
        );
        assert!(!memory.is_available().await);
    }

    #[test]
    fn a_json_array_of_memories_parses() {
        let output = r#"[
            {"id":"m1","content":"the project uses tabs","kind":"convention","relevance":0.92},
            {"id":"m2","content":"auth lives in src/auth.rs","kind":"finding"}
        ]"#;

        let entries = AiMemoryCli::parse_recall(output);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "m1");
        assert_eq!(entries[0].relevance, Some(0.92));
        assert_eq!(entries[1].relevance, None);
    }

    #[test]
    fn a_wrapped_json_object_parses_under_any_of_the_common_keys() {
        for key in ["memories", "results", "data"] {
            let output = format!(r#"{{"{key}":[{{"id":"m1","content":"x"}}]}}"#);
            assert_eq!(AiMemoryCli::parse_recall(&output).len(), 1, "key {key}");
        }
    }

    #[test]
    fn alternative_field_spellings_are_accepted() {
        let output = r#"[{"memory_id":"m1","text":"content here","type":"decision","score":0.5}]"#;
        let entries = AiMemoryCli::parse_recall(output);

        assert_eq!(entries[0].id, "m1");
        assert_eq!(entries[0].content, "content here");
        assert_eq!(entries[0].kind, "decision");
        assert_eq!(entries[0].relevance, Some(0.5));
    }

    #[test]
    fn newline_delimited_json_parses() {
        let output = "{\"id\":\"a\",\"content\":\"one\"}\n{\"id\":\"b\",\"content\":\"two\"}";
        assert_eq!(AiMemoryCli::parse_recall(output).len(), 2);
    }

    #[test]
    fn plain_text_lines_are_kept_as_unstructured_memories() {
        // A backend that emits no JSON should still contribute something,
        // rather than silently recalling nothing after a version bump.
        let entries = AiMemoryCli::parse_recall("the project uses tabs\nauth is in src/auth.rs");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kind, "unstructured");
    }

    #[test]
    fn empty_output_yields_no_memories() {
        assert!(AiMemoryCli::parse_recall("").is_empty());
        assert!(AiMemoryCli::parse_recall("   \n  ").is_empty());
    }

    #[test]
    fn a_memory_with_no_content_is_discarded() {
        let output = r#"[{"id":"m1"},{"id":"m2","content":"   "},{"id":"m3","content":"real"}]"#;
        let entries = AiMemoryCli::parse_recall(output);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "m3");
    }

    #[test]
    fn an_unparseable_command_is_a_configuration_error() {
        let memory = AiMemoryCli::new("unterminated 'quote");
        assert_eq!(
            memory.parts().unwrap_err().class(),
            cuma_core::ErrorClass::Configuration
        );
    }

    #[test]
    fn config_with_memory_disabled_yields_the_null_store() {
        let config = cuma_config::MemoryConfig {
            enabled: false,
            ..cuma_config::MemoryConfig::default()
        };
        let store = crate::from_config(&config);
        assert!(!futures_block_on(store.is_available()));
    }

    fn futures_block_on<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(future)
    }
}
