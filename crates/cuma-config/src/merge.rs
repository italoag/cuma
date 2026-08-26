//! Field-level layer merging.
//!
//! The rule: a later layer overrides a field only if it actually *set* that
//! field. TOML deserialization with `#[serde(default)]` cannot distinguish
//! "absent" from "set to the default value", so merging compares against the
//! default and keeps the lower layer when the upper one matches it.
//!
//! This is a deliberate trade-off. It means a project config cannot override a
//! global `max_retries = 5` back down to the default `3` by writing `3`
//! explicitly — but that ambiguity is far less costly than the alternative,
//! where a project config that mentions one router key silently resets every
//! other router key to its default.

use crate::model::{Config, RouterWeights};

/// Overwrite `$target` with `$source` when `$source` differs from its default.
macro_rules! merge_field {
    ($target:expr, $source:expr, $default:expr) => {
        if $source != $default {
            $target = $source;
        }
    };
}

/// Replace `$target` when `$source` is `Some`.
macro_rules! merge_option {
    ($target:expr, $source:expr) => {
        if $source.is_some() {
            $target = $source;
        }
    };
}

/// Replace `$target` when `$source` is a non-empty collection.
macro_rules! merge_collection {
    ($target:expr, $source:expr) => {
        if !$source.is_empty() {
            $target = $source;
        }
    };
}

