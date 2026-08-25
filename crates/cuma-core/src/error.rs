//! The error taxonomy and, more importantly, its *classification*.
//!
//! Resilience policy keys off [`ErrorClass`], not off the concrete error. A
//! rate limit and a crashed agent both fail a task, but one wants a backoff
//! against the same agent and the other wants an immediate reroute to a
//! different one. Collapsing them into "an error occurred" is what produces
//! harnesses that retry forever against a dead process.

use crate::ids::{AgentId, ModelId, TaskId};
use serde::{Deserialize, Serialize};

/// Convenience alias for fallible harness operations.
pub type Result<T> = std::result::Result<T, MetaAgentError>;

/// How the resilience layer should treat a failure.
///
/// The ordering here is meaningful only as a stable serialization; the policy
/// mapping lives in `cuma-resilience` so that classification (a domain concern)
/// stays separate from reaction (a policy concern).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    /// Provider throttling. Back off, then retry the *same* target.
    RateLimit,
    /// Budget or plan exhausted. Retrying the same target is pointless.
    QuotaExceeded,
    /// Credentials rejected. No amount of retrying helps; surface to the user.
    AuthenticationFailure,
    /// The agent process died. Reroute.
    AgentCrash,
    /// The operation exceeded its deadline. Bounded retry, then reroute.
    Timeout,
    /// The peer spoke the protocol incorrectly. Usually a version mismatch.
    ProtocolError,
    /// The prompt exceeded the model's context window. Re-plan with less context.
    ContextOverflow,
    /// A tool invoked by the agent failed.
    ToolFailure,
    /// The model is not currently selectable. Reroute to another model.
    ModelUnavailable,
    /// The transport could not be established. Retry, then reroute.
    ConnectionFailure,
    /// The agent replied, but the reply was unusable.
    InvalidResponse,
    /// The agent completed and reported that the task itself failed.
    TaskFailure,
    /// A security policy refused the operation. Never retried automatically.
    SecurityViolation,
    /// Local misconfiguration. Not retryable.
    Configuration,
    /// Deliberately cancelled. Not a failure.
    Cancelled,
    /// Anything unrecognized. Treated conservatively (bounded retry).
    Unknown,
}

impl ErrorClass {
    /// Whether retrying the *same* agent and model could plausibly help.
    pub fn is_retryable_on_same_target(&self) -> bool {
        matches!(
            self,
            Self::RateLimit | Self::Timeout | Self::ConnectionFailure | Self::Unknown
        )
    }

    /// Whether routing to a *different* agent or model could plausibly help.
    pub fn is_reroutable(&self) -> bool {
        matches!(
            self,
            Self::RateLimit
                | Self::QuotaExceeded
                | Self::AgentCrash
                | Self::Timeout
                | Self::ProtocolError
                | Self::ModelUnavailable
                | Self::ConnectionFailure
                | Self::InvalidResponse
                | Self::ToolFailure
                | Self::Unknown
        )
    }

    /// Whether this failure should count against the target's circuit breaker.
    ///
    /// Cancellation and local configuration errors say nothing about the
    /// health of the remote agent, so they must not trip its breaker.
    pub fn counts_against_health(&self) -> bool {
        !matches!(
            self,
            Self::Cancelled | Self::Configuration | Self::SecurityViolation | Self::ContextOverflow
        )
    }

    /// Whether the failure calls for re-planning rather than re-execution.
    pub fn requires_replan(&self) -> bool {
        matches!(self, Self::ContextOverflow | Self::TaskFailure)
    }
}

