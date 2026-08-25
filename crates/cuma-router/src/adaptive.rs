//! Adaptive routing: learning from what actually happened.
//!
//! History is bucketed by `(agent, model, task_type)`. The insight this
//! captures is that agent quality is not a single number — an agent that is
//! mediocre at documentation may be the best available at Rust debugging, and
//! a generic benchmark ranking would route around that strength forever.
//!
//! Deliberately *not* machine learning. This is a bucketed success rate with a
//! minimum-sample guard and a confidence-weighted blend. The blend point is
//! where a more sophisticated strategy would slot in later (see ADR-006);
//! introducing one now would add opacity to a system whose whole selling point
//! is explainable decisions.

use cuma_core::{AgentId, ErrorClass, ModelId, TaskType};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One recorded execution outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutcomeRecord {
    /// Which agent ran it.
    pub agent_id: AgentId,
    /// Which model, when known.
    pub model_id: Option<ModelId>,
    /// What kind of task it was.
    pub task_type: TaskType,
    /// Whether it succeeded.
    pub success: bool,
    /// How it failed, when it did.
    pub failure_class: Option<ErrorClass>,
    /// Wall-clock duration.
    pub latency_ms: u64,
    /// Tokens consumed.
    pub tokens: u64,
    /// Estimated USD, when pricing was known.
    pub estimated_cost_usd: Option<f64>,
    /// How many retries this attempt followed.
    pub retry_count: u32,
    /// Whether the user accepted the result, when they said.
    pub user_accepted: Option<bool>,
    /// When it happened.
    pub at: chrono::DateTime<chrono::Utc>,
}

impl OutcomeRecord {
    /// A successful outcome.
    pub fn success(
        agent_id: AgentId,
        model_id: Option<ModelId>,
        task_type: TaskType,
        latency_ms: u64,
        tokens: u64,
    ) -> Self {
        Self {
            agent_id,
            model_id,
            task_type,
            success: true,
            failure_class: None,
            latency_ms,
            tokens,
            estimated_cost_usd: None,
            retry_count: 0,
            user_accepted: None,
            at: chrono::Utc::now(),
        }
    }

    /// A failed outcome.
    pub fn failure(
        agent_id: AgentId,
        model_id: Option<ModelId>,
        task_type: TaskType,
        class: ErrorClass,
        latency_ms: u64,
    ) -> Self {
        Self {
            agent_id,
            model_id,
            task_type,
            success: false,
            failure_class: Some(class),
            latency_ms,
            tokens: 0,
            estimated_cost_usd: None,
            retry_count: 0,
            user_accepted: None,
            at: chrono::Utc::now(),
        }
    }
}

/// Aggregated statistics for one bucket.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct AdaptiveStats {
    /// Attempts recorded.
    pub attempts: u32,
    /// Successful attempts.
    pub successes: u32,
    /// Mean latency across attempts.
    pub mean_latency_ms: u64,
    /// Mean tokens across successful attempts.
    pub mean_tokens: u64,
}

impl AdaptiveStats {
    /// Observed success rate, or `None` with no attempts.
    pub fn success_rate(&self) -> Option<f64> {
        if self.attempts == 0 {
            return None;
        }
        Some(f64::from(self.successes) / f64::from(self.attempts))
    }

    fn record(&mut self, outcome: &OutcomeRecord) {
        let previous = self.attempts;
        self.attempts += 1;
        if outcome.success {
            self.successes += 1;
        }

        // Running means, so history costs O(1) memory per bucket regardless of
        // how long a session runs.
        self.mean_latency_ms = running_mean(self.mean_latency_ms, previous, outcome.latency_ms);
        if outcome.success {
            self.mean_tokens = running_mean(self.mean_tokens, previous, outcome.tokens);
        }
    }
}

fn running_mean(current: u64, count: u32, sample: u64) -> u64 {
    if count == 0 {
        return sample;
    }
    let n = u64::from(count);
    (current.saturating_mul(n).saturating_add(sample)) / (n + 1)
}

/// The bucket key. Model is optional because some agents never name one.
type Bucket = (AgentId, Option<ModelId>, TaskType);

/// Observed routing history.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RoutingHistory {
    buckets: BTreeMap<String, AdaptiveStats>,
}

