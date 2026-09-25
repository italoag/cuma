//! The harness's own overhead: what CUMA costs on top of the agents it runs.
//!
//! Each benchmark is a step the orchestrator takes on every task or event, so
//! a regression here is paid on every attempt of every session.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use cuma_config::RouterConfig;
use cuma_core::ports::ContextManager;
use cuma_core::{
    AgentDescriptor, AgentProtocol, CapabilitySet, CostProfile, Event, EventBus, EventKind, Known,
    ModelDescriptor, SessionId, Task, TaskGraph, TaskSpec, TaskType,
};
use cuma_registry::RegistrySnapshot;
use cuma_router::{RouteRequest, Router};
use std::hint::black_box;

fn every_capability() -> CapabilitySet {
    [
        TaskType::Implementation,
        TaskType::Refactor,
        TaskType::Testing,
        TaskType::BugFix,
    ]
    .into_iter()
    .flat_map(|t| {
        t.baseline_capabilities()
            .iter()
            .cloned()
            .collect::<Vec<_>>()
    })
    .collect()
}

fn fleet(size: usize) -> RegistrySnapshot {
    let agents = (0..size)
        .map(|i| {
            let mut agent = AgentDescriptor::new(
                format!("agent-{i}"),
                format!("agent-{i}"),
                AgentProtocol::Acp,
            )
            .with_capabilities(every_capability());
            for m in 0..3 {
                let mut model =
                    ModelDescriptor::minimal(agent.id.clone(), format!("m{i}-{m}"), "p");
                model.context_window = Known::Reported(200_000);
                model.cost = CostProfile {
                    input_per_mtok: Known::Reported(1.0 + i as f64 * 0.1),
                    output_per_mtok: Known::Reported(5.0 + m as f64),
                    cache_read_per_mtok: Known::Unknown,
                };
                agent.models.push(model);
            }
            agent
        })
        .collect();
    RegistrySnapshot::new(agents)
}

fn routing(c: &mut Criterion) {
    let router = Router::new(RouterConfig::default());
    let task = Task::new(TaskSpec::new(
        "implement the endpoint",
        TaskType::Implementation,
    ));
    for size in [5, 50, 200] {
        let snapshot = fleet(size);
        c.bench_function(&format!("route/{size}-agents"), |b| {
            b.iter(|| black_box(router.route(&RouteRequest::new(&task, &snapshot)).unwrap()));
        });
    }
}

fn event_bus(c: &mut Criterion) {
    c.bench_function("event_bus/publish-1000-to-4-subscribers", |b| {
        b.iter_batched(
            || {
                let bus = EventBus::new(2048);
                let receivers: Vec<_> = (0..4).map(|_| bus.subscribe()).collect();
                (bus, receivers)
            },
            |(bus, receivers)| {
                let session = SessionId::new("s");
                for _ in 0..1000 {
                    bus.publish(Event::session(
                        session.clone(),
                        EventKind::TaskPlanned { task_count: 3 },
                    ));
                }
                black_box(receivers)
            },
            BatchSize::SmallInput,
        );
    });
}

fn task_graph(c: &mut Criterion) {
    // A 200-task chain with fan-out: every task depends on the one before.
    let mut graph = TaskGraph::new();
    let mut previous = None;
    for i in 0..200 {
        let mut spec = TaskSpec::new(format!("task {i}"), TaskType::Implementation);
        if let Some(id) = previous.take() {
            spec.dependencies.push(id);
        }
        previous = Some(graph.add(Task::new(spec)));
    }
    c.bench_function("task_graph/ready-set-200", |b| {
        b.iter(|| black_box(graph.ready_tasks().len()))
    });
    c.bench_function("task_graph/validate-200", |b| {
        b.iter(|| graph.validate().unwrap())
    });
}

fn context_assembly(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let manager = cuma_orchestrator::MinimalContextManager::new();
    let mut graph = TaskGraph::new();
    let mut first = Task::new(TaskSpec::new("design the schema", TaskType::Design));
    first.status = cuma_core::TaskStatus::Completed;
    let first_id = graph.add(first);
    let mut spec = TaskSpec::new(
        "implement the migration for src/db/schema.rs",
        TaskType::Implementation,
    );
    spec.dependencies.push(first_id);
    let task_id = graph.add(Task::new(spec));
    let task = graph.get(&task_id).unwrap().clone();

    c.bench_function("context/assemble", |b| {
        b.iter(|| {
            black_box(
                runtime
                    .block_on(manager.assemble(&task, &graph, None, 100_000))
                    .unwrap(),
            )
        });
    });
}

fn write_prediction(c: &mut Criterion) {
    let files = (0..50_000)
        .map(|i| std::path::PathBuf::from(format!("crates/c{}/src/module_{i}.rs", i % 40)))
        .collect();
    let index = cuma_workspace::WorkspaceIndex::from_files(files);
    c.bench_function("ownership/predict-writes-50k-file-index", |b| {
        b.iter(|| {
            black_box(cuma_workspace::predict_writes(
                "fix module_4242.rs and crates/c3/src/module_3.rs",
                Some(&index),
                &[],
            ))
        });
    });
}

criterion_group!(
    benches,
    routing,
    event_bus,
    task_graph,
    context_assembly,
    write_prediction
);
criterion_main!(benches);
