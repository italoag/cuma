//! The `CUMA_*` environment layer.
//!
//! Environment variables sit above files so that CI and container deployments
//! can override anything without writing a file. Only a curated set of keys is
//! supported: a generic `CUMA_A__B__C` scheme reads well in documentation and
//! badly in practice, because a typo becomes a silently ignored variable.

use crate::model::{Config, RoutingStrategy, RtkMode, SkillAutoInstall};
use cuma_core::error::{MetaAgentError, Result};

/// Apply every recognized `CUMA_*` variable to `config`.
///
/// Returns whether any variable was applied, so the caller can record the
/// environment as a contributing layer. A variable that is set but unparseable
/// is an error rather than a warning — an operator who wrote
/// `CUMA_MAX_RETRIES=three` needs to know their retry limit is not what they
/// think it is.
pub fn apply(config: &mut Config) -> Result<bool> {
    let mut applied = false;

    if let Some(raw) = var("CUMA_ROUTER_STRATEGY") {
        config.router.strategy = parse_strategy(&raw)?;
        config.router.weights = config.router.strategy.default_weights();
        applied = true;
    }

    if let Some(raw) = var("CUMA_ROUTER_PIN_AGENT") {
        config.router.pin_agent = Some(raw);
        applied = true;
    }

    if let Some(raw) = var("CUMA_ROUTER_PIN_MODEL") {
        config.router.pin_model = Some(raw);
        applied = true;
    }

    if let Some(raw) = var("CUMA_ROUTER_EXCLUDE_AGENTS") {
        config.router.exclude_agents = split_list(&raw);
        applied = true;
    }

    if let Some(raw) = var("CUMA_MAX_PARALLEL_TASKS") {
        config.limits.max_parallel_tasks = parse_num(&raw, "CUMA_MAX_PARALLEL_TASKS")?;
        applied = true;
    }

    if let Some(raw) = var("CUMA_MAX_RETRIES") {
        config.limits.max_retries = parse_num(&raw, "CUMA_MAX_RETRIES")?;
        applied = true;
    }

    if let Some(raw) = var("CUMA_TASK_TIMEOUT_SECS") {
        config.limits.task_timeout_secs = parse_num(&raw, "CUMA_TASK_TIMEOUT_SECS")?;
        applied = true;
    }

    if let Some(raw) = var("CUMA_MAX_COST_USD") {
        config.limits.max_cost_usd = Some(parse_num(&raw, "CUMA_MAX_COST_USD")?);
        applied = true;
    }

    if let Some(raw) = var("CUMA_LOG_LEVEL") {
        config.telemetry.log_level = raw;
        applied = true;
    }

    if let Some(raw) = var("CUMA_JSON_LOGS") {
        config.telemetry.json_logs = parse_bool(&raw, "CUMA_JSON_LOGS")?;
        applied = true;
    }

    if let Some(raw) = var("CUMA_DATABASE_PATH") {
        config.telemetry.database_path = Some(raw);
        applied = true;
    }

    if let Some(raw) = var("CUMA_MEMORY_ENABLED") {
        config.memory.enabled = parse_bool(&raw, "CUMA_MEMORY_ENABLED")?;
        applied = true;
    }

    if let Some(raw) = var("CUMA_RTK") {
        config.rtk.enabled = match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => RtkMode::Auto,
            "always" | "true" | "1" => RtkMode::Always,
            "never" | "false" | "0" => RtkMode::Never,
            other => {
                return Err(MetaAgentError::Configuration(format!(
                    "CUMA_RTK must be auto, always or never, got {other:?}"
                )));
            }
        };
        applied = true;
    }

    if let Some(raw) = var("CUMA_SANDBOX") {
        config.security.sandbox = parse_bool(&raw, "CUMA_SANDBOX")?;
        applied = true;
    }

    if let Some(raw) = var("CUMA_SKILLS_AUTO_INSTALL") {
        config.skills.auto_install = match raw.trim().to_ascii_lowercase().as_str() {
            "never" => SkillAutoInstall::Never,
            "trusted-only" | "trusted_only" => SkillAutoInstall::TrustedOnly,
            "verified" => SkillAutoInstall::Verified,
            other => {
                return Err(MetaAgentError::Configuration(format!(
                    "CUMA_SKILLS_AUTO_INSTALL must be never, trusted-only or verified, got {other:?}"
                )));
            }
        };
        applied = true;
    }

    Ok(applied)
}

/// Read a variable, treating empty as unset.
fn var(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn split_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

fn parse_strategy(raw: &str) -> Result<RoutingStrategy> {
    match raw.trim().to_ascii_lowercase().replace('_', "-").as_str() {
        "balanced" => Ok(RoutingStrategy::Balanced),
        "quality-first" => Ok(RoutingStrategy::QualityFirst),
        "cost-first" => Ok(RoutingStrategy::CostFirst),
        "latency-first" => Ok(RoutingStrategy::LatencyFirst),
        "local-first" => Ok(RoutingStrategy::LocalFirst),
        "privacy-first" => Ok(RoutingStrategy::PrivacyFirst),
        "manual" => Ok(RoutingStrategy::Manual),
        other => Err(MetaAgentError::Configuration(format!(
            "unknown routing strategy {other:?}"
        ))),
    }
}

fn parse_num<T: std::str::FromStr>(raw: &str, key: &str) -> Result<T> {
    raw.trim()
        .parse()
        .map_err(|_| MetaAgentError::Configuration(format!("{key} must be a number, got {raw:?}")))
}

fn parse_bool(raw: &str, key: &str) -> Result<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        other => Err(MetaAgentError::Configuration(format!(
            "{key} must be a boolean, got {other:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn known_strategies_parse_in_any_casing_or_separator() {
        assert_eq!(
            parse_strategy("Cost_First").unwrap(),
            RoutingStrategy::CostFirst
        );
        assert_eq!(
            parse_strategy(" balanced ").unwrap(),
            RoutingStrategy::Balanced
        );
    }

    #[test]
    fn an_unknown_strategy_is_an_error_not_a_fallback_to_balanced() {
        assert!(parse_strategy("cheapest").is_err());
    }

    #[test]
    fn booleans_accept_the_usual_spellings() {
        for truthy in ["1", "true", "YES", "on"] {
            assert!(parse_bool(truthy, "K").unwrap());
        }
        for falsy in ["0", "false", "No", "OFF"] {
            assert!(!parse_bool(falsy, "K").unwrap());
        }
    }

    #[test]
    fn an_unparseable_number_names_the_variable_that_was_wrong() {
        let err = parse_num::<u32>("three", "CUMA_MAX_RETRIES").unwrap_err();
        assert!(err.to_string().contains("CUMA_MAX_RETRIES"));
    }

    #[test]
    fn comma_lists_are_split_and_trimmed() {
        assert_eq!(
            split_list("codex, claude ,, gemini"),
            vec!["codex", "claude", "gemini"]
        );
    }
}
