//! Usage accounting.
//!
//! The one rule this crate exists to enforce: **an estimate is never presented
//! as a measurement.** Agents differ in what they report — some give exact
//! token counts, some give nothing — and a dashboard that silently fills the
//! gaps with guesses is worse than one that admits it does not know.
//!
//! Every aggregate therefore carries how much of it was reported, and cost is
//! `None` rather than `$0.00` when pricing is unknown.

use cuma_core::{
    AgentId, AttemptId, CostProfile, ErrorClass, ModelId, SessionId, TaskId, TaskType, TokenUsage,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One attempt's usage record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRecord {
    /// The attempt.
    pub attempt_id: AttemptId,
    /// The session.
    pub session_id: SessionId,
    /// The task.
    pub task_id: TaskId,
    /// What kind of task it was.
    pub task_type: TaskType,
    /// Which agent ran it.
    pub agent_id: AgentId,
    /// Which model, when known.
    pub model_id: Option<ModelId>,
    /// The upstream provider, when the agent disclosed one.
    pub provider: Option<String>,
    /// When the attempt started.
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// Wall-clock duration.
    pub latency_ms: u64,
    /// Tokens, with their provenance.
    pub tokens: TokenUsage,
    /// Estimated USD. `None` when pricing is unknown — never `Some(0.0)`.
    pub estimated_cost_usd: Option<f64>,
    /// Whether the attempt succeeded.
    pub success: bool,
    /// How it failed, when it did.
    pub failure_class: Option<ErrorClass>,
    /// How many retries preceded this attempt.
    pub retry_count: u32,
}

/// Totals for one grouping (an agent, a model, a session).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageTotals {
    /// Attempts counted.
    pub attempts: u32,
    /// Successful attempts.
    pub successes: u32,
    /// Input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Cached tokens.
    pub cached_tokens: u64,
    /// Summed USD across attempts whose pricing was known.
    pub estimated_cost_usd: f64,
    /// Attempts whose pricing was unknown, so cost is incomplete.
    pub attempts_without_pricing: u32,
    /// Attempts whose token counts we estimated rather than received.
    pub attempts_with_estimated_tokens: u32,
    /// Summed latency, for computing a mean.
    pub total_latency_ms: u64,
    /// Retries counted across attempts.
    pub retries: u32,
}

impl UsageTotals {
    /// Fold one record in.
    pub fn add(&mut self, record: &UsageRecord) {
        self.attempts += 1;
        if record.success {
            self.successes += 1;
        }

        self.input_tokens = self.input_tokens.saturating_add(record.tokens.input);
        self.output_tokens = self.output_tokens.saturating_add(record.tokens.output);
        self.cached_tokens = self.cached_tokens.saturating_add(record.tokens.cached);

        if !record.tokens.reported {
            self.attempts_with_estimated_tokens += 1;
        }

        match record.estimated_cost_usd {
            Some(cost) => self.estimated_cost_usd += cost,
            None => self.attempts_without_pricing += 1,
        }

        self.total_latency_ms = self.total_latency_ms.saturating_add(record.latency_ms);
        self.retries += record.retry_count;
    }

    /// Total tokens.
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }

    /// Success rate, or `None` with no attempts.
    pub fn success_rate(&self) -> Option<f64> {
        if self.attempts == 0 {
            return None;
        }
        Some(f64::from(self.successes) / f64::from(self.attempts))
    }

    /// Mean latency, or `None` with no attempts.
    pub fn mean_latency_ms(&self) -> Option<u64> {
        if self.attempts == 0 {
            return None;
        }
        Some(self.total_latency_ms / u64::from(self.attempts))
    }

    /// Whether every attempt reported both tokens and pricing.
    ///
    /// The TUI marks a total with a `~` when this is false, so a number that
    /// is partly guessed never looks like one that is not.
    pub fn is_complete(&self) -> bool {
        self.attempts_without_pricing == 0 && self.attempts_with_estimated_tokens == 0
    }

    /// The cost, formatted with a marker when it is incomplete.
    pub fn render_cost(&self) -> String {
        if self.attempts == 0 {
            return "-".to_owned();
        }
        if self.attempts_without_pricing == self.attempts {
            return "unknown".to_owned();
        }
        if self.attempts_without_pricing > 0 {
            return format!(
                "≥${:.4} ({} of {} attempts unpriced)",
                self.estimated_cost_usd, self.attempts_without_pricing, self.attempts
            );
        }
        format!("~${:.4}", self.estimated_cost_usd)
    }
}

/// The session-wide usage ledger.
#[derive(Debug, Clone, Default)]
pub struct UsageTracker {
    records: Vec<UsageRecord>,
    rtk_tokens_saved: u64,
}

impl UsageTracker {
    /// An empty ledger.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an attempt.
    pub fn record(&mut self, record: UsageRecord) {
        self.records.push(record);
    }