impl Config {
    /// Merge `other` over `self`, field by field.
    pub fn merge(&mut self, other: Config) {
        let d = Config::default();

        // --- router -------------------------------------------------------
        merge_field!(
            self.router.strategy,
            other.router.strategy,
            d.router.strategy
        );

        // Weights are merged only when the layer wrote a non-default block.
        // Otherwise a layer that sets `strategy` alone would clobber the
        // weights it is supposed to be selecting a preset for.
        if other.router.weights != RouterWeights::default() {
            self.router.weights = other.router.weights;
        } else if other.router.strategy != d.router.strategy {
            // The layer chose a strategy but no explicit weights: adopt the
            // strategy's preset rather than leaving the previous layer's.
            self.router.weights = other.router.strategy.default_weights();
        }

        merge_option!(self.router.pin_agent, other.router.pin_agent);
        merge_option!(self.router.pin_model, other.router.pin_model);
        merge_collection!(self.router.exclude_agents, other.router.exclude_agents);
        merge_collection!(self.router.exclude_models, other.router.exclude_models);
        merge_field!(
            self.router.adaptive_weight,
            other.router.adaptive_weight,
            d.router.adaptive_weight
        );
        merge_field!(
            self.router.adaptive_min_samples,
            other.router.adaptive_min_samples,
            d.router.adaptive_min_samples
        );

        // --- agents -------------------------------------------------------
        // Keyed, so this is a per-agent upsert rather than a wholesale replace:
        // a project may add one agent without dropping the global set.
        for (id, agent) in other.agents {
            self.agents.insert(id, agent);
        }

        // --- memory -------------------------------------------------------
        merge_field!(self.memory.enabled, other.memory.enabled, d.memory.enabled);
        merge_field!(self.memory.backend, other.memory.backend, d.memory.backend);
        merge_option!(self.memory.command, other.memory.command);
        merge_field!(
            self.memory.recall_limit,
            other.memory.recall_limit,
            d.memory.recall_limit
        );

        // --- rtk ----------------------------------------------------------
        merge_field!(self.rtk.enabled, other.rtk.enabled, d.rtk.enabled);
        merge_option!(self.rtk.binary, other.rtk.binary);

        // --- skills -------------------------------------------------------
        merge_field!(self.skills.enabled, other.skills.enabled, d.skills.enabled);
        merge_field!(
            self.skills.auto_install,
            other.skills.auto_install,
            d.skills.auto_install
        );
        merge_collection!(self.skills.registries, other.skills.registries);
        merge_option!(self.skills.install_dir, other.skills.install_dir);
        merge_field!(
            self.skills.allow_creation,
            other.skills.allow_creation,
            d.skills.allow_creation
        );

        // --- security -----------------------------------------------------
        merge_field!(
            self.security.sandbox,
            other.security.sandbox,
            d.security.sandbox
        );
        merge_option!(
            self.security.sandbox_command,
            other.security.sandbox_command
        );
        merge_field!(
            self.security.allow_destructive_operations,
            other.security.allow_destructive_operations,
            d.security.allow_destructive_operations
        );
        merge_field!(
            self.security.checkpoint_before_write,
            other.security.checkpoint_before_write,
            d.security.checkpoint_before_write
        );
        merge_collection!(
            self.security.command_allowlist,
            other.security.command_allowlist
        );
        merge_collection!(
            self.security.network_allowlist,
            other.security.network_allowlist
        );

        // --- limits -------------------------------------------------------
        merge_field!(
            self.limits.max_parallel_tasks,
            other.limits.max_parallel_tasks,
            d.limits.max_parallel_tasks
        );
        merge_field!(
            self.limits.max_retries,
            other.limits.max_retries,
            d.limits.max_retries
        );
        merge_field!(
            self.limits.task_timeout_secs,
            other.limits.task_timeout_secs,
            d.limits.task_timeout_secs
        );
        merge_option!(self.limits.max_cost_usd, other.limits.max_cost_usd);
        merge_option!(self.limits.max_tokens, other.limits.max_tokens);

        // --- telemetry ----------------------------------------------------
        merge_field!(
            self.telemetry.log_level,
            other.telemetry.log_level,
            d.telemetry.log_level
        );
        merge_field!(
            self.telemetry.json_logs,
            other.telemetry.json_logs,
            d.telemetry.json_logs
        );
        merge_option!(self.telemetry.database_path, other.telemetry.database_path);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::model::RoutingStrategy;

    #[test]
    fn a_partial_layer_leaves_untouched_fields_alone() {
        let mut base = Config::from_toml(
            r#"
            [limits]
            max_retries = 7
            max_parallel_tasks = 8
            "#,
        )
        .unwrap();

        let overlay = Config::from_toml(
            r#"
            [limits]
            max_parallel_tasks = 2
            "#,
        )
        .unwrap();

        base.merge(overlay);
        assert_eq!(base.limits.max_parallel_tasks, 2, "overridden");
        assert_eq!(base.limits.max_retries, 7, "not mentioned, so preserved");
    }

    #[test]
    fn choosing_a_strategy_adopts_its_weight_preset() {
        let mut base = Config::default();
        let overlay = Config::from_toml("[router]\nstrategy = \"cost-first\"\n").unwrap();

        base.merge(overlay);
        assert_eq!(base.router.strategy, RoutingStrategy::CostFirst);
        assert!(base.router.weights.cost > base.router.weights.quality);
    }

    #[test]
    fn explicit_weights_beat_the_strategy_preset() {
        let mut base = Config::default();
        let overlay = Config::from_toml(
            r#"
            [router]
            strategy = "cost-first"
            [router.weights]
            quality = 0.9
            cost = 0.1
            latency = 0.0
            reliability = 0.0
            context = 0.0
            "#,
        )
        .unwrap();

        base.merge(overlay);
        assert_eq!(base.router.strategy, RoutingStrategy::CostFirst);
        assert!(
            (base.router.weights.quality - 0.9).abs() < 1e-9,
            "an explicit block must win over the preset"
        );
    }

    #[test]
    fn agents_are_upserted_not_replaced_wholesale() {
        let mut base = Config::from_toml(
            r#"
            [agents.codex]
            enabled = true
            [agents.claude]
            enabled = true
            "#,
        )
        .unwrap();

        let overlay = Config::from_toml(
            r#"
            [agents.claude]
            enabled = false
            [agents.gemini]
            enabled = true
            "#,
        )
        .unwrap();

        base.merge(overlay);
        assert_eq!(base.agents.len(), 3);
        assert!(base.agents["codex"].enabled, "untouched agent survives");
        assert!(!base.agents["claude"].enabled, "overridden agent updated");
        assert!(base.agents.contains_key("gemini"), "new agent added");
    }

    #[test]
    fn a_project_layer_can_pin_an_agent_over_a_global_one() {
        let mut base = Config::from_toml("[router]\npin_agent = \"codex\"\n").unwrap();
        let overlay = Config::from_toml("[router]\npin_agent = \"claude\"\n").unwrap();
        base.merge(overlay);
        assert_eq!(base.router.pin_agent.as_deref(), Some("claude"));
    }

    #[test]
    fn an_empty_layer_changes_nothing() {
        let mut base = Config::from_toml(
            r#"
            [router]
            strategy = "quality-first"
            [limits]
            max_retries = 5
            [security]
            sandbox = false
            "#,
        )
        .unwrap();
        let before = format!("{base:?}");

        base.merge(Config::from_toml("").unwrap());
        assert_eq!(format!("{base:?}"), before);
    }

    #[test]
    fn a_layer_can_turn_a_default_on_flag_off() {
        let mut base = Config::default();
        assert!(base.security.sandbox);
        base.merge(Config::from_toml("[security]\nsandbox = false\n").unwrap());
        assert!(!base.security.sandbox);
    }
}
