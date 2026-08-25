//! The configuration schema.
//!
//! Every struct denies unknown fields. A silently-ignored typo in a config
//! file is far more expensive to debug than a startup error.

use cuma_core::error::{MetaAgentError, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The complete harness configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Routing policy and weights.
    pub router: RouterConfig,
    /// Per-agent settings, keyed by agent id.
    pub agents: BTreeMap<String, AgentConfig>,
    /// Long-term memory backend.
    pub memory: MemoryConfig,
    /// Output-reduction proxy.
    pub rtk: RtkConfig,
    /// Skill discovery and installation policy.
    pub skills: SkillsConfig,
    /// Sandboxing and workspace protection.
    pub security: SecurityConfig,
    /// Concurrency and retry bounds.
    pub limits: LimitsConfig,
    /// Logging and tracing.
    pub telemetry: TelemetryConfig,
}

impl Config {
    /// Reject configurations that are internally inconsistent.
    ///
    /// Called after every layer is merged, so an operator learns at startup —
    /// not at the first routing decision — that their weights are nonsense.
    pub fn validate(&self) -> Result<()> {
        self.router.weights.validate()?;

        if self.limits.max_parallel_tasks == 0 {
            return Err(MetaAgentError::Configuration(
                "limits.max_parallel_tasks must be at least 1".to_owned(),
            ));
        }

        if self.limits.max_retries > 10 {
            return Err(MetaAgentError::Configuration(format!(
                "limits.max_retries = {} is implausibly high; retries must stay bounded",
                self.limits.max_retries
            )));
        }

        if let Some(budget) = self.limits.max_cost_usd
            && budget <= 0.0
        {
            return Err(MetaAgentError::Configuration(
                "limits.max_cost_usd must be positive when set".to_owned(),
            ));
        }

        Ok(())
    }
}

/// Which dimension the router optimizes for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RoutingStrategy {
    /// Weigh quality, cost, latency, reliability and context fit together.
    #[default]
    Balanced,
    /// Prefer the most capable agent, largely ignoring price.
    QualityFirst,
    /// Prefer the cheapest agent that can do the job.
    CostFirst,
    /// Prefer the fastest agent.
    LatencyFirst,
    /// Prefer locally running agents.
    LocalFirst,
    /// Prefer agents that keep data on the machine.
    PrivacyFirst,
    /// Use only what the operator pinned; never choose.
    Manual,
}

impl RoutingStrategy {
    /// The weight preset this strategy implies.
    ///
    /// Presets are a starting point: an explicit `[router.weights]` block
    /// overrides them, which is why merge order matters (see `merge.rs`).
    pub fn default_weights(&self) -> RouterWeights {
        match self {
            Self::Balanced => RouterWeights {
                quality: 0.30,
                cost: 0.20,
                latency: 0.10,
                reliability: 0.30,
                context: 0.10,
            },
            Self::QualityFirst => RouterWeights {
                quality: 0.55,
                cost: 0.05,
                latency: 0.05,
                reliability: 0.25,
                context: 0.10,
            },
            Self::CostFirst => RouterWeights {
                quality: 0.15,
                cost: 0.55,
                latency: 0.05,
                reliability: 0.15,
                context: 0.10,
            },
            Self::LatencyFirst => RouterWeights {
                quality: 0.15,
                cost: 0.10,
                latency: 0.50,
                reliability: 0.20,
                context: 0.05,
            },
            // Locality and privacy are expressed as hard filters plus a
            // reliability bias, not as a weight dimension of their own.
            Self::LocalFirst | Self::PrivacyFirst => RouterWeights {
                quality: 0.25,
                cost: 0.15,
                latency: 0.15,
                reliability: 0.35,
                context: 0.10,
            },
            Self::Manual => RouterWeights::default(),
        }
    }
}

/// Relative importance of each scoring dimension.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RouterWeights {
    /// Capability match and model quality.
    pub quality: f64,
    /// Estimated spend. Higher means cost matters more.
    pub cost: f64,
    /// Expected wall-clock time.
    pub latency: f64,
    /// Historical success rate and current health.
    pub reliability: f64,
    /// Whether the task's context fits the model's window.
    pub context: f64,
}

impl Default for RouterWeights {
    fn default() -> Self {
        RoutingStrategy::Balanced.default_weights()
    }
}