fn key(bucket: &Bucket) -> String {
    let model = bucket.1.as_ref().map_or("*", ModelId::as_str);
    format!("{}|{}|{:?}", bucket.0, model, bucket.2)
}

impl RoutingHistory {
    /// Empty history.
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold an outcome into its bucket.
    pub fn record(&mut self, outcome: &OutcomeRecord) {
        let bucket = (
            outcome.agent_id.clone(),
            outcome.model_id.clone(),
            outcome.task_type,
        );
        self.buckets.entry(key(&bucket)).or_default().record(outcome);
    }

    /// Statistics for one bucket.
    pub fn stats(
        &self,
        agent: &AgentId,
        model: Option<&ModelId>,
        task_type: TaskType,
    ) -> Option<AdaptiveStats> {
        let bucket = (agent.clone(), model.cloned(), task_type);
        self.buckets.get(&key(&bucket)).copied()
    }

    /// Blend an observed success rate into a prior.
    ///
    /// Three guards keep this from being noise amplification:
    ///
    /// - Buckets below `min_samples` are ignored entirely. One lucky success
    ///   must not make an agent look perfect.
    /// - `adaptive_weight` caps how far history can move the prior.
    /// - Influence ramps with sample count up to `2 * min_samples`, so a bucket
    ///   that has just crossed the threshold does not immediately dominate.
    ///
    /// Returns `(blended_rate, applied)`; `applied` is false when history was
    /// insufficient, which the router surfaces as an explanation note.
    pub fn blend(
        &self,
        prior: f64,
        agent: &AgentId,
        model: Option<&ModelId>,
        task_type: TaskType,
        adaptive_weight: f64,
        min_samples: u32,
    ) -> (f64, bool) {
        let weight = adaptive_weight.clamp(0.0, 1.0);
        if weight <= 0.0 {
            return (prior, false);
        }

        let Some(stats) = self.stats(agent, model, task_type) else {
            return (prior, false);
        };
        let Some(observed) = stats.success_rate() else {
            return (prior, false);
        };

        let floor = min_samples.max(1);
        if stats.attempts < floor {
            return (prior, false);
        }

        let confidence = (f64::from(stats.attempts) / f64::from(floor * 2)).clamp(0.0, 1.0);
        let effective = weight * confidence;

        (
            (prior * (1.0 - effective) + observed * effective).clamp(0.0, 1.0),
            true,
        )
    }

    /// Every bucket, for reporting.
    pub fn buckets(&self) -> impl Iterator<Item = (&String, &AdaptiveStats)> {
        self.buckets.iter()
    }

    /// How many buckets have data.
    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    /// Whether nothing has been recorded.
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn record_many(history: &mut RoutingHistory, agent: &str, task_type: TaskType, results: &[bool]) {
        for &success in results {
            let outcome = if success {
                OutcomeRecord::success(AgentId::new(agent), None, task_type, 1000, 500)
            } else {
                OutcomeRecord::failure(
                    AgentId::new(agent),
                    None,
                    task_type,
                    ErrorClass::TaskFailure,
                    1000,
                )
            };
            history.record(&outcome);
        }
    }

    #[test]
    fn history_is_bucketed_by_task_type_not_just_by_agent() {
        let mut history = RoutingHistory::new();
        record_many(&mut history, "claude", TaskType::BugFix, &[true, true, true, true]);
        record_many(
            &mut history,
            "claude",
            TaskType::Documentation,
            &[false, false, false, false],
        );

        let debugging = history
            .stats(&AgentId::new("claude"), None, TaskType::BugFix)
            .unwrap();
        let docs = history
            .stats(&AgentId::new("claude"), None, TaskType::Documentation)
            .unwrap();

        assert_eq!(debugging.success_rate(), Some(1.0));
        assert_eq!(docs.success_rate(), Some(0.0));
    }

    #[test]
    fn a_single_lucky_success_does_not_influence_routing() {
        let mut history = RoutingHistory::new();
        record_many(&mut history, "lucky", TaskType::BugFix, &[true]);

        let (blended, applied) = history.blend(
            0.5,
            &AgentId::new("lucky"),
            None,
            TaskType::BugFix,
            1.0,
            5, // require 5 samples
        );

        assert!(!applied, "one sample is not evidence");
        assert_eq!(blended, 0.5);
    }

