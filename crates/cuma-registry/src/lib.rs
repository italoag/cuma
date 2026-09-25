//! Agent, model and capability registries.
//!
//! The registry is the router's world view: which agents exist, what they can
//! do, which models they expose and how healthy they are. It is populated by
//! [`cuma_core::ports::AgentDiscovery`] implementations — config files, an ACP
//! registry, A2A Agent Cards — and is deliberately ignorant of how any of them
//! actually work.

mod agents;
mod capabilities;
mod models;

pub use agents::{AgentRegistry, RegistrySnapshot, descriptors_from_config};
pub use capabilities::CapabilityIndex;
pub use models::ModelRegistry;
