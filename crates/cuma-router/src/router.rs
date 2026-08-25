//! The router: filter, score, explain.

use crate::adaptive::RoutingHistory;
use crate::explain::{Candidate, Rejection, RoutingDecision, ScoreBreakdown};
use crate::score;
use cuma_config::{RouterConfig, RoutingStrategy};
use cuma_core::error::{MetaAgentError, Result};
use cuma_core::{AgentDescriptor, AgentId, AgentProtocol, ModelDescriptor, ModelId, Task};
use cuma_registry::RegistrySnapshot;
use cuma_resilience::CircuitBreakerRegistry;

/// How many runners-up an explanation carries.
///
/// Enough to see the shape of the decision; few enough that the explanation
/// stays readable in a terminal.
const MAX_ALTERNATIVES: usize = 5;

/// One routing request.
#[derive(Debug, Clone)]
pub struct RouteRequest<'a> {
    /// The task to route.
    pub task: &'a Task,
    /// Agents to consider.
    pub snapshot: &'a RegistrySnapshot,
    /// Targets that already failed this task and must not be re-selected.
    ///
    /// Without this, a fallback would happily hand the task straight back to
    /// the agent that just failed it.
    pub exclude_targets: &'a [(AgentId, Option<ModelId>)],
    /// USD already spent this session, for budget filtering.
    pub spent_usd: f64,
    /// Session budget ceiling, when one is set.
    pub budget_usd: Option<f64>,
}

impl<'a> RouteRequest<'a> {
    /// A request with no exclusions and no budget.
    pub fn new(task: &'a Task, snapshot: &'a RegistrySnapshot) -> Self {
        Self {
            task,
            snapshot,
            exclude_targets: &[],
            spent_usd: 0.0,
            budget_usd: None,
        }
    }

    /// Exclude targets that already failed.
    #[must_use]
    pub fn excluding(mut self, targets: &'a [(AgentId, Option<ModelId>)]) -> Self {
        self.exclude_targets = targets;
        self
    }

    /// Apply a session budget.
    #[must_use]
    pub fn with_budget(mut self, spent_usd: f64, budget_usd: Option<f64>) -> Self {
        self.spent_usd = spent_usd;
        self.budget_usd = budget_usd;
        self
    }
}

/// Selects an agent and model for a task.
pub struct Router {
    config: RouterConfig,
    breakers: CircuitBreakerRegistry,
    history: RoutingHistory,
}

impl Router {
    /// A router using `config`, with no breaker state and no history.
    pub fn new(config: RouterConfig) -> Self {
        Self {
            config,
            breakers: CircuitBreakerRegistry::default(),
            history: RoutingHistory::new(),
        }
    }

    /// Attach a shared circuit-breaker registry.
    #[must_use]
    pub fn with_breakers(mut self, breakers: CircuitBreakerRegistry) -> Self {
        self.breakers = breakers;
        self
    }

    /// Attach observed routing history.
    #[must_use]
    pub fn with_history(mut self, history: RoutingHistory) -> Self {
        self.history = history;
        self
    }

    /// Mutable access to the history, so the orchestrator can record outcomes.
    pub fn history_mut(&mut self) -> &mut RoutingHistory {
        &mut self.history
    }

    /// Read-only access to the history.
    pub fn history(&self) -> &RoutingHistory {
        &self.history
    }

    /// The effective weights: an explicit block, or the strategy's preset.
    fn weights(&self) -> cuma_config::RouterWeights {
        self.config.weights.normalized()
    }

