#![no_main]

use arbitrary::Arbitrary;
use cuma_core::{
    Capability, CapabilitySet, Task, TaskGraph, TaskId, TaskSpec, TaskStatus, TaskType,
};
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
struct PlanDagInput {
    reply: String,
    cap_mask: u32,
    nodes: Vec<(u8, u8, Vec<u8>, String)>,
    fail_node: u8,
}

const CAPS: [Capability; 8] = [
    Capability::CodeComprehension,
    Capability::FileSystem,
    Capability::Research,
    Capability::Architecture,
    Capability::Planning,
    Capability::CodeGeneration,
    Capability::ShellExecution,
    Capability::CodeReview,
];

const TASK_TYPES: [TaskType; 11] = [
    TaskType::Inspection,
    TaskType::Research,
    TaskType::Design,
    TaskType::Implementation,
    TaskType::BugFix,
    TaskType::Refactor,
    TaskType::Testing,
    TaskType::Validation,
    TaskType::Documentation,
    TaskType::Review,
    TaskType::General,
];

fuzz_target!(|input: PlanDagInput| {
    let mut available = CapabilitySet::new();
    for (i, cap) in CAPS.iter().enumerate() {
        if (input.cap_mask >> i) & 1 == 1 {
            available.insert(cap.clone());
        }
    }

    if let Some(graph) = cuma_planner::parse_plan(&input.reply, &available) {
        assert!(graph.validate().is_ok());
        assert!(graph.len() >= 2);
        assert!(
            !graph.ready_tasks().is_empty(),
            "a valid non-empty DAG must always have at least one ready task"
        );
    }

    let mut graph = TaskGraph::new();
    let count = input.nodes.len().min(16);
    if count == 0 {
        return;
    }

    let ids: Vec<TaskId> = (0..count).map(|i| TaskId::new(format!("t{i}"))).collect();
    for (i, (type_idx, priority, deps, desc)) in input.nodes.into_iter().take(count).enumerate() {
        let task_type = TASK_TYPES[usize::from(type_idx) % TASK_TYPES.len()];
        let mut spec = TaskSpec::new(desc, task_type);
        spec.priority = priority;
        spec.dependencies = deps
            .into_iter()
            .take(4)
            .map(|d| {
                let target = usize::from(d) % (count + 1);
                if target < count {
                    ids[target].clone()
                } else {
                    TaskId::new("dangling")
                }
            })
            .collect();
        graph.add(Task::with_id(ids[i].clone(), spec));
    }

    if graph.validate().is_ok() {
        let ready = graph.ready_tasks();
        assert!(!ready.is_empty());
        for w in ready.windows(2) {
            assert!(w[0].spec.priority >= w[1].spec.priority);
        }

        let fail_id = &ids[usize::from(input.fail_node) % count];
        if let Some(task) = graph.get_mut(fail_id) {
            task.status = TaskStatus::Failed;
        }
        let skipped = graph.cascade_skip(fail_id);
        for skipped_id in skipped {
            assert_eq!(
                graph.get(&skipped_id).map(|t| t.status),
                Some(TaskStatus::Skipped)
            );
        }
        assert!(graph.validate().is_ok());
    }
});