    /// Record tokens RTK kept out of the context.
    pub fn record_rtk_saving(&mut self, tokens: u64) {
        self.rtk_tokens_saved = self.rtk_tokens_saved.saturating_add(tokens);
    }

    /// Tokens RTK saved this session.
    pub fn rtk_tokens_saved(&self) -> u64 {
        self.rtk_tokens_saved
    }

    /// Every record.
    pub fn records(&self) -> &[UsageRecord] {
        &self.records
    }

    /// Session totals.
    pub fn totals(&self) -> UsageTotals {
        let mut totals = UsageTotals::default();
        for record in &self.records {
            totals.add(record);
        }
        totals
    }

    /// USD spent so far, counting only attempts with known pricing.
    ///
    /// This is what budget enforcement uses. It is a *lower bound*: unpriced
    /// attempts are not counted because inventing a number for them would
    /// make the budget arbitrary.
    pub fn spent_usd(&self) -> f64 {
        self.records
            .iter()
            .filter_map(|r| r.estimated_cost_usd)
            .sum()
    }

    /// Totals per agent.
    pub fn by_agent(&self) -> BTreeMap<AgentId, UsageTotals> {
        let mut grouped: BTreeMap<AgentId, UsageTotals> = BTreeMap::new();
        for record in &self.records {
            grouped
                .entry(record.agent_id.clone())
                .or_default()
                .add(record);
        }
        grouped
    }

    /// Totals per agent+model.
    pub fn by_model(&self) -> BTreeMap<(AgentId, ModelId), UsageTotals> {
        let mut grouped: BTreeMap<(AgentId, ModelId), UsageTotals> = BTreeMap::new();
        for record in &self.records {
            let Some(model) = &record.model_id else {
                continue;
            };
            grouped
                .entry((record.agent_id.clone(), model.clone()))
                .or_default()
                .add(record);
        }
        grouped
    }

    /// Totals per task type.
    pub fn by_task_type(&self) -> BTreeMap<TaskType, UsageTotals> {
        let mut grouped: BTreeMap<TaskType, UsageTotals> = BTreeMap::new();
        for record in &self.records {
            grouped.entry(record.task_type).or_default().add(record);
        }
        grouped
    }

    /// Fraction of attempts that were retries, or `None` with no attempts.
    pub fn retry_rate(&self) -> Option<f64> {
        let totals = self.totals();
        if totals.attempts == 0 {
            return None;
        }
        Some(f64::from(totals.retries) / f64::from(totals.attempts))
    }

    /// How often each failure class occurred.
    pub fn failure_breakdown(&self) -> BTreeMap<String, u32> {
        let mut counts: BTreeMap<String, u32> = BTreeMap::new();
        for record in self.records.iter().filter(|r| !r.success) {
            let label = record
                .failure_class
                .map_or_else(|| "unclassified".to_owned(), |c| format!("{c:?}"));
            *counts.entry(label).or_insert(0) += 1;
        }
        counts
    }

    /// Mean USD per successful task, when any attempt was priced.
    pub fn cost_per_success(&self) -> Option<f64> {
        let totals = self.totals();
        if totals.successes == 0 || totals.attempts_without_pricing == totals.attempts {
            return None;
        }
        Some(totals.estimated_cost_usd / f64::from(totals.successes))
    }
}

