//! Bounded retry with exponential backoff and jitter.

use cuma_core::ErrorClass;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Exponential backoff parameters.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Backoff {
    /// Delay before the first retry.
    pub base_ms: u64,
    /// Multiplier applied per attempt.
    pub multiplier: f64,
    /// Ceiling, so a long-lived session cannot back off for hours.
    pub max_ms: u64,
    /// Fraction of the delay randomized, in `[0.0, 1.0]`.
    ///
    /// Without jitter, every task that hit the same rate limit retries at the
    /// same instant and hits it again — a thundering herd of our own making.
    pub jitter: f64,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            base_ms: 500,
            multiplier: 2.0,
            max_ms: 60_000,
            jitter: 0.25,
        }
    }
}

impl Backoff {
    /// The delay before attempt number `attempt` (1-based), without jitter.
    pub fn delay_for(&self, attempt: u32) -> Duration {
        let exponent = attempt.saturating_sub(1).min(32);
        #[allow(clippy::cast_precision_loss)]
        let raw = self.base_ms as f64 * self.multiplier.powi(exponent as i32);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let clamped = raw.min(self.max_ms as f64).max(0.0) as u64;
        Duration::from_millis(clamped)
    }

    /// The delay before attempt `attempt`, with jitter applied.
    ///
    /// Jitter is symmetric around the nominal delay, so the expected wait is
    /// unchanged while the variance spreads retries out.
    pub fn delay_with_jitter(&self, attempt: u32, rng: &mut impl rand::Rng) -> Duration {
        let base = self.delay_for(attempt);
        if self.jitter <= 0.0 {
            return base;
        }

        let jitter = self.jitter.clamp(0.0, 1.0);
        #[allow(clippy::cast_precision_loss)]
        let base_ms = base.as_millis() as f64;
        let spread = base_ms * jitter;
        let offset = rng.random_range(-spread..=spread);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let jittered = (base_ms + offset).max(0.0) as u64;
        Duration::from_millis(jittered)
    }
}

/// What the resilience layer decided to do about a failure.
///
/// This is an enum rather than a `bool` because "retry" and "reroute" are
/// genuinely different actions with different costs, and collapsing them is
/// how a harness ends up hammering a dead agent.
#[derive(Debug, Clone, PartialEq)]
pub enum RetryDecision {
    /// Wait, then retry the same agent and model.
    RetrySameTarget {
        /// How long to wait.
        delay: Duration,
        /// Which attempt this will be.
        attempt: u32,
        /// Why, for the event log.
        reason: String,
    },
    /// Route to a different agent or model, carrying a handoff.
    Reroute {
        /// Why the previous target was abandoned.
        reason: String,
    },
    /// Ask the planner for a different plan.
    Replan {
        /// Why the current plan cannot succeed.
        reason: String,
    },
    /// Stop. The task has failed.
    GiveUp {
        /// Why no further attempt will be made.
        reason: String,
    },
}

impl RetryDecision {
    /// Whether this decision keeps the task alive.
    pub fn continues(&self) -> bool {
        !matches!(self, Self::GiveUp { .. })
    }

    /// The reason, whatever the variant.
    pub fn reason(&self) -> &str {
        match self {
            Self::RetrySameTarget { reason, .. }
            | Self::Reroute { reason }
            | Self::Replan { reason }
            | Self::GiveUp { reason } => reason,
        }
    }
}

/// How failures translate into actions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// Total attempts allowed per task, across every agent.
    ///
    /// This is the hard bound that makes infinite retry impossible.
    pub max_attempts: u32,
    /// Attempts allowed against a single agent+model before rerouting.
    pub max_attempts_per_target: u32,
    /// Backoff parameters.
    pub backoff: Backoff,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            max_attempts_per_target: 2,
            backoff: Backoff::default(),
        }
    }
}

impl RetryPolicy {
    /// A policy allowing `max_attempts` total attempts per task.
    pub fn with_max_attempts(max_attempts: u32) -> Self {
        Self {
            max_attempts: max_attempts.max(1),
            ..Self::default()
        }
    }

