//! Long-term memory shared across agents and sessions.
//!
//! ## Why this is a process boundary, not a linked crate
//!
//! `ai-memory` is a semantic memory system with its own embedding models,
//! encryption and storage. Linking it in would pull a machine-learning stack
//! into every CUMA build and couple the harness's release cadence to it.
//!
//! More importantly, it is *designed* to be shared: the point of long-term
//! memory here is that a Codex session and a Claude session and a CUMA session
//! all see the same knowledge. That only works if memory lives outside any one
//! of them. So CUMA speaks to it as an external process — over its CLI, or over
//! MCP — behind [`MemoryStore`](cuma_core::ports::MemoryStore).
//!
//! See ADR-005.
//!
//! ## Memory is always optional
//!
//! Every operation degrades rather than fails. A missing binary, a crashed
//! backend or malformed output costs recall, never the session.

mod cli;
mod null;

pub use cli::AiMemoryCli;
pub use null::NullMemory;

use cuma_core::ports::MemoryStore;
use std::sync::Arc;

/// Build the memory store the configuration asks for.
///
/// Returns [`NullMemory`] when memory is disabled or the backend is
/// unrecognized — the harness must start either way.
pub fn from_config(config: &cuma_config::MemoryConfig) -> Arc<dyn MemoryStore> {
    if !config.enabled {
        return Arc::new(NullMemory);
    }

    match config.backend.as_str() {
        "ai-memory-cli" | "cli" => {
            let command = config
                .command
                .clone()
                .unwrap_or_else(|| "ai-memory".to_owned());
            Arc::new(AiMemoryCli::new(command))
        }
        "none" => Arc::new(NullMemory),
        other => {
            tracing::warn!(
                backend = other,
                "unknown memory backend; running without long-term memory"
            );
            Arc::new(NullMemory)
        }
    }
}