    /// Choose an agent and model.
    ///
    /// Returns [`MetaAgentError::Routing`] when nothing survives filtering,
    /// with the rejection reasons in the message — "no agent available" with
    /// no explanation is an unactionable error.
    pub fn route(&self, request: &RouteRequest<'_>) -> Result<RoutingDecision> {
        let mut candidates = Vec::new();
        let mut rejected = Vec::new();

        for agent in request.snapshot.all() {
            self.consider_agent(agent, request, &mut candidates, &mut rejected);
        }

        candidates.sort_by(|a, b| {
            b.breakdown
                .total
                .partial_cmp(&a.breakdown.total)
                // NaN cannot arise from clamped scores, but ordering must be
                // total regardless: an inconsistent comparator is UB-adjacent.
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        if candidates.is_empty() {
            let detail = if rejected.is_empty() {
                "no agents are registered".to_owned()
            } else {
                rejected
                    .iter()
                    .map(|r: &Rejection| format!("{}: {}", r.agent_id, r.reason))
                    .collect::<Vec<_>>()
                    .join("; ")
            };

            return Err(MetaAgentError::Routing {
                task: request.task.id.clone(),
                reason: detail,
            });
        }

        let selected = candidates.remove(0);
        candidates.truncate(MAX_ALTERNATIVES);

        Ok(RoutingDecision {
            selected,
            alternatives: candidates,
            rejected,
            weights: self.weights(),
            strategy: self.config.strategy,
        })
    }

    /// Filter and score one agent's candidates.
    fn consider_agent(
        &self,
        agent: &AgentDescriptor,
        request: &RouteRequest<'_>,
        candidates: &mut Vec<Candidate>,
        rejected: &mut Vec<Rejection>,
    ) {
        let reject = |reason: String, rejected: &mut Vec<Rejection>| {
            rejected.push(Rejection {
                agent_id: agent.id.clone(),
                model_id: None,
                reason,
            });
        };

        if !agent.enabled {
            reject("disabled by configuration".to_owned(), rejected);
            return;
        }

        if self.config.exclude_agents.iter().any(|e| e == agent.id.as_str()) {
            reject("excluded by router.exclude_agents".to_owned(), rejected);
            return;
        }

        // A pin is a hard constraint, not a preference: `manual` strategy or a
        // pinned agent means the operator has already made the decision.
        if let Some(pinned) = &self.config.pin_agent
            && pinned != agent.id.as_str()
        {
            reject(format!("router.pin_agent is {pinned}"), rejected);
            return;
        }

        if !agent.health.state.is_routable() {
            reject(
                format!("health is {:?}", agent.health.state),
                rejected,
            );
            return;
        }

        if !self.breakers.allows(&agent.id, None) {
            reject("circuit breaker open".to_owned(), rejected);
            return;
        }

        // `local-first` and `privacy-first` are expressed as hard filters:
        // a remote agent is not "slightly worse" under a privacy policy, it is
        // disqualified.
        if matches!(
            self.config.strategy,
            RoutingStrategy::LocalFirst | RoutingStrategy::PrivacyFirst
        ) && agent.protocol == AgentProtocol::A2A
        {
            reject(
                format!("{:?} excludes remote A2A agents", self.config.strategy),
                rejected,
            );
            return;
        }

        if agent.models.is_empty() {
            self.consider_target(agent, None, request, candidates, rejected);
        } else {
            for model in &agent.models {
                self.consider_target(agent, Some(model), request, candidates, rejected);
            }
        }
    }

    /// Filter and score one agent+model pair.
    fn consider_target(
        &self,
        agent: &AgentDescriptor,
        model: Option<&ModelDescriptor>,
        request: &RouteRequest<'_>,
        candidates: &mut Vec<Candidate>,
        rejected: &mut Vec<Rejection>,
    ) {
        let model_id = model.map(|m| m.id.clone());

        let reject = |reason: String, rejected: &mut Vec<Rejection>| {
            rejected.push(Rejection {
                agent_id: agent.id.clone(),
                model_id: model_id.clone(),
                reason,
            });
        };

        if let Some(model) = model {
            if !model.available {
                reject("model unavailable".to_owned(), rejected);
                return;
            }

            if self
                .config
                .exclude_models
                .iter()
                .any(|e| e == model.id.as_str())
            {
                reject("excluded by router.exclude_models".to_owned(), rejected);
                return;
            }

            if let Some(pinned) = &self.config.pin_model
                && pinned != model.id.as_str()
            {
                reject(format!("router.pin_model is {pinned}"), rejected);
                return;
            }

            if !self.breakers.allows(&agent.id, Some(&model.id)) {
                reject("model circuit breaker open".to_owned(), rejected);
                return;
            }
        }

        // Never hand a task back to a target that just failed it.
        if request
            .exclude_targets
            .iter()
            .any(|(a, m)| a == &agent.id && (m.is_none() || m == &model_id))
        {
            reject("already failed this task".to_owned(), rejected);
            return;
        }

        // A candidate that cannot do the work at all is filtered, not merely
        // scored low: a cheap, fast, reliable agent that lacks the required
        // capability must never win on the strength of the other dimensions.
        let coverage = agent
            .effective_capabilities(model)
            .match_against(&request.task.spec.required_capabilities);
        if !coverage.is_complete() {
            let missing: Vec<String> = coverage.missing.iter().map(ToString::to_string).collect();
            reject(
                format!("missing required capabilities: {}", missing.join(", ")),
                rejected,
            );
            return;
        }

        if let Some(budget) = request.budget_usd {
            let remaining = budget - request.spent_usd;
            if remaining <= 0.0 {
                reject(
                    format!("session budget of ${budget:.2} is exhausted"),
                    rejected,
                );
                return;
            }

            if let Some(estimated) = self.estimate_cost(agent, model, request.task)
                && estimated > remaining
            {
                reject(
                    format!(
                        "estimated ${estimated:.4} exceeds ${remaining:.4} remaining budget"
                    ),
                    rejected,
                );
                return;
            }
        }

        let (dimensions, mut notes) = score::all(agent, model, request.task);

        // Fold observed history into the reliability dimension.
        let (reliability, adapted) = self.history.blend(
            dimensions.reliability,
            &agent.id,
            model_id.as_ref(),
            request.task.spec.task_type,
            self.config.adaptive_weight,
            self.config.adaptive_min_samples,
        );
        if adapted {
            notes.push(format!(
                "reliability adjusted from observed history for {:?}",
                request.task.spec.task_type
            ));
        }

        let w = self.weights();
        let total = dimensions.quality * w.quality
            + dimensions.cost * w.cost
            + dimensions.latency * w.latency
            + reliability * w.reliability
            + dimensions.context * w.context;

        candidates.push(Candidate {
            agent_id: agent.id.clone(),
            model_id,
            breakdown: ScoreBreakdown {
                quality: dimensions.quality,
                cost: dimensions.cost,
                latency: dimensions.latency,
                reliability,
                context: dimensions.context,
                total,
            },
            notes,
        });
    }

    /// Estimate what one attempt would cost, when there is enough information.
    fn estimate_cost(
        &self,
        agent: &AgentDescriptor,
        model: Option<&ModelDescriptor>,
        task: &Task,
    ) -> Option<f64> {
        let estimated_tokens = task.spec.estimated_tokens?;
        let profile = model.map(|m| &m.cost).unwrap_or(&agent.cost_profile);

        // Assume a 70/30 input/output split, typical for a coding turn where
        // the prompt carries files and the reply carries a diff.
        let input = estimated_tokens * 7 / 10;
        let output = estimated_tokens - input;
        profile.estimate(input, output).value()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::adaptive::OutcomeRecord;
    use cuma_config::{RouterWeights, RoutingStrategy};
    use cuma_core::{
        CapabilitySet, CostProfile, ErrorClass, HealthState, Known, TaskSpec, TaskType,
    };

    /// A broad capability set, so tests can vary the task type without every
    /// fixture agent silently failing the capability filter.
    fn coding_caps() -> CapabilitySet {
        [
            TaskType::Implementation,
            TaskType::Refactor,
            TaskType::Design,
            TaskType::BugFix,
            TaskType::Testing,
        ]
        .into_iter()
        .flat_map(|t| t.baseline_capabilities().iter().cloned().collect::<Vec<_>>())
        .collect()
    }

    fn agent(id: &str, protocol: AgentProtocol) -> AgentDescriptor {
        AgentDescriptor::new(id, id, protocol).with_capabilities(coding_caps())
    }

    fn priced_model(agent: &AgentDescriptor, id: &str, input: f64, output: f64) -> ModelDescriptor {
        let mut model = ModelDescriptor::minimal(agent.id.clone(), id, id);
        model.cost = CostProfile {
            input_per_mtok: Known::Reported(input),
            output_per_mtok: Known::Reported(output),
            cache_read_per_mtok: Known::Unknown,
        };
        model.context_window = Known::Reported(200_000);
        model
    }

    fn task() -> Task {
        Task::new(TaskSpec::new("write the thing", TaskType::Implementation))
    }

    fn config(strategy: RoutingStrategy) -> RouterConfig {
        RouterConfig {
            strategy,
            weights: strategy.default_weights(),
            ..RouterConfig::default()
        }
    }

    // --- capability filtering -------------------------------------------

    #[test]
    fn an_agent_lacking_a_required_capability_is_never_selected() {
        // The incapable agent is otherwise perfect: free, instant, flawless.
        let mut incapable = AgentDescriptor::new("cheap", "cheap", AgentProtocol::Native);
        incapable.cost_profile = CostProfile {
            input_per_mtok: Known::Reported(0.0),
            output_per_mtok: Known::Reported(0.0),
            cache_read_per_mtok: Known::Unknown,
        };
        incapable.performance_profile.typical_latency_ms = Known::Reported(1);
        incapable.performance_profile.historical_success_rate = Known::Reported(1.0);

        let capable = agent("capable", AgentProtocol::Acp);
        let snapshot = RegistrySnapshot::new(vec![incapable, capable]);

        let router = Router::new(config(RoutingStrategy::Balanced));
        let t = task();
        let decision = router.route(&RouteRequest::new(&t, &snapshot)).unwrap();

        assert_eq!(decision.selected.agent_id, AgentId::new("capable"));
        assert!(
            decision.rejected.iter().any(|r| r.reason.contains("missing required")),
            "the incapable agent must be filtered, not just outscored"
        );
    }

    #[test]
    fn routing_fails_with_an_explanation_when_nothing_qualifies() {
        let snapshot = RegistrySnapshot::new(vec![AgentDescriptor::new(
            "useless",
            "useless",
            AgentProtocol::Native,
        )]);

        let router = Router::new(config(RoutingStrategy::Balanced));
        let t = task();
        let err = router.route(&RouteRequest::new(&t, &snapshot)).unwrap_err();

        assert!(err.to_string().contains("missing required"), "got: {err}");
    }

    // --- cost and quality strategies -------------------------------------

    #[test]
    fn a_cheap_task_goes_to_a_cheap_capable_agent_under_cost_first() {
        let mut cheap = agent("cheap", AgentProtocol::Acp);
        cheap.models.push(priced_model(&cheap, "small", 0.25, 1.25));

        let mut premium = agent("premium", AgentProtocol::Acp);
        premium.models.push(priced_model(&premium, "big", 15.0, 75.0));

        let snapshot = RegistrySnapshot::new(vec![cheap, premium]);
        let router = Router::new(config(RoutingStrategy::CostFirst));

        let t = Task::new(TaskSpec::new("rename a variable", TaskType::Refactor).with_complexity(0.1));
        let decision = router.route(&RouteRequest::new(&t, &snapshot)).unwrap();

        assert_eq!(decision.selected.agent_id, AgentId::new("cheap"));
        assert!(decision.has_alternatives(), "premium is still a fallback");
    }

    #[test]
    fn a_complex_task_goes_to_the_stronger_agent_under_quality_first() {
        let mut weak = agent("weak", AgentProtocol::Acp);
        let mut weak_model = priced_model(&weak, "small", 0.25, 1.25);
        weak_model.performance.coding_score = Known::Reported(0.4);
        weak_model.performance.reasoning_score = Known::Reported(0.3);
        weak.models.push(weak_model);

        let mut strong = agent("strong", AgentProtocol::Acp);
        let mut strong_model = priced_model(&strong, "big", 15.0, 75.0);
        strong_model.performance.coding_score = Known::Reported(0.95);
        strong_model.performance.reasoning_score = Known::Reported(0.97);
        strong.models.push(strong_model);

        let snapshot = RegistrySnapshot::new(vec![weak, strong]);
        let router = Router::new(config(RoutingStrategy::QualityFirst));

        let t = Task::new(
            TaskSpec::new("redesign the auth layer", TaskType::Design).with_complexity(0.95),
        );
        let decision = router.route(&RouteRequest::new(&t, &snapshot)).unwrap();

        assert_eq!(decision.selected.agent_id, AgentId::new("strong"));
    }

    #[test]
    fn the_same_candidates_route_differently_under_different_strategies() {
        let build = || {
            let mut cheap = agent("cheap", AgentProtocol::Acp);
            let mut cheap_model = priced_model(&cheap, "small", 0.25, 1.25);
            cheap_model.performance.coding_score = Known::Reported(0.5);
            cheap_model.performance.reasoning_score = Known::Reported(0.5);
            cheap.models.push(cheap_model);

            let mut premium = agent("premium", AgentProtocol::Acp);
            let mut premium_model = priced_model(&premium, "big", 15.0, 75.0);
            premium_model.performance.coding_score = Known::Reported(0.98);
            premium_model.performance.reasoning_score = Known::Reported(0.98);
            premium.models.push(premium_model);

            RegistrySnapshot::new(vec![cheap, premium])
        };

        let t = task();
        let snapshot = build();

        let cost_pick = Router::new(config(RoutingStrategy::CostFirst))
            .route(&RouteRequest::new(&t, &snapshot))
            .unwrap()
            .selected
            .agent_id;
        let quality_pick = Router::new(config(RoutingStrategy::QualityFirst))
            .route(&RouteRequest::new(&t, &snapshot))
            .unwrap()
            .selected
            .agent_id;

        assert_eq!(cost_pick, AgentId::new("cheap"));
        assert_eq!(quality_pick, AgentId::new("premium"));
    }

    // --- health and breakers ---------------------------------------------

    #[test]
    fn an_unhealthy_agent_is_excluded_from_routing() {
        let mut sick = agent("sick", AgentProtocol::Acp);
        sick.health.state = HealthState::Unavailable;
        let healthy = agent("healthy", AgentProtocol::Acp);

        let snapshot = RegistrySnapshot::new(vec![sick, healthy]);
        let router = Router::new(config(RoutingStrategy::Balanced));
        let t = task();

        let decision = router.route(&RouteRequest::new(&t, &snapshot)).unwrap();
        assert_eq!(decision.selected.agent_id, AgentId::new("healthy"));
        assert!(!decision.has_alternatives());
    }

    #[test]
    fn an_open_circuit_breaker_removes_an_agent_from_the_pool() {
        let breakers = CircuitBreakerRegistry::default();
        let tripped = AgentId::new("tripped");
        for _ in 0..10 {
            breakers.record_failure(&tripped, None, ErrorClass::AgentCrash, "boom");
        }

        let snapshot = RegistrySnapshot::new(vec![
            agent("tripped", AgentProtocol::Acp),
            agent("fine", AgentProtocol::Acp),
        ]);

        let router = Router::new(config(RoutingStrategy::Balanced)).with_breakers(breakers);
        let t = task();
        let decision = router.route(&RouteRequest::new(&t, &snapshot)).unwrap();

        assert_eq!(decision.selected.agent_id, AgentId::new("fine"));
        assert!(
            decision
                .rejected
                .iter()
                .any(|r| r.reason.contains("circuit breaker"))
        );
    }

    // --- fallback ---------------------------------------------------------

    #[test]
    fn a_rate_limited_agent_falls_back_to_the_next_best() {
        let snapshot = RegistrySnapshot::new(vec![
            agent("first-choice", AgentProtocol::Acp),
            agent("backup", AgentProtocol::Acp),
        ]);

        let router = Router::new(config(RoutingStrategy::Balanced));
        let t = task();

        let first = router.route(&RouteRequest::new(&t, &snapshot)).unwrap();
        let failed = [(first.selected.agent_id.clone(), first.selected.model_id.clone())];

        let second = router
            .route(&RouteRequest::new(&t, &snapshot).excluding(&failed))
            .unwrap();

        assert_ne!(second.selected.agent_id, first.selected.agent_id);
        assert!(
            second.rejected.iter().any(|r| r.reason.contains("already failed")),
            "the failed target must be filtered, with a reason"
        );
    }

    #[test]
    fn exhausting_every_agent_produces_a_routing_error_not_a_repeat() {
        let snapshot = RegistrySnapshot::new(vec![agent("only", AgentProtocol::Acp)]);
        let router = Router::new(config(RoutingStrategy::Balanced));
        let t = task();

        let failed = [(AgentId::new("only"), None)];
        let err = router
            .route(&RouteRequest::new(&t, &snapshot).excluding(&failed))
            .unwrap_err();

        assert!(err.to_string().contains("already failed"));
    }

    // --- pins and exclusions ---------------------------------------------

    #[test]
    fn a_pinned_agent_wins_even_when_it_scores_worse() {
        let mut good = agent("good", AgentProtocol::Acp);
        good.performance_profile.historical_success_rate = Known::Reported(1.0);
        let mut bad = agent("pinned", AgentProtocol::Acp);
        bad.performance_profile.historical_success_rate = Known::Reported(0.1);

        let snapshot = RegistrySnapshot::new(vec![good, bad]);
        let mut cfg = config(RoutingStrategy::Balanced);
        cfg.pin_agent = Some("pinned".to_owned());

        let router = Router::new(cfg);
        let t = task();
        let decision = router.route(&RouteRequest::new(&t, &snapshot)).unwrap();

        assert_eq!(decision.selected.agent_id, AgentId::new("pinned"));
        assert!(!decision.has_alternatives(), "a pin leaves nothing else");
    }

    #[test]
    fn an_excluded_agent_is_never_selected() {
        let snapshot = RegistrySnapshot::new(vec![
            agent("banned", AgentProtocol::Acp),
            agent("allowed", AgentProtocol::Acp),
        ]);

        let mut cfg = config(RoutingStrategy::Balanced);
        cfg.exclude_agents = vec!["banned".to_owned()];

        let router = Router::new(cfg);
        let t = task();
        let decision = router.route(&RouteRequest::new(&t, &snapshot)).unwrap();
        assert_eq!(decision.selected.agent_id, AgentId::new("allowed"));
    }

    #[test]
    fn privacy_first_excludes_remote_agents_outright() {
        let snapshot = RegistrySnapshot::new(vec![
            agent("remote", AgentProtocol::A2A),
            agent("local", AgentProtocol::Acp),
        ]);

        let router = Router::new(config(RoutingStrategy::PrivacyFirst));
        let t = task();
        let decision = router.route(&RouteRequest::new(&t, &snapshot)).unwrap();

        assert_eq!(decision.selected.agent_id, AgentId::new("local"));
        assert!(decision.rejected.iter().any(|r| r.reason.contains("remote")));
    }

    // --- budget -----------------------------------------------------------

    #[test]
    fn a_candidate_that_would_blow_the_budget_is_filtered() {
        let mut expensive = agent("expensive", AgentProtocol::Acp);
        expensive.models.push(priced_model(&expensive, "big", 15.0, 75.0));
        let mut cheap = agent("cheap", AgentProtocol::Acp);
        cheap.models.push(priced_model(&cheap, "small", 0.25, 1.25));

        let snapshot = RegistrySnapshot::new(vec![expensive, cheap]);
        let router = Router::new(config(RoutingStrategy::QualityFirst));

        let mut t = task();
        t.spec.estimated_tokens = Some(1_000_000);

        // $1.00 left. The cheap model needs ~$0.55 for a million tokens; the
        // premium one needs ~$33, so exactly one candidate survives.
        let decision = router
            .route(&RouteRequest::new(&t, &snapshot).with_budget(9.0, Some(10.0)))
            .unwrap();

        assert_eq!(decision.selected.agent_id, AgentId::new("cheap"));
        assert!(
            decision.rejected.iter().any(|r| r.reason.contains("budget")),
            "rejections were: {:?}",
            decision.rejected
        );
    }

    #[test]
    fn an_exhausted_budget_stops_routing_entirely() {
        let mut a = agent("a", AgentProtocol::Acp);
        a.models.push(priced_model(&a, "m", 1.0, 1.0));

        let snapshot = RegistrySnapshot::new(vec![a]);
        let router = Router::new(config(RoutingStrategy::Balanced));
        let t = task();

        let err = router
            .route(&RouteRequest::new(&t, &snapshot).with_budget(10.0, Some(10.0)))
            .unwrap_err();
        assert!(err.to_string().contains("budget"));
    }

    // --- adaptive ---------------------------------------------------------

    #[test]
    fn observed_history_can_overturn_a_static_preference() {
        let mut favoured = agent("looks-good", AgentProtocol::Acp);
        favoured.performance_profile.historical_success_rate = Known::Reported(0.9);
        let mut underdog = agent("actually-good", AgentProtocol::Acp);
        underdog.performance_profile.historical_success_rate = Known::Reported(0.85);

        let snapshot = RegistrySnapshot::new(vec![favoured, underdog]);

        let mut cfg = config(RoutingStrategy::Balanced);
        cfg.weights = RouterWeights {
            quality: 0.0,
            cost: 0.0,
            latency: 0.0,
            reliability: 1.0,
            context: 0.0,
        };
        cfg.adaptive_weight = 0.9;
        cfg.adaptive_min_samples = 5;

        // Without history, the statically-better agent wins.
        let t = task();
        let baseline = Router::new(cfg.clone())
            .route(&RouteRequest::new(&t, &snapshot))
            .unwrap();
        assert_eq!(baseline.selected.agent_id, AgentId::new("looks-good"));

        // Now record what actually happened on this kind of task.
        let mut history = RoutingHistory::new();
        for _ in 0..30 {
            history.record(&OutcomeRecord::success(
                AgentId::new("actually-good"),
                None,
                TaskType::Implementation,
                1000,
                100,
            ));
            history.record(&OutcomeRecord::failure(
                AgentId::new("looks-good"),
                None,
                TaskType::Implementation,
                ErrorClass::TaskFailure,
                1000,
            ));
        }

        let adapted = Router::new(cfg)
            .with_history(history)
            .route(&RouteRequest::new(&t, &snapshot))
            .unwrap();

        assert_eq!(
            adapted.selected.agent_id,
            AgentId::new("actually-good"),
            "history should overturn the static profile"
        );
        assert!(
            adapted
                .selected
                .notes
                .iter()
                .any(|n| n.contains("observed history")),
            "and say so in the explanation"
        );
    }

    // --- explainability ---------------------------------------------------

    #[test]
    fn every_decision_carries_a_complete_explanation() {
        let mut a = agent("claude", AgentProtocol::Acp);
        a.models.push(priced_model(&a, "sonnet", 3.0, 15.0));
        let mut b = agent("codex", AgentProtocol::Acp);
        b.models.push(priced_model(&b, "gpt", 2.0, 8.0));

        let snapshot = RegistrySnapshot::new(vec![
            a,
            b,
            AgentDescriptor::new("useless", "useless", AgentProtocol::Native),
        ]);

        let router = Router::new(config(RoutingStrategy::Balanced));
        let t = task();
        let decision = router.route(&RouteRequest::new(&t, &snapshot)).unwrap();
        let text = decision.explain();

        assert!(text.contains("Selected:"));
        assert!(text.contains("Model:"));
        assert!(text.contains("Reasons:"));
        assert!(text.contains("Alternatives:"));
        assert!(text.contains("Rejected:"));
        assert!(decision.summary().contains("score"));
    }

    #[test]
    fn scores_stay_within_zero_and_one() {
        let mut a = agent("a", AgentProtocol::Acp);
        a.models.push(priced_model(&a, "m", 3.0, 15.0));
        let snapshot = RegistrySnapshot::new(vec![a]);

        let router = Router::new(config(RoutingStrategy::Balanced));
        let t = task();
        let decision = router.route(&RouteRequest::new(&t, &snapshot)).unwrap();

        assert!((0.0..=1.0).contains(&decision.selected.breakdown.total));
    }

    #[test]
    fn an_agent_with_several_models_yields_one_candidate_per_model() {
        let mut multi = agent("multi", AgentProtocol::Acp);
        multi.models.push(priced_model(&multi, "small", 0.25, 1.25));
        multi.models.push(priced_model(&multi, "large", 15.0, 75.0));

        let snapshot = RegistrySnapshot::new(vec![multi]);
        let router = Router::new(config(RoutingStrategy::CostFirst));
        let t = task();
        let decision = router.route(&RouteRequest::new(&t, &snapshot)).unwrap();

        assert_eq!(decision.selected.model_id, Some(ModelId::new("small")));
        assert_eq!(decision.alternatives.len(), 1);
        assert_eq!(decision.alternatives[0].model_id, Some(ModelId::new("large")));
    }
}
