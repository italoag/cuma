//! The ACP adapter.
//!
//! ACP is the preferred transport for local coding agents: it is what Zed,
//! JetBrains and other editors already speak, and it means CUMA reuses an
//! agent's *own* authenticated session rather than needing an API key of its
//! own (see ADR-002 and `docs/PROTOCOLS.md`).
//!
//! This crate wraps the official `agent-client-protocol` Rust SDK. It does not
//! reimplement JSON-RPC, framing or the schema — writing a second, subtly
//! different ACP implementation is exactly the kind of work this project
//! exists to avoid.
//!
//! ## What crosses the boundary
//!
//! Nothing ACP-shaped escapes this crate. `SessionNotification`,
//! `PromptRequest` and `StopReason` are translated into
//! [`cuma_core::ports::ExecutionUpdate`] and [`cuma_core::ExecutionOutcome`]
//! at the edge, so the orchestrator stays protocol-agnostic.

mod adapter;
mod capabilities;
mod discovery;

pub use adapter::AcpAdapter;
pub use capabilities::{capabilities_from_initialize, well_known_agent_command};
pub use discovery::AcpConfigDiscovery;
