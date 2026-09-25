//! Normalized descriptions of agents and models.
//!
//! An [`AgentDescriptor`] is what the router sees. Whether the agent behind it
//! is a locally spawned ACP process, a remote A2A endpoint or an in-process
//! mock is recorded in [`AgentProtocol`] for bookkeeping and nothing else —
//! no routing decision may branch on it.

use crate::capability::CapabilitySet;
use crate::ids::{AgentId, ModelId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Which wire protocol an adapter uses to reach an agent.
///
/// This exists so operators can see *how* an agent is reached, and so adapters
/// can be looked up. Routing logic must not read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentProtocol {
    /// Agent Client Protocol — the preferred transport for local coding agents.
    Acp,
    /// Agent2Agent — for independent, often remote, agents.
    A2A,
    /// In-process; used by the test kit and by built-in agents.
    Native,
}

/// A value that may legitimately be unknown.
///
/// Half the metadata the router would like to have simply is not reported by
/// every agent. Modelling that as `0.0` would make an agent that reports no
/// cost look free, and modelling it as `Option` alone loses the distinction
/// between "we asked and it said nothing" and "we estimated it ourselves".
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum Known<T> {
    /// The agent or provider reported this value directly.
    Reported(T),
    /// We derived this value from heuristics or history. Not ground truth.
    Estimated(T),
    /// No value is available and we will not pretend otherwise.
    #[default]
    Unknown,
}

impl<T: Copy> Known<T> {
    /// The value, if any, regardless of provenance.
    pub fn value(&self) -> Option<T> {
        match self {
            Self::Reported(v) | Self::Estimated(v) => Some(*v),
            Self::Unknown => None,
        }
    }

    /// The value, or `fallback` when unknown.
    pub fn or(&self, fallback: T) -> T {
        self.value().unwrap_or(fallback)
    }

    /// Whether this value came from the agent itself.
    pub fn is_reported(&self) -> bool {
        matches!(self, Self::Reported(_))
    }
}

/// Circuit-breaker state for one agent (or agent+model pair).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    /// Accepting work.
    #[default]
    Healthy,
    /// Reachable but degraded — usable, penalised by the router.
    Degraded,
    /// Temporarily rate limited by the provider.
    RateLimited,
    /// Circuit open: excluded from routing until the breaker half-opens.
    Unavailable,
    /// Never contacted, so nothing is known yet.
    Unknown,
}

impl HealthState {
    /// Whether the router may consider an agent in this state at all.
    pub fn is_routable(&self) -> bool {
        matches!(self, Self::Healthy | Self::Degraded | Self::Unknown)
    }
}

/// Observed health of an agent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentHealth {
    /// Current circuit state.
    pub state: HealthState,
    /// Consecutive failures since the last success.
    pub consecutive_failures: u32,
    /// Last observed round-trip latency, in milliseconds.
    pub last_latency_ms: Known<u64>,
    /// When the agent last completed a task successfully.
    pub last_success: Option<chrono::DateTime<chrono::Utc>>,
    /// Why the agent is unavailable, when it is.
    pub last_error: Option<String>,
}

/// Hard limits an agent imposes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentLimits {
    /// Maximum concurrent sessions the harness will open against this agent.
    pub max_concurrent_sessions: Option<u32>,
    /// Provider-imposed requests per minute.
    pub requests_per_minute: Known<u32>,
    /// Provider-imposed tokens per minute.
    pub tokens_per_minute: Known<u64>,
}

/// What a model costs, in USD per million tokens.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CostProfile {
    /// USD per million input tokens.
    pub input_per_mtok: Known<f64>,
    /// USD per million output tokens.
    pub output_per_mtok: Known<f64>,
    /// USD per million cached-read tokens.
    pub cache_read_per_mtok: Known<f64>,
}

