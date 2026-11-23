#![no_main]

use arbitrary::Arbitrary;
use cuma_config::{RouterConfig, RouterWeights, RoutingStrategy};
use cuma_core::{
    AgentDescriptor, AgentProtocol, Capability, CapabilitySet, CostProfile, ErrorClass,
    HealthState, Known, ModelDescriptor, Task, TaskId, TaskSpec, TaskType,
};
use cuma_registry::RegistrySnapshot;
use cuma_resilience::{Backoff, RetryDecision, RetryPolicy};
use cuma_router::{RouteRequest, Router};
use cuma_workspace::OwnershipLedger;
use libfuzzer_sys::fuzz_target;
use rand::SeedableRng;
use rand::rngs::StdRng;
use std::path::PathBuf;

#[derive(Arbitrary, Debug)]
struct FuzzAgent {
    id: u8,
    enabled: bool,
    a2a: bool,
    health_idx: u8,
    cap_mask: u8,
    input_price: Option<u8>,
    output_price: Option<u8>,
    latency_ms: Option<u16>,
    success_pct: Option<u8>,
    coding_pct: Option<u8>,
    reasoning_pct: Option<u8>,
    context_window: Option<u16>,
}

#[derive(Arbitrary, Debug)]
struct RouterResilienceInput {
    agents: Vec<FuzzAgent>,
    strategy_idx: u8,
    task_cap_mask: u8,
    complexity_pct: u8,
    estimated_tokens: Option<u16>,
    spent_cents: u8,
    budget_cents: Option<u8>,
    error_idx: u8,
    max_attempts: u8,
    per_target: u8,
    base_ms: u8,
    max_ms: u8,
    jitter_pct: u8,
    seed: u16,
    paths_a: Vec<String>,
    paths_b: Vec<String>,
}

const CAPS: [Capability; 5] = [
    Capability::CodeComprehension,
    Capability::CodeGeneration,
    Capability::CodeEditing,
    Capability::Debugging,
    Capability::ShellExecution,
];

const STRATEGIES: [RoutingStrategy; 7] = [
    RoutingStrategy::Balanced,
    RoutingStrategy::QualityFirst,
    RoutingStrategy::CostFirst,
    RoutingStrategy::LatencyFirst,
    RoutingStrategy::LocalFirst,
    RoutingStrategy::PrivacyFirst,
    RoutingStrategy::Manual,
];

const HEALTHS: [HealthState; 5] = [
    HealthState::Healthy,
    HealthState::Degraded,
    HealthState::RateLimited,
    HealthState::Unavailable,
    HealthState::Unknown,
];

const ERRORS: [ErrorClass; 16] = [
    ErrorClass::RateLimit,
    ErrorClass::QuotaExceeded,
    ErrorClass::AuthenticationFailure,
    ErrorClass::AgentCrash,
    ErrorClass::Timeout,
    ErrorClass::ProtocolError,
    ErrorClass::ContextOverflow,
    ErrorClass::ToolFailure,
    ErrorClass::ModelUnavailable,
    ErrorClass::ConnectionFailure,
    ErrorClass::InvalidResponse,
    ErrorClass::TaskFailure,
    ErrorClass::SecurityViolation,
    ErrorClass::Configuration,
    ErrorClass::Cancelled,
    ErrorClass::Unknown,
];

fn caps_from_mask(mask: u8) -> CapabilitySet {
    let mut set = CapabilitySet::new();
    for (i, cap) in CAPS.iter().enumerate() {
        if (mask >> i) & 1 == 1 {
            set.insert(cap.clone());
        }
    }
    set
}

