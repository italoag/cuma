//! Worktree isolation, against a real git repository.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use cuma_config::{Config, TaskIsolation};
use cuma_core::{
    AgentDescriptor, AgentProtocol, Capability, CapabilitySet, Known, ModelDescriptor,
};
use cuma_orchestrator::Orchestrator;
use cuma_planner::HeuristicPlanner;
use cuma_testkit::{Behaviour, MockAgent};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn repository() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "test@example.invalid"]);
    git(root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("README.md"), "hello\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "initial"]);
    directory
}

fn every_capability() -> CapabilitySet {
    [
        Capability::CodeComprehension,
        Capability::CodeGeneration,
        Capability::CodeEditing,
        Capability::Debugging,
        Capability::Refactoring,
        Capability::Testing,
        Capability::ShellExecution,
        Capability::FileSystem,
        Capability::VersionControl,
        Capability::Research,
        Capability::Documentation,
        Capability::Architecture,
        Capability::CodeReview,
        Capability::Planning,
        Capability::ToolUse,
    ]
    .into_iter()
    .collect()
}

#[tokio::test]
async fn an_isolated_task_writes_in_its_own_worktree_and_its_work_lands_uncommitted() {
    let repository = repository();
    let root = repository.path();
    // Uncommitted work the agent must see, and must not disturb.
    std::fs::write(root.join("notes.txt"), "in progress\n").unwrap();
    let head_before = git(root, &["rev-parse", "HEAD"]);

    let mut descriptor = AgentDescriptor::new("writer", "writer", AgentProtocol::Native)
        .with_capabilities(every_capability());
    let mut model = ModelDescriptor::minimal(descriptor.id.clone(), "m", "writer");
    model.context_window = Known::Reported(200_000);
    descriptor.models.push(model);
    let agent = MockAgent::always(
        "writer",
        Behaviour::Writes {
            files: vec![("src/health.rs".into(), "pub fn health() {}\n".into())],
            output: "added the endpoint".into(),
        },
    )
    .with_descriptor(descriptor);
    let workspaces = agent.workspace_log();

    let mut config = Config::default();
    config.limits.isolation = TaskIsolation::Worktree;
    config.security.checkpoint_before_write = false;

    let mut orchestrator = Orchestrator::new(
        config,
        Arc::new(HeuristicPlanner::new()),
        root.to_path_buf(),
    );
    orchestrator.add_agent(Arc::new(agent)).await.unwrap();

    let result = orchestrator.run("implement src/health.rs").await.unwrap();
    assert!(result.success, "{}", result.summary);

    let workspaces = workspaces.lock().unwrap().clone();
    assert!(!workspaces.is_empty());
    // Read-only tasks share the workspace by design; writing tasks do not.
    let isolated: Vec<_> = workspaces
        .iter()
        .filter(|w| w.to_string_lossy().contains("cuma-worktrees"))
        .collect();
    let writing = result
        .graph
        .iter()
        .filter(|t| t.spec.risk != cuma_core::Risk::ReadOnly)
        .count();
    assert_eq!(
        isolated.len(),
        writing,
        "one worktree per writing task: {workspaces:?}"
    );
    assert!(writing > 0);

    assert_eq!(
        std::fs::read_to_string(root.join("src/health.rs")).unwrap(),
        "pub fn health() {}\n",
        "the work came back"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("notes.txt")).unwrap(),
        "in progress\n"
    );
    assert_eq!(
        git(root, &["rev-parse", "HEAD"]),
        head_before,
        "nothing was committed"
    );
    assert!(
        git(root, &["worktree", "list"]).lines().count() == 1,
        "finished worktrees are removed"
    );

    let wrote_health = result
        .graph
        .iter()
        .any(|task| task.artifacts.iter().any(|a| a == "src/health.rs"));
    assert!(wrote_health, "the changed file is recorded on the task");
}

#[tokio::test]
async fn without_isolation_tasks_work_in_the_workspace_itself() {
    let repository = repository();
    let root = repository.path();

    let mut descriptor = AgentDescriptor::new("writer", "writer", AgentProtocol::Native)
        .with_capabilities(every_capability());
    let mut model = ModelDescriptor::minimal(descriptor.id.clone(), "m", "writer");
    model.context_window = Known::Reported(200_000);
    descriptor.models.push(model);
    let agent = MockAgent::always("writer", Behaviour::ok("done")).with_descriptor(descriptor);
    let workspaces = agent.workspace_log();

    let mut orchestrator = Orchestrator::new(
        Config::default(),
        Arc::new(HeuristicPlanner::new()),
        root.to_path_buf(),
    );
    orchestrator.add_agent(Arc::new(agent)).await.unwrap();
    orchestrator.run("implement src/health.rs").await.unwrap();

    assert!(workspaces.lock().unwrap().iter().all(|w| w == root));
}
