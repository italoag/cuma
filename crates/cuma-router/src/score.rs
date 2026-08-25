//! The scoring functions.
//!
//! Each dimension normalizes to `[0.0, 1.0]` where **higher is always better**.
//! Cost and latency are therefore inverted: a cheap candidate scores near 1.0.
//! Keeping every dimension in the same direction is what lets the weighted sum
//! be a plain dot product instead of a pile of sign conventions.
//!
//! Unknown metrics score at a neutral midpoint, never at the best value.
//! Rewarding an agent for disclosing nothing would make silence the winning
//! strategy.

use cuma_core::{AgentDescriptor, Known, ModelDescriptor, Task};

/// The neutral score for a metric nobody reported.
const UNKNOWN: f64 = 0.5;

/// Raw, unweighted dimension scores.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DimensionScores {
    /// Capability coverage blended with model quality.
    pub quality: f64,
    /// Cost desirability (inverted price).
    pub cost: f64,
    /// Latency desirability (inverted latency).
    pub latency: f64,
    /// Historical success blended with current health.
    pub reliability: f64,
    /// Context-window headroom.
    pub context: f64,
}

/// The reference price used to normalize cost, in USD per million tokens.
///
/// Anything at or above this scores 0.0. It is chosen to sit above premium
/// frontier pricing so that real models spread across the range rather than
/// all bunching at the bottom.
const COST_CEILING_PER_MTOK: f64 = 30.0;

/// The reference latency used to normalize speed, in milliseconds.
const LATENCY_CEILING_MS: f64 = 120_000.0;

/// Score capability coverage and model quality together.
///
/// Coverage dominates: a fast, cheap agent that cannot do the work is worth
/// less than a slow one that can. Quality signals only break ties among
/// candidates that all cover the requirements.
pub fn quality(agent: &AgentDescriptor, model: Option<&ModelDescriptor>, task: &Task) -> f64 {
    let effective = agent.effective_capabilities(model);
    let coverage = effective
        .match_against(&task.spec.required_capabilities)
        .score;

    let performance = model
        .map(|m| &m.performance)
        .unwrap_or(&agent.performance_profile);

    let coding = performance.coding_score.or(UNKNOWN);
    let reasoning = performance.reasoning_score.or(UNKNOWN);

    // Harder tasks lean more on reasoning; routine ones on coding throughput.
    let complexity = task.spec.complexity.clamp(0.0, 1.0);
    let skill = coding * (1.0 - complexity) + reasoning * complexity;

    (coverage * 0.7 + skill * 0.3).clamp(0.0, 1.0)
}

/// Score cost desirability from a blended per-token price.
///
/// Output tokens are weighted more heavily than input because coding tasks
/// generate substantially more output than a chat turn, and output is the more
/// expensive side on every major provider.
pub fn cost(agent: &AgentDescriptor, model: Option<&ModelDescriptor>) -> (f64, bool) {
    let profile = model.map(|m| &m.cost).unwrap_or(&agent.cost_profile);

    let (Some(input), Some(output)) = (
        profile.input_per_mtok.value(),
        profile.output_per_mtok.value(),
    ) else {
        return (UNKNOWN, false);
    };

    let blended = input * 0.4 + output * 0.6;
    let score = 1.0 - (blended / COST_CEILING_PER_MTOK).clamp(0.0, 1.0);
    (score, true)
}

/// Score latency desirability, preferring a live measurement over a profile.
pub fn latency(agent: &AgentDescriptor, model: Option<&ModelDescriptor>) -> (f64, bool) {
    // A measurement from the last call beats an advertised average.
    let observed = agent.health.last_latency_ms.value();
    let profiled = model
        .map(|m| &m.performance)
        .unwrap_or(&agent.performance_profile)
        .typical_latency_ms
        .value();

    let Some(ms) = observed.or(profiled) else {
        return (UNKNOWN, false);
    };

    #[allow(clippy::cast_precision_loss)]
    let score = 1.0 - (ms as f64 / LATENCY_CEILING_MS).clamp(0.0, 1.0);
    (score, true)
}