impl CostProfile {
    /// Estimate the USD cost of a token split.
    ///
    /// Returns [`Known::Unknown`] when neither price is known, rather than
    /// silently reporting `$0.00` for a model whose pricing we never learned.
    pub fn estimate(&self, input_tokens: u64, output_tokens: u64) -> Known<f64> {
        let (Some(input_price), Some(output_price)) =
            (self.input_per_mtok.value(), self.output_per_mtok.value())
        else {
            return Known::Unknown;
        };

        #[allow(clippy::cast_precision_loss)]
        let cost = (input_tokens as f64 / 1_000_000.0) * input_price
            + (output_tokens as f64 / 1_000_000.0) * output_price;

        // Always `Estimated`, even when both prices were reported: the token
        // counts feeding this may not have been, and a cost derived from a
        // guessed token count is a guess however exact the price was.
        Known::Estimated(cost)
    }
}

/// Observed or advertised performance characteristics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PerformanceProfile {
    /// Typical end-to-end latency for a task, in milliseconds.
    pub typical_latency_ms: Known<u64>,
    /// Output tokens per second.
    pub throughput_tps: Known<f64>,
    /// Fraction of past attempts that succeeded, in `[0.0, 1.0]`.
    pub historical_success_rate: Known<f64>,
    /// A coarse 0-1 quality signal for coding work.
    pub coding_score: Known<f64>,
    /// A coarse 0-1 signal for depth of reasoning.
    pub reasoning_score: Known<f64>,
}

/// How the harness authenticates to an agent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum AgentAuth {
    /// The agent manages its own credentials (an already-logged-in CLI).
    ///
    /// This is the preferred mode: it reuses the user's existing subscription
    /// and keeps secrets out of the harness entirely.
    #[default]
    AgentManaged,
    /// A secret resolved at run time from the OS keychain or environment.
    ///
    /// Only the *reference* is stored — never the secret itself.
    SecretRef {
        /// Opaque handle understood by the secret store.
        handle: String,
    },
    /// No authentication required.
    None,
}

/// One model reachable through one agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelDescriptor {
    /// Model identifier, unique within its agent.
    pub id: ModelId,
    /// The agent that exposes this model.
    pub agent_id: AgentId,
    /// Human-readable name.
    pub name: String,
    /// Upstream provider, when the agent discloses one.
    pub provider: Option<String>,
    /// Maximum context window in tokens.
    pub context_window: Known<u64>,
    /// Pricing.
    pub cost: CostProfile,
    /// Performance signals.
    pub performance: PerformanceProfile,
    /// Capabilities this specific model adds beyond its agent's baseline.
    pub capabilities: CapabilitySet,
    /// Whether the model is currently selectable.
    pub available: bool,
}

impl ModelDescriptor {
    /// A minimal descriptor with everything unknown.
    ///
    /// Discovery fills in what it learns; anything it does not learn stays
    /// [`Known::Unknown`] instead of defaulting to a flattering value.
    pub fn minimal(agent_id: AgentId, id: impl Into<ModelId>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            agent_id,
            name: name.into(),
            provider: None,
            context_window: Known::Unknown,
            cost: CostProfile::default(),
            performance: PerformanceProfile::default(),
            capabilities: CapabilitySet::new(),
            available: true,
        }
    }
}

/// The router's complete view of one agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDescriptor {
    /// Stable identifier.
    pub id: AgentId,
    /// Human-readable name.
    pub name: String,
    /// Transport used to reach it. Bookkeeping only.
    pub protocol: AgentProtocol,
    /// Capabilities advertised by the agent as a whole.
    pub capabilities: CapabilitySet,
    /// Models this agent exposes. May be empty when the agent hides them.
    pub models: Vec<ModelDescriptor>,
    /// Live health.
    pub health: AgentHealth,
    /// Hard limits.
    pub limits: AgentLimits,
    /// Baseline cost, used when a model has no pricing of its own.
    pub cost_profile: CostProfile,
    /// Baseline performance, used when a model has no profile of its own.
    pub performance_profile: PerformanceProfile,
    /// Authentication mode.
    pub auth: AgentAuth,
    /// Whether the operator has enabled this agent.
    pub enabled: bool,
    /// Adapter-specific settings, opaque to the core.
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

