//! Recording as it happens: the database writes every attempt pays for.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use criterion::{Criterion, criterion_group, criterion_main};
use cuma_core::{AgentId, AttemptId, SessionId, TaskId, TaskType, TokenUsage};
use cuma_persistence::RuntimeStore;
use cuma_usage::UsageRecord;
use std::hint::black_box;

fn store(c: &mut Criterion) {
    let store = RuntimeStore::in_memory().unwrap();
    let session = SessionId::new("s");
    store.begin_session(&session, "goal").unwrap();

    c.bench_function("persistence/record-attempt", |b| {
        b.iter(|| {
            let record = UsageRecord {
                attempt_id: AttemptId::generate(),
                session_id: session.clone(),
                task_id: TaskId::new("t"),
                task_type: TaskType::Implementation,
                agent_id: AgentId::new("codex"),
                model_id: None,
                provider: None,
                started_at: chrono::Utc::now(),
                latency_ms: 1200,
                tokens: TokenUsage::reported(1000, 200),
                estimated_cost_usd: Some(0.01),
                cost_reported: false,
                success: true,
                failure_class: None,
                retry_count: 0,
            };
            store.record_attempt(black_box(&record)).unwrap();
        });
    });

    c.bench_function("persistence/load-routing-history", |b| {
        b.iter(|| black_box(store.load_routing_history().unwrap()));
    });

    c.bench_function("persistence/record-routing-decision", |b| {
        b.iter(|| {
            store
                .record_routing_decision(
                    &session,
                    &TaskId::new("t"),
                    &AgentId::new("codex"),
                    None,
                    0.8,
                    "because",
                )
                .unwrap();
        });
    });
}

criterion_group!(benches, store);
criterion_main!(benches);