/// Score reliability from history and current health.
///
/// A degraded agent is multiplied down rather than excluded: the circuit
/// breaker decides exclusion, and duplicating that here would make one
/// mechanism silently override the other.
pub fn reliability(agent: &AgentDescriptor, model: Option<&ModelDescriptor>) -> (f64, bool) {
    let performance = model
        .map(|m| &m.performance)
        .unwrap_or(&agent.performance_profile);

    let (base, known) = match performance.historical_success_rate {
        Known::Reported(rate) | Known::Estimated(rate) => (rate.clamp(0.0, 1.0), true),
        Known::Unknown => (UNKNOWN, false),
    };

    let health_multiplier = match agent.health.state {
        cuma_core::HealthState::Healthy => 1.0,
        cuma_core::HealthState::Degraded => 0.6,
        cuma_core::HealthState::RateLimited => 0.3,
        // Unknown health is not penalised: a brand-new agent has done nothing
        // wrong, and penalising it would prevent it ever accumulating history.
        cuma_core::HealthState::Unknown => 1.0,
        cuma_core::HealthState::Unavailable => 0.0,
    };

    ((base * health_multiplier).clamp(0.0, 1.0), known)
}

/// Score how comfortably the task fits the model's context window.
///
/// Fitting exactly is not as good as fitting with headroom: an agent needs
/// room for its own tool output and reasoning, and a prompt that fills the
/// window leaves none.
pub fn context(model: Option<&ModelDescriptor>, estimated_tokens: Option<u64>) -> (f64, bool) {
    let (Some(model), Some(needed)) = (model, estimated_tokens) else {
        return (UNKNOWN, false);
    };

    let Some(window) = model.context_window.value() else {
        return (UNKNOWN, false);
    };

    if needed >= window {
        return (0.0, true);
    }

    #[allow(clippy::cast_precision_loss)]
    let headroom = 1.0 - (needed as f64 / window as f64);
    // Two thirds free is already comfortable; scale so that saturates at 1.0
    // rather than only rewarding models with absurdly large windows.
    ((headroom / 0.66).clamp(0.0, 1.0), true)
}