/// The harness error type.
///
/// Every variant carries enough context to be actionable in a log line without
/// a stack trace, and `#[source]` chains are preserved throughout so the root
/// cause survives the trip up through the orchestrator.
#[derive(Debug, thiserror::Error)]
pub enum MetaAgentError {
    /// A protocol adapter failed at the wire level.
    #[error("protocol error on {protocol} adapter: {message}")]
    Protocol {
        /// Which adapter (`acp`, `a2a`, `mcp`).
        protocol: &'static str,
        /// What went wrong.
        message: String,
        /// Underlying cause, if any.
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    /// An agent failed to do its job.
    #[error("agent {agent} failed: {message}")]
    Agent {
        /// The agent at fault.
        agent: AgentId,
        /// What went wrong.
        message: String,
        /// How the resilience layer should treat this.
        class: ErrorClass,
    },

    /// A model was unusable.
    #[error("model {model} unavailable: {message}")]
    Model {
        /// The model at fault.
        model: ModelId,
        /// What went wrong.
        message: String,
    },

    /// No agent could be selected for a task.
    #[error("no agent could be routed for task {task}: {reason}")]
    Routing {
        /// The task that could not be routed.
        task: TaskId,
        /// Why every candidate was rejected.
        reason: String,
    },

    /// Credentials were missing or rejected.
    #[error("authentication failed for {target}: {message}")]
    Authentication {
        /// What we were authenticating to.
        target: String,
        /// What went wrong.
        message: String,
    },

    /// A deadline elapsed.
    #[error("{operation} timed out after {elapsed_ms}ms")]
    Timeout {
        /// What was being attempted.
        operation: String,
        /// How long we waited.
        elapsed_ms: u64,
    },

    /// The provider throttled us.
    #[error("rate limited by {agent}{}", retry_after_ms.map(|ms| format!(", retry after {ms}ms")).unwrap_or_default())]
    RateLimit {
        /// The throttling agent.
        agent: AgentId,
        /// Server-suggested wait, when provided.
        retry_after_ms: Option<u64>,
    },

    /// A skill operation failed.
    #[error("skill error: {0}")]
    Skill(String),

    /// The memory backend failed.
    #[error("memory error: {0}")]
    Memory(String),

    /// A tool invocation failed.
    #[error("tool {tool} failed: {message}")]
    Tool {
        /// The tool that failed.
        tool: String,
        /// What went wrong.
        message: String,
    },

    /// A security policy refused the operation.
    ///
    /// Never retried and never downgraded — see `docs/SECURITY.md`.
    #[error("security policy violation: {0}")]
    Security(String),

    /// Local configuration is wrong.
    #[error("configuration error: {0}")]
    Configuration(String),

    /// Persistence failed.
    #[error("persistence error: {0}")]
    Persistence(String),

    /// The operation was cancelled.
    #[error("operation cancelled: {0}")]
    Cancelled(String),

    /// An error that does not fit anywhere else.
    #[error("{0}")]
    Other(String),
}

impl MetaAgentError {
    /// Classify this error for the resilience layer.
    pub fn class(&self) -> ErrorClass {
        match self {
            Self::Protocol { .. } => ErrorClass::ProtocolError,
            Self::Agent { class, .. } => *class,
            Self::Model { .. } => ErrorClass::ModelUnavailable,
            Self::Routing { .. } => ErrorClass::Configuration,
            Self::Authentication { .. } => ErrorClass::AuthenticationFailure,
            Self::Timeout { .. } => ErrorClass::Timeout,
            Self::RateLimit { .. } => ErrorClass::RateLimit,
            Self::Skill(_) => ErrorClass::ToolFailure,
            Self::Memory(_) => ErrorClass::Unknown,
            Self::Tool { .. } => ErrorClass::ToolFailure,
            Self::Security(_) => ErrorClass::SecurityViolation,
            Self::Configuration(_) => ErrorClass::Configuration,
            Self::Persistence(_) => ErrorClass::Unknown,
            Self::Cancelled(_) => ErrorClass::Cancelled,
            Self::Other(_) => ErrorClass::Unknown,
        }
    }

    /// Build a protocol error from an arbitrary source error.
    pub fn protocol(
        protocol: &'static str,
        message: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::Protocol {
            protocol,
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }

    /// Build a protocol error with no underlying cause.
    pub fn protocol_msg(protocol: &'static str, message: impl Into<String>) -> Self {
        Self::Protocol {
            protocol,
            message: message.into(),
            source: None,
        }
    }

    /// Build an agent error with an explicit classification.
    pub fn agent(agent: AgentId, message: impl Into<String>, class: ErrorClass) -> Self {
        Self::Agent {
            agent,
            message: message.into(),
            class,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn auth_failures_are_neither_retried_nor_rerouted() {
        let class = ErrorClass::AuthenticationFailure;
        assert!(!class.is_retryable_on_same_target());
        assert!(!class.is_reroutable());
    }

    #[test]
    fn quota_exhaustion_reroutes_but_does_not_retry_in_place() {
        let class = ErrorClass::QuotaExceeded;
        assert!(!class.is_retryable_on_same_target());
        assert!(class.is_reroutable());
    }

    #[test]
    fn rate_limits_both_retry_and_reroute() {
        let class = ErrorClass::RateLimit;
        assert!(class.is_retryable_on_same_target());
        assert!(class.is_reroutable());
    }

    #[test]
    fn cancellation_never_trips_a_circuit_breaker() {
        assert!(!ErrorClass::Cancelled.counts_against_health());
        assert!(ErrorClass::AgentCrash.counts_against_health());
    }

    #[test]
    fn context_overflow_asks_for_a_replan_not_a_retry() {
        assert!(ErrorClass::ContextOverflow.requires_replan());
        assert!(!ErrorClass::ContextOverflow.is_retryable_on_same_target());
        assert!(!ErrorClass::ContextOverflow.counts_against_health());
    }

    #[test]
    fn security_violations_are_terminal() {
        let err = MetaAgentError::Security("skill signature invalid".into());
        assert_eq!(err.class(), ErrorClass::SecurityViolation);
        assert!(!err.class().is_retryable_on_same_target());
        assert!(!err.class().is_reroutable());
    }

    #[test]
    fn error_chains_are_preserved() {
        let io = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pipe closed");
        let err = MetaAgentError::protocol("acp", "transport died", io);
        assert!(std::error::Error::source(&err).is_some());
    }
}