impl AgentDescriptor {
    /// An enabled, healthy agent with no models and no known metrics.
    pub fn new(id: impl Into<AgentId>, name: impl Into<String>, protocol: AgentProtocol) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            protocol,
            capabilities: CapabilitySet::new(),
            models: Vec::new(),
            health: AgentHealth::default(),
            limits: AgentLimits::default(),
            cost_profile: CostProfile::default(),
            performance_profile: PerformanceProfile::default(),
            auth: AgentAuth::default(),
            enabled: true,
            metadata: BTreeMap::new(),
        }
    }

    /// Set the advertised capabilities.
    #[must_use]
    pub fn with_capabilities(mut self, capabilities: CapabilitySet) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Add a model.
    #[must_use]
    pub fn with_model(mut self, model: ModelDescriptor) -> Self {
        self.models.push(model);
        self
    }

    /// Whether the router may consider this agent right now.
    pub fn is_routable(&self) -> bool {
        self.enabled && self.health.state.is_routable()
    }

    /// The union of the agent's capabilities and those of `model`.
    ///
    /// Models may add capabilities (vision on one model but not another), so
    /// matching must be done per agent+model pair, not per agent.
    pub fn effective_capabilities(&self, model: Option<&ModelDescriptor>) -> CapabilitySet {
        let mut set = self.capabilities.clone();
        if let Some(model) = model {
            for capability in model.capabilities.iter() {
                set.insert(capability.clone());
            }
        }
        set
    }

    /// Find a model by id.
    pub fn model(&self, id: &ModelId) -> Option<&ModelDescriptor> {
        self.models.iter().find(|m| &m.id == id)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::capability::Capability;

    #[test]
    fn unknown_metrics_do_not_masquerade_as_zero() {
        let cost = CostProfile::default();
        assert_eq!(cost.estimate(1000, 1000), Known::Unknown);
        assert_eq!(cost.input_per_mtok.value(), None);
    }

    #[test]
    fn cost_estimate_uses_both_prices() {
        let cost = CostProfile {
            input_per_mtok: Known::Reported(3.0),
            output_per_mtok: Known::Reported(15.0),
            cache_read_per_mtok: Known::Unknown,
        };
        // 1M in at $3 + 1M out at $15 = $18.
        let estimated = cost.estimate(1_000_000, 1_000_000).value().unwrap();
        assert!((estimated - 18.0).abs() < 1e-9);
    }

    #[test]
    fn partial_pricing_yields_unknown_rather_than_a_half_price() {
        let cost = CostProfile {
            input_per_mtok: Known::Reported(3.0),
            output_per_mtok: Known::Unknown,
            cache_read_per_mtok: Known::Unknown,
        };
        assert_eq!(cost.estimate(1_000_000, 1_000_000), Known::Unknown);
    }

    #[test]
    fn open_circuit_removes_an_agent_from_routing() {
        let mut agent = AgentDescriptor::new("a", "A", AgentProtocol::Native);
        assert!(agent.is_routable());

        agent.health.state = HealthState::Unavailable;
        assert!(!agent.is_routable());

        agent.health.state = HealthState::Degraded;
        assert!(agent.is_routable(), "degraded is penalised, not excluded");
    }

    #[test]
    fn a_disabled_agent_is_never_routable_however_healthy() {
        let mut agent = AgentDescriptor::new("a", "A", AgentProtocol::Native);
        agent.enabled = false;
        agent.health.state = HealthState::Healthy;
        assert!(!agent.is_routable());
    }

    #[test]
    fn model_capabilities_union_with_the_agent_baseline() {
        let agent = AgentDescriptor::new("a", "A", AgentProtocol::Acp)
            .with_capabilities(CapabilitySet::new().with(Capability::CodeEditing));
        let mut model = ModelDescriptor::minimal(agent.id.clone(), "m", "M");
        model.capabilities.insert(Capability::Vision);

        let effective = agent.effective_capabilities(Some(&model));
        assert!(effective.contains(&Capability::CodeEditing));
        assert!(effective.contains(&Capability::Vision));
        assert!(!agent.capabilities.contains(&Capability::Vision));
    }
}