/// Score every dimension for one candidate.
pub fn all(
    agent: &AgentDescriptor,
    model: Option<&ModelDescriptor>,
    task: &Task,
) -> (DimensionScores, Vec<String>) {
    let mut notes = Vec::new();

    let quality_score = quality(agent, model, task);

    let (cost_score, cost_known) = cost(agent, model);
    if !cost_known {
        notes.push("pricing unknown; cost scored neutrally".to_owned());
    }

    let (latency_score, latency_known) = latency(agent, model);
    if !latency_known {
        notes.push("no latency data; scored neutrally".to_owned());
    }

    let (reliability_score, reliability_known) = reliability(agent, model);
    if !reliability_known {
        notes.push("no execution history yet; reliability scored neutrally".to_owned());
    }

    let (context_score, context_known) = context(model, task.spec.estimated_tokens);
    if !context_known {
        notes.push("context window or task size unknown; scored neutrally".to_owned());
    }

    let missing = agent
        .effective_capabilities(model)
        .match_against(&task.spec.required_capabilities)
        .missing;
    if !missing.is_empty() {
        let names: Vec<String> = missing.iter().map(ToString::to_string).collect();
        notes.push(format!("missing capabilities: {}", names.join(", ")));
    }

    (
        DimensionScores {
            quality: quality_score,
            cost: cost_score,
            latency: latency_score,
            reliability: reliability_score,
            context: context_score,
        },
        notes,
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use cuma_core::{
        AgentProtocol, Capability, CapabilitySet, CostProfile, HealthState, PerformanceProfile,
        TaskSpec, TaskType,
    };

    fn task(task_type: TaskType) -> Task {
        Task::new(TaskSpec::new("do it", task_type))
    }

    fn agent(id: &str, caps: CapabilitySet) -> AgentDescriptor {
        AgentDescriptor::new(id, id, AgentProtocol::Acp).with_capabilities(caps)
    }

    #[test]
    fn covering_every_required_capability_beats_covering_none() {
        let t = task(TaskType::BugFix);
        let capable = agent("capable", t.spec.required_capabilities.clone());
        let incapable = agent("incapable", CapabilitySet::new());

        assert!(quality(&capable, None, &t) > quality(&incapable, None, &t));
    }

    #[test]
    fn a_cheap_model_scores_higher_on_cost_than_an_expensive_one() {
        let a = agent("a", CapabilitySet::new());

        let mut cheap = ModelDescriptor::minimal(a.id.clone(), "cheap", "cheap");
        cheap.cost = CostProfile {
            input_per_mtok: Known::Reported(0.25),
            output_per_mtok: Known::Reported(1.25),
            cache_read_per_mtok: Known::Unknown,
        };

        let mut pricey = ModelDescriptor::minimal(a.id.clone(), "pricey", "pricey");
        pricey.cost = CostProfile {
            input_per_mtok: Known::Reported(15.0),
            output_per_mtok: Known::Reported(75.0),
            cache_read_per_mtok: Known::Unknown,
        };

        let (cheap_score, _) = cost(&a, Some(&cheap));
        let (pricey_score, _) = cost(&a, Some(&pricey));
        assert!(cheap_score > pricey_score);
        assert!(cheap_score > 0.9);
        assert_eq!(pricey_score, 0.0, "above the ceiling bottoms out");
    }

    #[test]
    fn unknown_pricing_is_neutral_not_free() {
        let a = agent("a", CapabilitySet::new());
        let (score, known) = cost(&a, None);
        assert_eq!(score, UNKNOWN);
        assert!(!known);
        assert!(
            score < 1.0,
            "silence must not be rewarded as if it were free"
        );
    }

    #[test]
    fn an_observed_latency_overrides_an_advertised_one() {
        let mut a = agent("a", CapabilitySet::new());
        a.performance_profile.typical_latency_ms = Known::Reported(100_000);
        a.health.last_latency_ms = Known::Reported(1_000);

        let (score, known) = latency(&a, None);
        assert!(known);
        assert!(score > 0.9, "the fast measurement should win, got {score}");
    }

    #[test]
    fn a_degraded_agent_is_penalised_but_not_zeroed() {
        let mut a = agent("a", CapabilitySet::new());
        a.performance_profile.historical_success_rate = Known::Reported(1.0);

        a.health.state = HealthState::Healthy;
        let (healthy, _) = reliability(&a, None);

        a.health.state = HealthState::Degraded;
        let (degraded, _) = reliability(&a, None);

        assert!(degraded < healthy);
        assert!(
            degraded > 0.0,
            "exclusion is the breaker's job, not scoring's"
        );
    }

    #[test]
    fn a_brand_new_agent_is_not_penalised_for_having_no_history() {
        let mut a = agent("new", CapabilitySet::new());
        a.health.state = HealthState::Unknown;

        let (score, known) = reliability(&a, None);
        assert!(!known);
        assert_eq!(score, UNKNOWN, "it has not failed; it has just not run");
    }

    #[test]
    fn a_task_that_overflows_the_window_scores_zero_on_context() {
        let a = agent("a", CapabilitySet::new());
        let mut model = ModelDescriptor::minimal(a.id.clone(), "small", "small");
        model.context_window = Known::Reported(8_000);

        let (score, known) = context(Some(&model), Some(100_000));
        assert!(known);
        assert_eq!(score, 0.0);
    }

    #[test]
    fn headroom_scores_better_than_a_tight_fit() {
        let a = agent("a", CapabilitySet::new());
        let mut model = ModelDescriptor::minimal(a.id.clone(), "m", "m");
        model.context_window = Known::Reported(100_000);

        let (roomy, _) = context(Some(&model), Some(10_000));
        let (tight, _) = context(Some(&model), Some(95_000));
        assert!(roomy > tight);
        assert_eq!(roomy, 1.0);
    }

    #[test]
    fn every_dimension_stays_within_zero_and_one() {
        let t = task(TaskType::Implementation);
        let mut a = agent("a", t.spec.required_capabilities.clone());
        a.performance_profile = PerformanceProfile {
            typical_latency_ms: Known::Reported(u64::MAX),
            throughput_tps: Known::Reported(f64::MAX),
            historical_success_rate: Known::Reported(99.0),
            coding_score: Known::Reported(50.0),
            reasoning_score: Known::Reported(-3.0),
        };

        let (scores, _) = all(&a, None, &t);
        for value in [
            scores.quality,
            scores.cost,
            scores.latency,
            scores.reliability,
            scores.context,
        ] {
            assert!(
                (0.0..=1.0).contains(&value),
                "score escaped its range: {value}"
            );
        }
    }

    #[test]
    fn missing_capabilities_are_reported_as_a_note() {
        let t = task(TaskType::Implementation);
        let a = agent("weak", CapabilitySet::new().with(Capability::Research));

        let (_, notes) = all(&a, None, &t);
        assert!(
            notes.iter().any(|n| n.contains("missing capabilities")),
            "notes were: {notes:?}"
        );
    }
}
