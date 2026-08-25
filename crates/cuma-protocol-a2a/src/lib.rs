//! The A2A adapter: Agent Card discovery and task delegation.
//!
//! ## Why this is a native client rather than `a2a-rs`
//!
//! The official `a2a-rs` crate is the right long-term dependency, but its
//! current release requires a newer Rust than this workspace's MSRV, and
//! pinning to an older release would mean coding against a superseded API.
//! Rather than raise the floor for the whole workspace on account of one
//! optional protocol, this crate implements the narrow slice of A2A that
//! CUMA actually needs — Agent Card discovery, `message/send`, `tasks/get`
//! and `tasks/cancel` over JSON-RPC — behind the same
//! [`AgentAdapter`](cuma_core::ports::AgentAdapter) port as everything else.
//!
//! Because the port is the boundary, swapping in `a2a-rs` later is a change to
//! this crate alone. See `ADR-003` and `DEPENDENCY_ANALYSIS.md`.
//!
//! ## Trust
//!
//! Everything an A2A peer returns is untrusted input, including its Agent
//! Card. Skills, descriptions and artifact text are treated as data: they can
//! influence what capabilities an agent is *believed* to have, never what the
//! harness is permitted to do.

mod card;
mod client;

pub use card::{AgentCard, AgentSkill, capabilities_from_card};
pub use client::{A2aAdapter, A2aDiscovery};