impl RouterWeights {
    /// Reject negative or all-zero weights.
    ///
    /// All-zero weights would make every candidate score identically and turn
    /// routing into an arbitrary pick, which is worse than an error.
    pub fn validate(&self) -> Result<()> {
        let dims = [
            ("quality", self.quality),
            ("cost", self.cost),
            ("latency", self.latency),
            ("reliability", self.reliability),
            ("context", self.context),
        ];

        for (name, value) in dims {
            if value < 0.0 || !value.is_finite() {
                return Err(MetaAgentError::Configuration(format!(
                    "router.weights.{name} must be a finite non-negative number, got {value}"
                )));
            }
        }

        if self.sum() <= 0.0 {
            return Err(MetaAgentError::Configuration(
                "router weights cannot all be zero; routing would be arbitrary".to_owned(),
            ));
        }

        Ok(())
    }

    /// Sum of all weights.
    pub fn sum(&self) -> f64 {
        self.quality + self.cost + self.latency + self.reliability + self.context
    }

    /// The same weights scaled to sum to 1.0.
    ///
    /// Operators write weights that sum to whatever they like; the router
    /// needs them normalized so scores stay comparable across configurations.
    pub fn normalized(&self) -> Self {
        let sum = self.sum();
        if sum <= 0.0 {
            return Self::default();
        }
        Self {
            quality: self.quality / sum,
            cost: self.cost / sum,
            latency: self.latency / sum,
            reliability: self.reliability / sum,
            context: self.context / sum,
        }
    }
}

/// Router configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RouterConfig {
    /// Optimization target.
    pub strategy: RoutingStrategy,
    /// Explicit weights. When absent, `strategy`'s preset is used.
    pub weights: RouterWeights,
    /// Force this agent for every task.
    pub pin_agent: Option<String>,
    /// Force this model for every task.
    pub pin_model: Option<String>,
    /// Agents the router must never select.
    pub exclude_agents: Vec<String>,
    /// Models the router must never select.
    pub exclude_models: Vec<String>,
    /// Weight given to observed history over static profiles, in `[0.0, 1.0]`.
    pub adaptive_weight: f64,
    /// Minimum attempts in a history bucket before it influences routing.
    ///
    /// Without this, one lucky success would make an agent look perfect.
    pub adaptive_min_samples: u32,
}

/// One agent's settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    /// Whether the operator has enabled it.
    pub enabled: bool,
    /// Protocol: `acp`, `a2a` or `native`.
    pub protocol: String,
    /// Command to spawn, for ACP agents.
    pub command: Option<String>,
    /// Endpoint URL, for A2A agents.
    pub endpoint: Option<String>,
    /// Capabilities to assume when the agent does not advertise any.
    pub capabilities: Vec<String>,
    /// Models to assume when the agent does not enumerate them.
    pub models: Vec<String>,
    /// Handle for the secret store. Never the secret itself.
    pub auth_secret_ref: Option<String>,
    /// Adapter-specific extras.
    pub metadata: BTreeMap<String, String>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            protocol: "acp".to_owned(),
            command: None,
            endpoint: None,
            capabilities: Vec::new(),
            models: Vec::new(),
            auth_secret_ref: None,
            metadata: BTreeMap::new(),
        }
    }
}

/// Long-term memory backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MemoryConfig {
    /// Whether to use long-term memory at all.
    pub enabled: bool,
    /// How to reach the backend: `ai-memory-cli`, `mcp` or `none`.
    pub backend: String,
    /// Command that runs the memory backend, for the CLI transport.
    pub command: Option<String>,
    /// How many memories to inject into a plan.
    pub recall_limit: usize,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            // Off by default: memory is an external dependency, and a harness
            // that fails to start because an optional binary is missing is a
            // worse default than one that starts without recall.
            enabled: false,
            backend: "ai-memory-cli".to_owned(),
            command: None,
            recall_limit: 8,
        }
    }
}

/// Whether to route shell output through RTK.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RtkMode {
    /// Use RTK if it is on `PATH`, otherwise carry on without it.
    #[default]
    Auto,
    /// Require RTK; fail loudly if it is missing.
    Always,
    /// Never use RTK.
    Never,
}

/// RTK integration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RtkConfig {
    /// Detection mode.
    pub enabled: RtkMode,
    /// Override the binary path.
    pub binary: Option<String>,
}

/// When a skill may be installed without asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkillAutoInstall {
    /// Never install without explicit confirmation.
    Never,
    /// Install only skills that validate as `Trusted`.
    #[default]
    TrustedOnly,
    /// Install `Trusted` or `Verified` skills.
    Verified,
}