    /// Decide what to do about a failure.
    ///
    /// `attempts_so_far` counts every attempt on this task including the one
    /// that just failed; `attempts_on_target` counts only those against the
    /// agent+model that just failed. `alternatives_available` says whether the
    /// router has anything else to offer — without it the policy would suggest
    /// rerouting into a void.
    pub fn decide(
        &self,
        class: ErrorClass,
        attempts_so_far: u32,
        attempts_on_target: u32,
        alternatives_available: bool,
        rng: &mut impl rand::Rng,
    ) -> RetryDecision {
        // Cancellation is a decision, not a failure. It is never retried.
        if class == ErrorClass::Cancelled {
            return RetryDecision::GiveUp {
                reason: "task was cancelled".to_owned(),
            };
        }

        // Terminal classes: no amount of retrying or rerouting helps, and
        // pretending otherwise burns tokens and hides the real problem.
        if matches!(
            class,
            ErrorClass::AuthenticationFailure
                | ErrorClass::SecurityViolation
                | ErrorClass::Configuration
        ) {
            return RetryDecision::GiveUp {
                reason: format!("{class:?} is not recoverable by retrying"),
            };
        }

        // The hard bound. Checked before anything else that could continue.
        if attempts_so_far >= self.max_attempts {
            return RetryDecision::GiveUp {
                reason: format!(
                    "exhausted the retry budget ({} attempts)",
                    self.max_attempts
                ),
            };
        }

        // Context overflow means the prompt was wrong, not the agent. Trying a
        // different agent with the same oversized prompt fails identically.
        if class.requires_replan() {
            return RetryDecision::Replan {
                reason: format!("{class:?} requires a smaller or different plan"),
            };
        }

        let target_budget_left = attempts_on_target < self.max_attempts_per_target;

        if class.is_retryable_on_same_target() && target_budget_left {
            let next_attempt = attempts_so_far + 1;
            return RetryDecision::RetrySameTarget {
                delay: self.backoff.delay_with_jitter(next_attempt, rng),
                attempt: next_attempt,
                reason: format!("{class:?} is transient; backing off"),
            };
        }

        if class.is_reroutable() && alternatives_available {
            return RetryDecision::Reroute {
                reason: if target_budget_left {
                    format!("{class:?} will not clear on this agent")
                } else {
                    format!(
                        "{class:?} persisted for {attempts_on_target} attempts on this agent"
                    )
                },
            };
        }

        RetryDecision::GiveUp {
            reason: if class.is_reroutable() {
                format!("{class:?} needs a different agent, but no alternative is available")
            } else {
                format!("{class:?} is not retryable and not reroutable")
            },
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    fn rng() -> StdRng {
        StdRng::seed_from_u64(42)
    }

    #[test]
    fn backoff_grows_exponentially_and_then_stops() {
        let b = Backoff {
            base_ms: 100,
            multiplier: 2.0,
            max_ms: 1000,
            jitter: 0.0,
        };
        assert_eq!(b.delay_for(1).as_millis(), 100);
        assert_eq!(b.delay_for(2).as_millis(), 200);
        assert_eq!(b.delay_for(3).as_millis(), 400);
        assert_eq!(b.delay_for(4).as_millis(), 800);
        assert_eq!(b.delay_for(5).as_millis(), 1000, "capped");
        assert_eq!(b.delay_for(99).as_millis(), 1000, "still capped");
    }

    #[test]
    fn jitter_spreads_retries_without_shifting_the_average() {
        let b = Backoff {
            base_ms: 1000,
            multiplier: 1.0,
            max_ms: 10_000,
            jitter: 0.5,
        };
        let mut r = rng();

        let samples: Vec<u128> = (0..200)
            .map(|_| b.delay_with_jitter(1, &mut r).as_millis())
            .collect();

        assert!(samples.iter().all(|&d| (500..=1500).contains(&d)));
        assert!(
            samples.iter().collect::<std::collections::BTreeSet<_>>().len() > 50,
            "jitter must actually vary"
        );

        let mean = samples.iter().sum::<u128>() / samples.len() as u128;
        assert!((900..=1100).contains(&mean), "mean drifted to {mean}");
    }

    #[test]
    fn zero_jitter_is_deterministic() {
        let b = Backoff {
            jitter: 0.0,
            ..Backoff::default()
        };
        let mut r = rng();
        assert_eq!(b.delay_with_jitter(3, &mut r), b.delay_for(3));
    }

    #[test]
    fn a_rate_limit_retries_the_same_agent_first() {
        let policy = RetryPolicy::default();
        let decision = policy.decide(ErrorClass::RateLimit, 1, 1, true, &mut rng());
        assert!(matches!(decision, RetryDecision::RetrySameTarget { .. }));
    }

    #[test]
    fn a_rate_limit_that_persists_reroutes_to_another_agent() {
        let policy = RetryPolicy::default();
        // Two attempts already spent on this target: its per-target budget is gone.
        let decision = policy.decide(ErrorClass::RateLimit, 2, 2, true, &mut rng());
        assert!(matches!(decision, RetryDecision::Reroute { .. }));
    }

    #[test]
    fn a_crash_reroutes_immediately_without_retrying_the_dead_agent() {
        let policy = RetryPolicy::default();
        let decision = policy.decide(ErrorClass::AgentCrash, 1, 1, true, &mut rng());
        assert!(
            matches!(decision, RetryDecision::Reroute { .. }),
            "retrying a crashed process wastes a whole attempt"
        );
    }

    #[test]
    fn auth_failures_give_up_at_once() {
        let policy = RetryPolicy::default();
        let decision = policy.decide(ErrorClass::AuthenticationFailure, 1, 1, true, &mut rng());
        assert!(matches!(decision, RetryDecision::GiveUp { .. }));
        assert!(!decision.continues());
    }

    #[test]
    fn security_violations_are_never_retried_however_much_budget_remains() {
        let policy = RetryPolicy::with_max_attempts(100);
        let decision = policy.decide(ErrorClass::SecurityViolation, 0, 0, true, &mut rng());
        assert!(matches!(decision, RetryDecision::GiveUp { .. }));
    }

    #[test]
    fn context_overflow_asks_for_a_replan_not_another_agent() {
        let policy = RetryPolicy::default();
        let decision = policy.decide(ErrorClass::ContextOverflow, 1, 1, true, &mut rng());
        assert!(matches!(decision, RetryDecision::Replan { .. }));
    }

    #[test]
    fn the_retry_budget_is_a_hard_bound() {
        let policy = RetryPolicy::with_max_attempts(3);
        // A rate limit is the most retryable class there is; the budget still wins.
        let decision = policy.decide(ErrorClass::RateLimit, 3, 0, true, &mut rng());
        assert!(matches!(decision, RetryDecision::GiveUp { .. }));
        assert!(decision.reason().contains("budget"));
    }

    #[test]
    fn rerouting_is_not_suggested_when_there_is_nowhere_to_go() {
        let policy = RetryPolicy::default();
        let decision = policy.decide(ErrorClass::AgentCrash, 1, 1, false, &mut rng());
        assert!(matches!(decision, RetryDecision::GiveUp { .. }));
        assert!(decision.reason().contains("no alternative"));
    }

    #[test]
    fn cancellation_is_never_treated_as_a_failure_to_retry() {
        let policy = RetryPolicy::with_max_attempts(100);
        let decision = policy.decide(ErrorClass::Cancelled, 0, 0, true, &mut rng());
        assert!(matches!(decision, RetryDecision::GiveUp { .. }));
        assert!(decision.reason().contains("cancelled"));
    }

    /// The property that matters most: whatever the class, whatever the
    /// counters, the loop terminates.
    #[test]
    fn every_failure_sequence_terminates() {
        let policy = RetryPolicy::default();
        let mut r = rng();

        for class in [
            ErrorClass::RateLimit,
            ErrorClass::Timeout,
            ErrorClass::AgentCrash,
            ErrorClass::ConnectionFailure,
            ErrorClass::Unknown,
            ErrorClass::ToolFailure,
            ErrorClass::InvalidResponse,
            ErrorClass::ModelUnavailable,
            ErrorClass::QuotaExceeded,
        ] {
            let mut attempts = 0u32;
            let mut on_target = 0u32;
            let mut steps = 0;

            loop {
                steps += 1;
                assert!(steps < 100, "{class:?} did not terminate");

                attempts += 1;
                on_target += 1;

                match policy.decide(class, attempts, on_target, true, &mut r) {
                    RetryDecision::RetrySameTarget { .. } => {}
                    RetryDecision::Reroute { .. } => on_target = 0,
                    RetryDecision::Replan { .. } | RetryDecision::GiveUp { .. } => break,
                }
            }

            assert!(
                attempts <= policy.max_attempts,
                "{class:?} exceeded the budget with {attempts} attempts"
            );
        }
    }
}