    #[test]
    fn enough_evidence_does_move_the_prior() {
        let mut history = RoutingHistory::new();
        record_many(&mut history, "good", TaskType::BugFix, &[true; 20]);

        let (blended, applied) =
            history.blend(0.5, &AgentId::new("good"), None, TaskType::BugFix, 0.5, 5);

        assert!(applied);
        assert!(blended > 0.5, "sustained success should raise the score");
    }

    #[test]
    fn sustained_failure_lowers_the_prior() {
        let mut history = RoutingHistory::new();
        record_many(&mut history, "bad", TaskType::BugFix, &[false; 20]);

        let (blended, applied) =
            history.blend(0.9, &AgentId::new("bad"), None, TaskType::BugFix, 0.5, 5);

        assert!(applied);
        assert!(blended < 0.9);
    }

    #[test]
    fn a_bucket_that_just_crossed_the_threshold_has_only_partial_influence() {
        let mut history = RoutingHistory::new();
        record_many(&mut history, "a", TaskType::BugFix, &[true; 5]);

        let (just_qualified, _) =
            history.blend(0.0, &AgentId::new("a"), None, TaskType::BugFix, 1.0, 5);

        let mut mature = RoutingHistory::new();
        record_many(&mut mature, "a", TaskType::BugFix, &[true; 50]);
        let (well_established, _) =
            mature.blend(0.0, &AgentId::new("a"), None, TaskType::BugFix, 1.0, 5);

        assert!(
            just_qualified < well_established,
            "confidence must ramp with evidence: {just_qualified} vs {well_established}"
        );
    }

    #[test]
    fn zero_adaptive_weight_disables_learning_entirely() {
        let mut history = RoutingHistory::new();
        record_many(&mut history, "a", TaskType::BugFix, &[false; 100]);

        let (blended, applied) =
            history.blend(0.95, &AgentId::new("a"), None, TaskType::BugFix, 0.0, 1);

        assert!(!applied);
        assert_eq!(blended, 0.95, "operators can turn this off completely");
    }

    #[test]
    fn the_documented_scenario_favours_the_agent_that_is_better_at_this_task() {
        // "Rust debugging: Codex 92%, Claude 96%, Gemini 73%" — even if a
        // generic benchmark ranked them differently.
        let mut history = RoutingHistory::new();
        record_many(&mut history, "codex", TaskType::BugFix, &[true; 92]);
        record_many(&mut history, "codex", TaskType::BugFix, &[false; 8]);
        record_many(&mut history, "claude", TaskType::BugFix, &[true; 96]);
        record_many(&mut history, "claude", TaskType::BugFix, &[false; 4]);
        record_many(&mut history, "gemini", TaskType::BugFix, &[true; 73]);
        record_many(&mut history, "gemini", TaskType::BugFix, &[false; 27]);

        // Every agent starts from the same static prior.
        let prior = 0.8;
        let blend = |agent: &str| {
            history
                .blend(prior, &AgentId::new(agent), None, TaskType::BugFix, 0.5, 10)
                .0
        };

        let (codex, claude, gemini) = (blend("codex"), blend("claude"), blend("gemini"));
        assert!(claude > codex, "{claude} should beat {codex}");
        assert!(codex > gemini, "{codex} should beat {gemini}");
    }

    #[test]
    fn statistics_track_mean_latency_and_tokens() {
        let mut history = RoutingHistory::new();
        for latency in [1000u64, 2000, 3000] {
            history.record(&OutcomeRecord::success(
                AgentId::new("a"),
                None,
                TaskType::General,
                latency,
                100,
            ));
        }

        let stats = history
            .stats(&AgentId::new("a"), None, TaskType::General)
            .unwrap();
        assert_eq!(stats.attempts, 3);
        assert_eq!(stats.mean_latency_ms, 2000);
        assert_eq!(stats.mean_tokens, 100);
    }

    #[test]
    fn an_agent_with_no_history_is_left_at_its_prior() {
        let history = RoutingHistory::new();
        let (blended, applied) =
            history.blend(0.7, &AgentId::new("unseen"), None, TaskType::General, 1.0, 1);
        assert!(!applied);
        assert_eq!(blended, 0.7);
    }
}
