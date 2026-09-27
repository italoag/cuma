//! # cuma-core
//!
//! The protocol-agnostic heart of the CUMA meta-agent harness.
//!
//! Everything in this crate is expressed in terms of the *domain* — tasks,
//! agents, models, capabilities — and never in terms of a wire protocol. ACP,
//! A2A and MCP live in adapter crates at the edge of the system and translate
//! into the types defined here.
//!
//! The rule that keeps the architecture honest: **nothing in `cuma-core` may
//! depend on a protocol SDK or a provider SDK.** If a type here needs to know
//! whether an agent speaks ACP or A2A for anything other than bookkeeping,
//! the abstraction has leaked.
//!
//! ## Module map
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`agent`] | Normalized agent + model descriptors |
//! | [`capability`] | What an agent can do, and what a task needs |
//! | [`task`] | Tasks, the task DAG, execution outcomes |
//! | [`event`] | The internal event bus vocabulary |
//! | [`ports`] | Traits the outer layers implement (hexagonal ports) |
//! | [`error`] | The `MetaAgentError` taxonomy and failure classification |
//! | [`handoff`] | Structured agent-to-agent handoff |
//! | [`ids`] | Correlatable identifiers |

pub mod agent;
pub mod capability;
pub mod error;
pub mod event;
pub mod handoff;
pub mod ids;
pub mod ports;
pub mod task;

pub use agent::{
    AgentAuth, AgentDescriptor, AgentHealth, AgentLimits, AgentProtocol, CostProfile, HealthState,
    Known, ModelDescriptor, PerformanceProfile,
};
pub use capability::{Capability, CapabilityMatch, CapabilitySet};
pub use error::{ErrorClass, MetaAgentError, Result};
pub use event::{Event, EventBus, EventKind, EventSubscriber};
pub use handoff::AgentHandoff;
pub use ids::{AgentId, AttemptId, ModelId, SessionId, SkillId, TaskId};
pub use task::{
    ExecutionOutcome, Risk, Task, TaskGraph, TaskSpec, TaskStatus, TaskType, TokenUsage,
};
