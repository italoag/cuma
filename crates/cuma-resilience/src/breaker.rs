//! Circuit breakers, keyed per agent and per agent+model.
//!
//! An agent that has failed five times in a row is not going to succeed on the
//! sixth attempt just because a new task arrived. The breaker removes it from
//! the routing pool for a cooldown, then lets exactly one probe through to see
//! whether it has recovered.

use cuma_core::{AgentId, ErrorClass, HealthState, ModelId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Breaker state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BreakerState {
    /// Traffic flows normally.
    #[default]
    Closed,
    /// Traffic is blocked; the cooldown is running.
    Open,
    /// One probe is allowed through to test recovery.
    HalfOpen,
}

impl BreakerState {
    /// The health state a router should see for this breaker state.
    pub fn to_health(self) -> HealthState {
        match self {
            Self::Closed => HealthState::Healthy,
            Self::Open => HealthState::Unavailable,
            // Half-open is routable but penalised: we want traffic to prefer a
            // known-good agent while still occasionally testing this one.
            Self::HalfOpen => HealthState::Degraded,
        }
    }
}

/// Breaker tuning.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct BreakerConfig {
    /// Consecutive failures that trip the breaker.
    pub failure_threshold: u32,
    /// Consecutive successes in half-open that close it again.
    pub success_threshold: u32,
    /// How long the breaker stays open before allowing a probe.
    pub cooldown_ms: u64,
}

impl Default for BreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 3,
            success_threshold: 1,
            cooldown_ms: 30_000,
        }
    }
}

/// One breaker.
///
/// Time is injected through the `now` parameters rather than read from the
/// clock internally, so the state machine is testable without sleeping.
#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    config: BreakerConfig,
    state: BreakerState,
    consecutive_failures: u32,
    consecutive_successes: u32,
    opened_at: Option<Instant>,
    last_error: Option<String>,
}

impl CircuitBreaker {
    /// A closed breaker.
    pub fn new(config: BreakerConfig) -> Self {
        Self {
            config,
            state: BreakerState::Closed,
            consecutive_failures: 0,
            consecutive_successes: 0,
            opened_at: None,
            last_error: None,
        }
    }

    /// The state as of `now`, transitioning `Open` to `HalfOpen` if the
    /// cooldown has elapsed.
    pub fn state_at(&mut self, now: Instant) -> BreakerState {
        if self.state == BreakerState::Open
            && let Some(opened_at) = self.opened_at
            && now.duration_since(opened_at) >= Duration::from_millis(self.config.cooldown_ms)
        {
            self.state = BreakerState::HalfOpen;
            self.consecutive_successes = 0;
        }
        self.state
    }

    /// Whether a request may proceed as of `now`.
    pub fn allows_request(&mut self, now: Instant) -> bool {
        self.state_at(now) != BreakerState::Open
    }

    /// Record a success.
    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.last_error = None;

        match self.state {
            BreakerState::HalfOpen => {
                self.consecutive_successes += 1;
                if self.consecutive_successes >= self.config.success_threshold {
                    self.state = BreakerState::Closed;
                    self.opened_at = None;
                    self.consecutive_successes = 0;
                }
            }
            BreakerState::Closed => {}
            // A success while open means a request slipped past the gate.
            // Treat it as evidence of recovery rather than ignoring it.
            BreakerState::Open => {
                self.state = BreakerState::HalfOpen;
                self.consecutive_successes = 1;
            }
        }
    }

    /// Record a failure.
    ///
    /// Classes that say nothing about the agent's health — cancellation, our
    /// own misconfiguration — are ignored, so a user pressing Ctrl-C three
    /// times cannot take a healthy agent out of rotation.
    pub fn record_failure(&mut self, class: ErrorClass, message: impl Into<String>, now: Instant) {
        if !class.counts_against_health() {
            return;
        }

        self.consecutive_failures += 1;
        self.consecutive_successes = 0;
        self.last_error = Some(message.into());

        match self.state {
            // A failed probe re-opens immediately; the agent has not recovered.
            BreakerState::HalfOpen => {
                self.state = BreakerState::Open;
                self.opened_at = Some(now);
            }
            BreakerState::Closed => {
                if self.consecutive_failures >= self.config.failure_threshold {
                    self.state = BreakerState::Open;
                    self.opened_at = Some(now);
                }
            }
            BreakerState::Open => {
                self.opened_at = Some(now);
            }
        }
    }

    /// Consecutive failures since the last success.
    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    /// The most recent failure message, if the breaker is unhappy.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
}

/// The key a breaker is tracked under.
///
/// Both granularities matter: one model being overloaded should not disqualify
/// an agent's other models, but a crashed process should disqualify all of them.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum BreakerKey {
    /// The whole agent.
    Agent(AgentId),
    /// One model on one agent.
    Model(AgentId, ModelId),
}