fuzz_target!(|input: RouterResilienceInput| {
    // --- Router scoring & filtering invariants ---
    let mut descriptors = Vec::new();
    for (idx, fa) in input.agents.iter().take(5).enumerate() {
        let protocol = if fa.a2a {
            AgentProtocol::A2A
        } else {
            AgentProtocol::Acp
        };
        let mut agent = AgentDescriptor::new(format!("agent-{}-{}", fa.id, idx), "fuzz", protocol)
            .with_capabilities(caps_from_mask(fa.cap_mask));
        agent.enabled = fa.enabled;
        agent.health.state = HEALTHS[usize::from(fa.health_idx) % HEALTHS.len()];
        agent.performance_profile.typical_latency_ms = fa
            .latency_ms
            .map_or(Known::Unknown, |v| Known::Reported(u64::from(v)));
        agent.performance_profile.historical_success_rate = fa
            .success_pct
            .map_or(Known::Unknown, |p| Known::Reported(f64::from(p) / 100.0));
        agent.performance_profile.coding_score = fa
            .coding_pct
            .map_or(Known::Unknown, |p| Known::Reported(f64::from(p) / 100.0));
        agent.performance_profile.reasoning_score = fa
            .reasoning_pct
            .map_or(Known::Unknown, |p| Known::Reported(f64::from(p) / 100.0));
        if let (Some(i), Some(o)) = (fa.input_price, fa.output_price) {
            agent.cost_profile = CostProfile {
                input_per_mtok: Known::Reported(f64::from(i) / 100.0),
                output_per_mtok: Known::Reported(f64::from(o) / 100.0),
                cache_read_per_mtok: Known::Unknown,
            };
        }
        let mut model = ModelDescriptor::minimal(agent.id.clone(), "m1", "M1");
        model.context_window = fa
            .context_window
            .map_or(Known::Unknown, |w| Known::Reported(u64::from(w) * 100));
        model.cost = agent.cost_profile.clone();
        model.performance = agent.performance_profile.clone();
        agent.models.push(model);
        descriptors.push(agent);
    }

    let snapshot = RegistrySnapshot::new(descriptors);
    let strategy = STRATEGIES[usize::from(input.strategy_idx) % STRATEGIES.len()];
    let weights = if strategy == RoutingStrategy::Manual {
        RouterWeights::default()
    } else {
        strategy.default_weights()
    };
    let router = Router::new(RouterConfig {
        strategy,
        weights,
        ..RouterConfig::default()
    });

    let mut spec = TaskSpec::new("fuzz task", TaskType::General)
        .with_complexity(f64::from(input.complexity_pct) / 100.0);
    spec.required_capabilities = caps_from_mask(input.task_cap_mask);
    spec.estimated_tokens = input.estimated_tokens.map(|t| u64::from(t) * 100);
    let task = Task::new(spec);

    let budget = input.budget_cents.map(|c| f64::from(c) / 100.0);
    let req = RouteRequest::new(&task, &snapshot)
        .with_budget(f64::from(input.spent_cents) / 100.0, budget);

    if let Ok(decision) = router.route(&req) {
        let b = decision.selected.breakdown;
        for v in [b.quality, b.cost, b.latency, b.reliability, b.context] {
            assert!(
                (0.0..=1.0).contains(&v),
                "dimension score out of bounds: {v}"
            );
        }
    }

    // --- Resilience bounded termination invariant ---
    let policy = RetryPolicy {
        max_attempts: u32::from(input.max_attempts.clamp(1, 24)),
        max_attempts_per_target: u32::from(input.per_target.clamp(1, 12)),
        backoff: Backoff {
            base_ms: u64::from(input.base_ms),
            multiplier: 2.0,
            max_ms: u64::from(input.max_ms),
            jitter: f64::from(input.jitter_pct.min(100)) / 100.0,
        },
    };
    let class = ERRORS[usize::from(input.error_idx) % ERRORS.len()];
    let mut rng = StdRng::seed_from_u64(u64::from(input.seed));
    let mut attempts = 0u32;
    let mut on_target = 0u32;

    loop {
        attempts += 1;
        on_target += 1;
        assert!(
            attempts <= policy.max_attempts + 1,
            "retry policy exceeded hard bound"
        );
        match policy.decide(class, attempts, on_target, true, &mut rng) {
            RetryDecision::RetrySameTarget { .. } => {}
            RetryDecision::Reroute { .. } => on_target = 0,
            RetryDecision::Replan { .. } | RetryDecision::GiveUp { .. } => break,
        }
    }

    // --- Ownership ledger conflict/release invariants ---
    let ledger = OwnershipLedger::new();
    let t1 = TaskId::new("t1");
    let t2 = TaskId::new("t2");
    let p1: Vec<PathBuf> = input
        .paths_a
        .into_iter()
        .take(6)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect();
    let p2: Vec<PathBuf> = input
        .paths_b
        .into_iter()
        .take(6)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect();

    if !p1.is_empty() {
        let _ = ledger.claim(&t1, &p1);
        let would = ledger.would_conflict(&t2, &p2);
        let claimed = ledger.claim(&t2, &p2);
        assert_eq!(would.is_some(), claimed.is_err());
        if claimed.is_err() {
            assert!(ledger.held_by(&t2).is_empty());
        }
        ledger.release(&t1);
        assert!(ledger.held_by(&t1).is_empty());
    }
});