/// Estimate the USD cost of an attempt.
///
/// Returns `None` when pricing is unknown, so callers cannot accidentally
/// treat a missing price as free.
pub fn estimate_cost(profile: &CostProfile, tokens: TokenUsage) -> Option<f64> {
    profile.estimate(tokens.input, tokens.output).value()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use cuma_core::Known;

    fn record(agent: &str, success: bool, cost: Option<f64>, reported: bool) -> UsageRecord {
        UsageRecord {
            attempt_id: AttemptId::generate(),
            session_id: SessionId::new("s"),
            task_id: TaskId::new("t"),
            task_type: TaskType::Implementation,
            agent_id: AgentId::new(agent),
            model_id: Some(ModelId::new("m")),
            provider: None,
            started_at: chrono::Utc::now(),
            latency_ms: 1000,
            tokens: TokenUsage {
                input: 1000,
                output: 500,
                cached: 0,
                reported,
            },
            estimated_cost_usd: cost,
            success,
            failure_class: if success {
                None
            } else {
                Some(ErrorClass::RateLimit)
            },
            retry_count: 0,
        }
    }

    #[test]
    fn an_unpriced_attempt_is_never_counted_as_free() {
        let mut tracker = UsageTracker::new();
        tracker.record(record("a", true, None, true));

        let totals = tracker.totals();
        assert_eq!(totals.estimated_cost_usd, 0.0);
        assert_eq!(totals.attempts_without_pricing, 1);
        assert!(!totals.is_complete());
        assert_eq!(totals.render_cost(), "unknown");
    }

    #[test]
    fn a_partially_priced_total_is_rendered_as_a_lower_bound() {
        let mut tracker = UsageTracker::new();
        tracker.record(record("a", true, Some(0.10), true));
        tracker.record(record("a", true, None, true));

        let rendered = tracker.totals().render_cost();
        assert!(rendered.starts_with('≥'), "got {rendered}");
        assert!(rendered.contains("1 of 2 attempts unpriced"));
    }

    #[test]
    fn a_fully_priced_total_is_still_marked_an_estimate() {
        let mut tracker = UsageTracker::new();
        tracker.record(record("a", true, Some(0.10), true));

        // Even with exact token counts and exact prices, the total is derived.
        assert_eq!(tracker.totals().render_cost(), "~$0.1000");
    }

    #[test]
    fn estimated_token_counts_mark_a_total_incomplete() {
        let mut tracker = UsageTracker::new();
        tracker.record(record("a", true, Some(0.10), false));

        let totals = tracker.totals();
        assert_eq!(totals.attempts_with_estimated_tokens, 1);
        assert!(!totals.is_complete());
    }

    #[test]
    fn budget_enforcement_uses_only_known_costs() {
        let mut tracker = UsageTracker::new();
        tracker.record(record("a", true, Some(1.50), true));
        tracker.record(record("a", true, None, true));

        assert!((tracker.spent_usd() - 1.50).abs() < 1e-9);
    }

    #[test]
    fn failed_attempts_count_towards_spend() {
        let mut tracker = UsageTracker::new();
        tracker.record(record("a", false, Some(0.25), true));
        tracker.record(record("a", true, Some(0.25), true));

        assert!((tracker.spent_usd() - 0.50).abs() < 1e-9);
        assert_eq!(tracker.totals().attempts, 2);
        assert_eq!(tracker.totals().successes, 1);
    }

    #[test]
    fn usage_groups_by_agent_model_and_task_type() {
        let mut tracker = UsageTracker::new();
        tracker.record(record("claude", true, Some(0.1), true));
        tracker.record(record("claude", true, Some(0.1), true));
        tracker.record(record("codex", false, Some(0.1), true));

        let by_agent = tracker.by_agent();
        assert_eq!(by_agent[&AgentId::new("claude")].attempts, 2);
        assert_eq!(by_agent[&AgentId::new("codex")].successes, 0);
        assert_eq!(tracker.by_model().len(), 2);
        assert_eq!(
            tracker.by_task_type()[&TaskType::Implementation].attempts,
            3
        );
    }

    #[test]
    fn success_rate_and_mean_latency_are_none_before_any_attempt() {
        let totals = UsageTotals::default();
        assert_eq!(totals.success_rate(), None);
        assert_eq!(totals.mean_latency_ms(), None);
        assert_eq!(totals.render_cost(), "-");
    }

    #[test]
    fn the_failure_breakdown_counts_each_class() {
        let mut tracker = UsageTracker::new();
        tracker.record(record("a", false, None, true));
        tracker.record(record("a", false, None, true));
        tracker.record(record("a", true, None, true));

        let breakdown = tracker.failure_breakdown();
        assert_eq!(breakdown["RateLimit"], 2);
        assert_eq!(
            breakdown.values().sum::<u32>(),
            2,
            "successes are not failures"
        );
    }

    #[test]
    fn cost_per_success_is_unavailable_when_nothing_was_priced() {
        let mut tracker = UsageTracker::new();
        tracker.record(record("a", true, None, true));
        assert_eq!(tracker.cost_per_success(), None);
    }

    #[test]
    fn cost_per_success_divides_total_spend_by_successes() {
        let mut tracker = UsageTracker::new();
        tracker.record(record("a", false, Some(1.0), true));
        tracker.record(record("a", true, Some(1.0), true));

        // $2 spent, one success: the failed attempt's cost is attributed too,
        // because it is money that was actually spent getting there.
        assert!((tracker.cost_per_success().unwrap() - 2.0).abs() < 1e-9);
    }

    #[test]
    fn rtk_savings_accumulate_separately_from_spend() {
        let mut tracker = UsageTracker::new();
        tracker.record_rtk_saving(12_000);
        tracker.record_rtk_saving(36_000);
        assert_eq!(tracker.rtk_tokens_saved(), 48_000);
        assert_eq!(tracker.spent_usd(), 0.0);
    }

    #[test]
    fn cost_estimation_returns_none_for_unknown_pricing() {
        assert_eq!(
            estimate_cost(&CostProfile::default(), TokenUsage::reported(1000, 1000)),
            None
        );
    }

    #[test]
    fn cost_estimation_uses_both_prices_when_known() {
        let profile = CostProfile {
            input_per_mtok: Known::Reported(3.0),
            output_per_mtok: Known::Reported(15.0),
            cache_read_per_mtok: Known::Unknown,
        };
        let cost = estimate_cost(&profile, TokenUsage::reported(1_000_000, 1_000_000)).unwrap();
        assert!((cost - 18.0).abs() < 1e-9);
    }
}