/// Skill policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SkillsConfig {
    /// Whether skills are used at all.
    pub enabled: bool,
    /// Auto-installation policy.
    pub auto_install: SkillAutoInstall,
    /// Registries to search, in order.
    pub registries: Vec<String>,
    /// Where installed skills live.
    pub install_dir: Option<String>,
    /// Whether the harness may generate a skill that does not exist.
    pub allow_creation: bool,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_install: SkillAutoInstall::TrustedOnly,
            registries: vec!["builtin".to_owned(), "local".to_owned()],
            install_dir: None,
            // Generating and running new code unprompted is the highest-risk
            // thing the harness can do, so it is opt-in.
            allow_creation: false,
        }
    }
}

/// Sandboxing and workspace protection.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SecurityConfig {
    /// Run untrusted code sandboxed.
    pub sandbox: bool,
    /// Command used to sandbox, when `sandbox` is set.
    pub sandbox_command: Option<String>,
    /// Whether destructive git and filesystem operations are permitted at all.
    pub allow_destructive_operations: bool,
    /// Snapshot the working tree before a task that may write.
    pub checkpoint_before_write: bool,
    /// Shell commands agents may run. Empty means "no allowlist enforced here".
    pub command_allowlist: Vec<String>,
    /// Hosts agents may reach. Empty means "no allowlist enforced here".
    pub network_allowlist: Vec<String>,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            sandbox: true,
            sandbox_command: None,
            // Defaults deny: `git reset --hard` and `rm -rf` require an
            // explicit opt-in, per the workspace-safety requirement.
            allow_destructive_operations: false,
            checkpoint_before_write: true,
            command_allowlist: Vec::new(),
            network_allowlist: Vec::new(),
        }
    }
}

/// Concurrency, retry and budget bounds.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimitsConfig {
    /// Tasks that may execute at once.
    pub max_parallel_tasks: usize,
    /// Attempts per task, across all agents. Never unbounded.
    pub max_retries: u32,
    /// Per-task deadline.
    pub task_timeout_secs: u64,
    /// Session spend ceiling in USD, when set.
    pub max_cost_usd: Option<f64>,
    /// Session token ceiling, when set.
    pub max_tokens: Option<u64>,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_parallel_tasks: 4,
            max_retries: 3,
            task_timeout_secs: 600,
            max_cost_usd: None,
            max_tokens: None,
        }
    }
}

/// Logging and tracing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TelemetryConfig {
    /// `error`, `warn`, `info`, `debug` or `trace`.
    pub log_level: String,
    /// Emit JSON lines instead of human-readable logs.
    pub json_logs: bool,
    /// Where to write the runtime database.
    pub database_path: Option<String>,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            log_level: "info".to_owned(),
            json_logs: false,
            database_path: None,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn cost_first_weights_cost_above_quality() {
        let w = RoutingStrategy::CostFirst.default_weights();
        assert!(w.cost > w.quality);
    }

    #[test]
    fn quality_first_weights_quality_above_cost() {
        let w = RoutingStrategy::QualityFirst.default_weights();
        assert!(w.quality > w.cost);
    }

    #[test]
    fn normalized_weights_sum_to_one() {
        let w = RouterWeights {
            quality: 3.0,
            cost: 1.0,
            latency: 1.0,
            reliability: 4.0,
            context: 1.0,
        };
        assert!((w.normalized().sum() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn normalization_preserves_relative_importance() {
        let w = RouterWeights {
            quality: 6.0,
            cost: 2.0,
            latency: 1.0,
            reliability: 1.0,
            context: 0.0,
        };
        let n = w.normalized();
        assert!((n.quality / n.cost - 3.0).abs() < 1e-9);
    }

    #[test]
    fn all_zero_weights_are_rejected() {
        let w = RouterWeights {
            quality: 0.0,
            cost: 0.0,
            latency: 0.0,
            reliability: 0.0,
            context: 0.0,
        };
        assert!(w.validate().is_err());
    }

    #[test]
    fn negative_weights_are_rejected() {
        let w = RouterWeights {
            quality: -1.0,
            ..RouterWeights::default()
        };
        assert!(w.validate().unwrap_err().to_string().contains("quality"));
    }

    #[test]
    fn zero_parallelism_is_rejected() {
        let config = Config {
            limits: LimitsConfig {
                max_parallel_tasks: 0,
                ..LimitsConfig::default()
            },
            ..Config::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn implausible_retry_counts_are_rejected() {
        let config = Config {
            limits: LimitsConfig {
                max_retries: 500,
                ..LimitsConfig::default()
            },
            ..Config::default()
        };
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("bounded")
        );
    }

    #[test]
    fn destructive_operations_and_creation_are_off_by_default() {
        let config = Config::default();
        assert!(!config.security.allow_destructive_operations);
        assert!(!config.skills.allow_creation);
        assert!(config.security.sandbox);
        assert_eq!(config.skills.auto_install, SkillAutoInstall::TrustedOnly);
    }
}