/// Thread-safe collection of breakers.
#[derive(Debug, Clone, Default)]
pub struct CircuitBreakerRegistry {
    config: BreakerConfig,
    breakers: Arc<Mutex<BTreeMap<BreakerKey, CircuitBreaker>>>,
}

impl CircuitBreakerRegistry {
    /// A registry whose breakers use `config`.
    pub fn new(config: BreakerConfig) -> Self {
        Self {
            config,
            breakers: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Apply `f` to the breaker for `key`, creating it if needed.
    ///
    /// A poisoned mutex is treated as "no breaker information available"
    /// rather than a panic: losing circuit-breaker state degrades routing
    /// quality, but panicking here would take down the whole harness.
    fn with_breaker<T>(
        &self,
        key: BreakerKey,
        f: impl FnOnce(&mut CircuitBreaker) -> T,
    ) -> Option<T> {
        let mut guard = self.breakers.lock().ok()?;
        let breaker = guard
            .entry(key)
            .or_insert_with(|| CircuitBreaker::new(self.config));
        Some(f(breaker))
    }

    /// Whether `agent` (optionally with `model`) may be routed to now.
    pub fn allows(&self, agent: &AgentId, model: Option<&ModelId>) -> bool {
        let now = Instant::now();

        let agent_ok = self
            .with_breaker(BreakerKey::Agent(agent.clone()), |b| b.allows_request(now))
            .unwrap_or(true);
        if !agent_ok {
            return false;
        }

        match model {
            Some(model) => self
                .with_breaker(BreakerKey::Model(agent.clone(), model.clone()), |b| {
                    b.allows_request(now)
                })
                .unwrap_or(true),
            None => true,
        }
    }

    /// The health state the router should assume for `agent`.
    pub fn health(&self, agent: &AgentId) -> HealthState {
        self.with_breaker(BreakerKey::Agent(agent.clone()), |b| {
            b.state_at(Instant::now()).to_health()
        })
        .unwrap_or(HealthState::Unknown)
    }

    /// Record a successful attempt.
    pub fn record_success(&self, agent: &AgentId, model: Option<&ModelId>) {
        self.with_breaker(
            BreakerKey::Agent(agent.clone()),
            CircuitBreaker::record_success,
        );
        if let Some(model) = model {
            self.with_breaker(
                BreakerKey::Model(agent.clone(), model.clone()),
                CircuitBreaker::record_success,
            );
        }
    }

    /// Record a failed attempt.
    ///
    /// Returns the agent breaker's state afterwards, so the caller can publish
    /// a `CircuitBreakerChanged` event without a second lookup.
    pub fn record_failure(
        &self,
        agent: &AgentId,
        model: Option<&ModelId>,
        class: ErrorClass,
        message: &str,
    ) -> BreakerState {
        let now = Instant::now();

        // A model-specific failure (that model is overloaded) should not count
        // against the agent as a whole, or one bad model would disable an
        // otherwise healthy agent.
        if class == ErrorClass::ModelUnavailable
            && let Some(model) = model
        {
            self.with_breaker(BreakerKey::Model(agent.clone(), model.clone()), |b| {
                b.record_failure(class, message, now)
            });
            return self
                .with_breaker(BreakerKey::Agent(agent.clone()), |b| b.state_at(now))
                .unwrap_or_default();
        }

        if let Some(model) = model {
            self.with_breaker(BreakerKey::Model(agent.clone(), model.clone()), |b| {
                b.record_failure(class, message, now)
            });
        }

        self.with_breaker(BreakerKey::Agent(agent.clone()), |b| {
            b.record_failure(class, message, now);
            b.state_at(now)
        })
        .unwrap_or_default()
    }

    /// A snapshot of every breaker's state, for `cuma doctor` and the TUI.
    pub fn snapshot(&self) -> Vec<(String, BreakerState, u32)> {
        let Ok(mut guard) = self.breakers.lock() else {
            return Vec::new();
        };
        let now = Instant::now();
        guard
            .iter_mut()
            .map(|(key, breaker)| {
                let label = match key {
                    BreakerKey::Agent(a) => a.to_string(),
                    BreakerKey::Model(a, m) => format!("{a}/{m}"),
                };
                (label, breaker.state_at(now), breaker.consecutive_failures())
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn config() -> BreakerConfig {
        BreakerConfig {
            failure_threshold: 3,
            success_threshold: 1,
            cooldown_ms: 1000,
        }
    }

    #[test]
    fn a_breaker_starts_closed_and_passes_traffic() {
        let mut b = CircuitBreaker::new(config());
        assert_eq!(b.state_at(Instant::now()), BreakerState::Closed);
        assert!(b.allows_request(Instant::now()));
    }

    #[test]
    fn it_trips_only_after_the_threshold_is_reached() {
        let mut b = CircuitBreaker::new(config());
        let now = Instant::now();

        b.record_failure(ErrorClass::AgentCrash, "boom", now);
        b.record_failure(ErrorClass::AgentCrash, "boom", now);
        assert_eq!(b.state_at(now), BreakerState::Closed, "2 < 3");

        b.record_failure(ErrorClass::AgentCrash, "boom", now);
        assert_eq!(b.state_at(now), BreakerState::Open);
        assert!(!b.allows_request(now));
    }

    #[test]
    fn a_success_resets_the_failure_streak() {
        let mut b = CircuitBreaker::new(config());
        let now = Instant::now();

        b.record_failure(ErrorClass::Timeout, "slow", now);
        b.record_failure(ErrorClass::Timeout, "slow", now);
        b.record_success();
        b.record_failure(ErrorClass::Timeout, "slow", now);

        assert_eq!(b.state_at(now), BreakerState::Closed);
        assert_eq!(b.consecutive_failures(), 1);
    }

    #[test]
    fn the_cooldown_moves_an_open_breaker_to_half_open() {
        let mut b = CircuitBreaker::new(config());
        let opened = Instant::now();

        for _ in 0..3 {
            b.record_failure(ErrorClass::AgentCrash, "boom", opened);
        }
        assert_eq!(b.state_at(opened), BreakerState::Open);

        let later = opened + Duration::from_millis(1001);
        assert_eq!(b.state_at(later), BreakerState::HalfOpen);
        assert!(b.allows_request(later), "one probe gets through");
    }

    #[test]
    fn a_successful_probe_closes_the_breaker() {
        let mut b = CircuitBreaker::new(config());
        let opened = Instant::now();
        for _ in 0..3 {
            b.record_failure(ErrorClass::AgentCrash, "boom", opened);
        }

        let later = opened + Duration::from_millis(1001);
        assert_eq!(b.state_at(later), BreakerState::HalfOpen);

        b.record_success();
        assert_eq!(b.state_at(later), BreakerState::Closed);
    }

    #[test]
    fn a_failed_probe_reopens_immediately_without_a_new_streak() {
        let mut b = CircuitBreaker::new(config());
        let opened = Instant::now();
        for _ in 0..3 {
            b.record_failure(ErrorClass::AgentCrash, "boom", opened);
        }

        let later = opened + Duration::from_millis(1001);
        assert_eq!(b.state_at(later), BreakerState::HalfOpen);

        b.record_failure(ErrorClass::AgentCrash, "still broken", later);
        assert_eq!(b.state_at(later), BreakerState::Open);
    }

    #[test]
    fn cancellations_never_trip_a_breaker() {
        let mut b = CircuitBreaker::new(config());
        let now = Instant::now();

        for _ in 0..20 {
            b.record_failure(ErrorClass::Cancelled, "user hit ctrl-c", now);
        }

        assert_eq!(b.state_at(now), BreakerState::Closed);
        assert_eq!(b.consecutive_failures(), 0);
    }

    #[test]
    fn an_open_breaker_removes_an_agent_from_routing() {
        let registry = CircuitBreakerRegistry::new(config());
        let agent = AgentId::new("flaky");

        assert!(registry.allows(&agent, None));

        for _ in 0..3 {
            registry.record_failure(&agent, None, ErrorClass::AgentCrash, "boom");
        }

        assert!(!registry.allows(&agent, None));
        assert_eq!(registry.health(&agent), HealthState::Unavailable);
    }

    #[test]
    fn one_bad_model_does_not_disable_its_whole_agent() {
        let registry = CircuitBreakerRegistry::new(config());
        let agent = AgentId::new("multi");
        let bad = ModelId::new("overloaded");
        let good = ModelId::new("fine");

        for _ in 0..3 {
            registry.record_failure(
                &agent,
                Some(&bad),
                ErrorClass::ModelUnavailable,
                "overloaded",
            );
        }

        assert!(!registry.allows(&agent, Some(&bad)), "the bad model is out");
        assert!(registry.allows(&agent, Some(&good)), "its sibling is not");
        assert_eq!(registry.health(&agent), HealthState::Healthy);
    }

    #[test]
    fn a_crash_disables_every_model_on_the_agent() {
        let registry = CircuitBreakerRegistry::new(config());
        let agent = AgentId::new("dead");
        let model = ModelId::new("m1");

        for _ in 0..3 {
            registry.record_failure(&agent, Some(&model), ErrorClass::AgentCrash, "exited 139");
        }

        assert!(!registry.allows(&agent, Some(&ModelId::new("m2"))));
    }

    #[test]
    fn the_snapshot_reports_every_tracked_breaker() {
        let registry = CircuitBreakerRegistry::new(config());
        registry.record_failure(&AgentId::new("a"), None, ErrorClass::Timeout, "slow");
        registry.record_success(&AgentId::new("b"), None);

        let snapshot = registry.snapshot();
        assert_eq!(snapshot.len(), 2);
    }

    #[test]
    fn an_unseen_agent_has_unknown_health_rather_than_being_excluded() {
        let registry = CircuitBreakerRegistry::new(config());
        let agent = AgentId::new("never-used");
        assert!(registry.allows(&agent, None));
        assert!(registry.health(&agent).is_routable());
    }
}
