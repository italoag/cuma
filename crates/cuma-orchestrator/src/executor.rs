//! The execution loop.

use crate::context::MinimalContextManager;
use cuma_config::Config;
use cuma_core::error::{MetaAgentError, Result};
use cuma_core::ports::{AgentAdapter, ContextManager, ExecutionUpdate, MemoryStore, Planner, PlanningContext};
use cuma_core::{
    AgentHandoff, AgentId, AttemptId, Event, EventBus, EventKind, ExecutionOutcome, ModelId,
    SessionId, Task, TaskGraph, TaskId, TaskStatus, TokenUsage,
};
use cuma_registry::{AgentRegistry, ModelRegistry};
use cuma_resilience::{CircuitBreakerRegistry, RetryDecision, RetryPolicy, classify_message};
use cuma_router::{OutcomeRecord, RouteRequest, Router, RoutingHistory};
use cuma_usage::{UsageRecord, UsageTracker};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

/// What a session produced.
#[derive(Debug, Clone)]
pub struct SessionResult {
    /// The session identifier.
    pub session_id: SessionId,
    /// The final state of every task.
    pub graph: TaskGraph,
    /// Whether every task completed.
    pub success: bool,
    /// Usage across the whole session.
    pub usage: cuma_usage::UsageTotals,
    /// USD spent, counting only priced attempts.
    pub spent_usd: f64,
    /// A human-readable summary of what happened.
    pub summary: String,
}

impl SessionResult {
    /// Tasks that completed successfully.
    pub fn completed_tasks(&self) -> usize {
        self.graph
            .iter()
            .filter(|t| t.status == TaskStatus::Completed)
            .count()
    }

    /// Tasks that failed outright.
    pub fn failed_tasks(&self) -> usize {
        self.graph
            .iter()
            .filter(|t| t.status == TaskStatus::Failed)
            .count()
    }

    /// Tasks skipped because a dependency failed.
    pub fn skipped_tasks(&self) -> usize {
        self.graph
            .iter()
            .filter(|t| t.status == TaskStatus::Skipped)
            .count()
    }
}

/// Drives a plan to completion.
pub struct Orchestrator {
    config: Config,
    planner: Arc<dyn Planner>,
    agents: AgentRegistry,
    models: ModelRegistry,
    adapters: BTreeMap<AgentId, Arc<dyn AgentAdapter>>,
    breakers: CircuitBreakerRegistry,
    retry_policy: RetryPolicy,
    context_manager: Arc<dyn ContextManager>,
    memory: Option<Arc<dyn MemoryStore>>,
    events: EventBus,
    usage: Arc<Mutex<UsageTracker>>,
    history: Arc<Mutex<RoutingHistory>>,
    workspace: PathBuf,
}

impl Orchestrator {
    /// Build an orchestrator.
    pub fn new(config: Config, planner: Arc<dyn Planner>, workspace: PathBuf) -> Self {
        let retry_policy = RetryPolicy::with_max_attempts(config.limits.max_retries.max(1) + 1);

        Self {
            config,
            planner,
            agents: AgentRegistry::new(),
            models: ModelRegistry::new(),
            adapters: BTreeMap::new(),
            breakers: CircuitBreakerRegistry::default(),
            retry_policy,
            context_manager: Arc::new(MinimalContextManager::new()),
            memory: None,
            events: EventBus::default(),
            usage: Arc::new(Mutex::new(UsageTracker::new())),
            history: Arc::new(Mutex::new(RoutingHistory::new())),
            workspace,
        }
    }

    /// Register an agent and the adapter that reaches it.
    ///
    /// Registry and adapters are registered together because an agent the
    /// router can select but the orchestrator cannot reach is worse than an
    /// agent that does not exist: it wins a routing decision and then fails.
    pub async fn add_agent(&mut self, adapter: Arc<dyn AgentAdapter>) -> Result<()> {
        let descriptor = adapter.describe().await?;
        self.models.register_agent_models(&descriptor).await;
        self.agents.register(descriptor).await;
        self.adapters.insert(adapter.agent_id().clone(), adapter);
        Ok(())
    }

    /// Attach a long-term memory store.
    #[must_use]
    pub fn with_memory(mut self, memory: Arc<dyn MemoryStore>) -> Self {
        self.memory = Some(memory);
        self
    }

    /// Attach a context manager.
    #[must_use]
    pub fn with_context_manager(mut self, manager: Arc<dyn ContextManager>) -> Self {
        self.context_manager = manager;
        self
    }

    /// Attach an event bus, so a UI can subscribe.
    #[must_use]
    pub fn with_events(mut self, events: EventBus) -> Self {
        self.events = events;
        self
    }

    /// Seed the router with prior history.
    #[must_use]
    pub fn with_history(self, history: RoutingHistory) -> Self {
        // Replace the contents rather than the Arc, so any handle already
        // handed out keeps pointing at the live history.
        if let Ok(mut guard) = self.history.try_lock() {
            *guard = history;
        }
        self
    }

    /// The event bus, for subscribing.
    pub fn events(&self) -> &EventBus {
        &self.events
    }

    /// The agent registry.
    pub fn agents(&self) -> &AgentRegistry {
        &self.agents
    }

    /// The circuit breakers.
    pub fn breakers(&self) -> &CircuitBreakerRegistry {
        &self.breakers
    }

    /// A snapshot of the usage ledger.
    pub async fn usage_snapshot(&self) -> UsageTracker {
        self.usage.lock().await.clone()
    }

    /// A snapshot of the routing history.
    pub async fn history_snapshot(&self) -> RoutingHistory {
        self.history.lock().await.clone()
    }

    /// Plan and execute a goal end to end.
    pub async fn run(&self, goal: &str) -> Result<SessionResult> {
        let session_id = SessionId::generate();

        self.events.publish(Event::session(
            session_id.clone(),
            EventKind::SessionStarted {
                goal: goal.to_owned(),
            },
        ));

        let graph = self.plan(goal).await?;

        self.events.publish(Event::session(
            session_id.clone(),
            EventKind::TaskPlanned {
                task_count: graph.len(),
            },
        ));

        let graph = self.execute_graph(&session_id, graph).await?;

        let usage = self.usage.lock().await;
        let totals = usage.totals();
        let spent = usage.spent_usd();
        drop(usage);

        let success = graph
            .iter()
            .all(|t| matches!(t.status, TaskStatus::Completed));

        self.events.publish(Event::session(
            session_id.clone(),
            EventKind::SessionCompleted { success },
        ));

        let summary = Self::summarize(&graph, &totals);

        Ok(SessionResult {
            session_id,
            graph,
            success,
            usage: totals,
            spent_usd: spent,
            summary,
        })
    }

    /// Produce a plan without executing it.
    ///
    /// Backs `cuma explain` and `cuma run --dry-run`: seeing what the harness
    /// intends to do, and what it would cost, before it does any of it.
    pub async fn plan_only(&self, goal: &str) -> Result<TaskGraph> {
        self.plan(goal).await
    }

    /// Show how a task would route, without executing it.
    pub async fn explain_routing(&self, task: &Task) -> Result<cuma_router::RoutingDecision> {
        self.route(task, &[]).await
    }

    /// Produce a plan, seeded with what the registry can do and what memory knows.
    async fn plan(&self, goal: &str) -> Result<TaskGraph> {
        let snapshot = self.agents.snapshot().await;

        let memories = match &self.memory {
            Some(store) if store.is_available().await => store
                .recall(goal, self.config.memory.recall_limit)
                .await
                // Memory is an optimization. A memory backend that is down
                // must degrade recall, never fail the session.
                .unwrap_or_else(|err| {
                    tracing::warn!(error = %err, "memory recall failed; continuing without it");
                    Vec::new()
                }),
            _ => Vec::new(),
        };

        let context = PlanningContext {
            workspace: self.workspace.clone(),
            available_capabilities: snapshot.available_capabilities(),
            memories,
            hints: BTreeMap::new(),
        };

        let graph = self.planner.plan(goal, &context).await?;
        graph.validate()?;
        Ok(graph)
    }

    /// Walk the DAG until every task is terminal.
    async fn execute_graph(&self, session_id: &SessionId, mut graph: TaskGraph) -> Result<TaskGraph> {
        // Each pass executes one wave of ready tasks. The loop is bounded by
        // the number of tasks because every pass drives at least one task to a
        // terminal state — a pass that cannot is treated as a stall and broken
        // out of, rather than spun on.
        let max_passes = graph.len().saturating_mul(2) + 4;

        for _ in 0..max_passes {
            if graph.is_complete() {
                break;
            }

            let ready: Vec<TaskId> = graph
                .ready_tasks()
                .iter()
                .take(self.config.limits.max_parallel_tasks)
                .map(|t| t.id.clone())
                .collect();

            if ready.is_empty() {
                if !graph.is_complete() {
                    tracing::warn!("no tasks are ready and the plan is unfinished; stopping");
                }
                break;
            }

            // Tasks in one wave are dependency-independent, so they could run
            // concurrently. They are run sequentially here because concurrent
            // writers to one workspace corrupt each other, and workspace
            // isolation (git worktrees, file ownership) is the mechanism that
            // makes real parallelism safe. See docs/ORCHESTRATION.md.
            for task_id in ready {
                let outcome = self.execute_task(session_id, &mut graph, &task_id).await;

                match outcome {
                    Ok(true) => {}
                    Ok(false) | Err(_) => {
                        if let Some(task) = graph.get_mut(&task_id) {
                            task.status = TaskStatus::Failed;
                        }

                        let skipped = graph.cascade_skip(&task_id);
                        for skipped_id in skipped {
                            self.events.publish(Event::task(
                                session_id.clone(),
                                skipped_id,
                                EventKind::TaskSkipped {
                                    blocked_by: task_id.clone(),
                                },
                            ));
                        }
                    }
                }
            }
        }

        Ok(graph)
    }

    /// Run one task through route → execute → classify → react, until it
    /// reaches a terminal state.
    ///
    /// Returns `Ok(true)` when the task completed, `Ok(false)` when it failed
    /// after exhausting its options.
    async fn execute_task(
        &self,
        session_id: &SessionId,
        graph: &mut TaskGraph,
        task_id: &TaskId,
    ) -> Result<bool> {
        let mut handoff: Option<AgentHandoff> = None;
        let mut attempts_on_target = 0u32;

        // Targets the resilience layer has decided to *abandon*, as opposed to
        // targets that have merely failed once. A rate limit produces a failed
        // attempt but explicitly asks for the same agent to be retried, so
        // excluding every failed target here would turn every retry into a
        // reroute — and, with one agent registered, into an immediate failure.
        let mut abandoned: Vec<(AgentId, Option<ModelId>)> = Vec::new();

        loop {
            let Some(task) = graph.get(task_id).cloned() else {
                return Ok(false);
            };

            if task.attempt_count() as u32 >= self.retry_policy.max_attempts {
                self.fail_task(session_id, graph, task_id, "retry budget exhausted");
                return Ok(false);
            }

            // --- route ----------------------------------------------------
            let decision = match self.route(&task, &abandoned).await {
                Ok(decision) => decision,
                Err(err) => {
                    self.events.publish(Event::task(
                        session_id.clone(),
                        task_id.clone(),
                        EventKind::RoutingFailed {
                            reason: err.to_string(),
                        },
                    ));
                    self.fail_task(session_id, graph, task_id, &err.to_string());
                    return Ok(false);
                }
            };

            let agent_id = decision.selected.agent_id.clone();
            let model_id = decision.selected.model_id.clone();

            self.events.publish(Event::task(
                session_id.clone(),
                task_id.clone(),
                EventKind::AgentSelected {
                    agent: agent_id.clone(),
                    model: model_id.clone(),
                    score: decision.selected.breakdown.total,
                    explanation: decision.explain(),
                },
            ));

            if let Some(task) = graph.get_mut(task_id) {
                task.status = TaskStatus::Running;
                task.assigned_agent = Some(agent_id.clone());
                task.assigned_model = model_id.clone();
            }

            // --- execute --------------------------------------------------
            let attempt_id = AttemptId::generate();
            let started_at = chrono::Utc::now();
            let started = std::time::Instant::now();

            self.events.publish(
                Event::task(
                    session_id.clone(),
                    task_id.clone(),
                    EventKind::AgentStarted {
                        agent: agent_id.clone(),
                    },
                )
                .with_attempt(attempt_id.clone()),
            );

            let result = self
                .invoke_adapter(
                    session_id,
                    &task,
                    graph,
                    &agent_id,
                    model_id.as_ref(),
                    handoff.as_ref(),
                )
                .await;

            #[allow(clippy::cast_possible_truncation)]
            let latency_ms = started.elapsed().as_millis() as u64;

            // --- record ---------------------------------------------------
            let (outcome, failure) = match result {
                Ok(outcome) if outcome.success => (Some(outcome), None),
                Ok(outcome) => {
                    let class = outcome
                        .failure_class
                        .unwrap_or(cuma_core::ErrorClass::TaskFailure);
                    let reason = outcome
                        .failure_reason
                        .clone()
                        .unwrap_or_else(|| "the agent reported the task as failed".to_owned());
                    (Some(outcome), Some((class, reason)))
                }
                Err(err) => {
                    // Adapters classify what they can; anything left as
                    // `Unknown` gets one more pass over its message text.
                    let class = match err.class() {
                        cuma_core::ErrorClass::Unknown => classify_message(&err.to_string()),
                        known => known,
                    };
                    (None, Some((class, err.to_string())))
                }
            };

            let tokens = outcome
                .as_ref()
                .map_or(TokenUsage::estimated(0, 0), |o| o.tokens);

            self.record_attempt(
                session_id,
                &task,
                &attempt_id,
                &agent_id,
                model_id.as_ref(),
                started_at,
                latency_ms,
                tokens,
                failure.is_none(),
                failure.as_ref().map(|(class, _)| *class),
                attempts_on_target,
            )
            .await;

            // --- success --------------------------------------------------
            let Some((class, reason)) = failure else {
                self.breakers.record_success(&agent_id, model_id.as_ref());
                self.agents
                    .set_health(&agent_id, cuma_core::HealthState::Healthy, None)
                    .await;
                self.agents.record_latency(&agent_id, latency_ms).await;

                if let Some(model) = &model_id {
                    self.models.record_outcome(&agent_id, model, true).await;
                }

                if let (Some(task), Some(outcome)) = (graph.get_mut(task_id), outcome) {
                    task.artifacts.extend(outcome.changed_files.iter().cloned());
                    task.attempts.push(outcome);
                    task.status = TaskStatus::Completed;
                }

                self.events.publish(Event::task(
                    session_id.clone(),
                    task_id.clone(),
                    EventKind::TaskCompleted { tokens },
                ));

                self.remember_success(&task, &agent_id).await;
                return Ok(true);
            };

            // --- failure --------------------------------------------------
            self.events.publish(
                Event::task(
                    session_id.clone(),
                    task_id.clone(),
                    EventKind::AgentFailed {
                        agent: agent_id.clone(),
                        class,
                        message: reason.clone(),
                    },
                )
                .with_attempt(attempt_id.clone()),
            );

            let breaker_state =
                self.breakers
                    .record_failure(&agent_id, model_id.as_ref(), class, &reason);

            self.events.publish(Event::task(
                session_id.clone(),
                task_id.clone(),
                EventKind::CircuitBreakerChanged {
                    agent: agent_id.clone(),
                    state: format!("{breaker_state:?}"),
                },
            ));

            if class.counts_against_health() {
                self.agents
                    .set_health(
                        &agent_id,
                        self.breakers.health(&agent_id),
                        Some(reason.clone()),
                    )
                    .await;
            }

            if let Some(model) = &model_id {
                self.models.record_outcome(&agent_id, model, false).await;
            }

            let recorded = outcome.unwrap_or_else(|| ExecutionOutcome {
                attempt_id: attempt_id.clone(),
                agent_id: agent_id.clone(),
                model_id: model_id.clone(),
                success: false,
                output: String::new(),
                changed_files: Vec::new(),
                tokens,
                latency_ms,
                failure_class: Some(class),
                failure_reason: Some(reason.clone()),
            });

            let attempts_so_far = if let Some(task) = graph.get_mut(task_id) {
                task.attempts.push(recorded);
                task.attempt_count() as u32
            } else {
                return Ok(false);
            };

            attempts_on_target += 1;

            // --- decide ---------------------------------------------------
            let Some(task) = graph.get(task_id).cloned() else {
                return Ok(false);
            };

            // "Is there anywhere else to go?" means: excluding everything
            // already abandoned *and* the target that just failed.
            let mut probe = abandoned.clone();
            probe.push((agent_id.clone(), model_id.clone()));
            let alternatives_available = self.route(&task, &probe).await.is_ok();

            let decision = self.retry_policy.decide(
                class,
                attempts_so_far,
                attempts_on_target,
                alternatives_available,
                &mut rand::rng(),
            );

            match decision {
                RetryDecision::RetrySameTarget {
                    delay,
                    attempt,
                    reason,
                } => {
                    self.events.publish(Event::task(
                        session_id.clone(),
                        task_id.clone(),
                        EventKind::RetryScheduled {
                            attempt,
                            #[allow(clippy::cast_possible_truncation)]
                            delay_ms: delay.as_millis() as u64,
                            reason,
                        },
                    ));
                    tokio::time::sleep(delay).await;
                }

                RetryDecision::Reroute { reason } => {
                    // Build the handoff *before* rerouting, so the next agent
                    // starts from a summary rather than from nothing.
                    handoff = Some(Self::build_handoff(&task, &agent_id, &reason));
                    attempts_on_target = 0;
                    abandoned.push((agent_id.clone(), model_id.clone()));

                    self.events.publish(Event::task(
                        session_id.clone(),
                        task_id.clone(),
                        EventKind::FallbackSelected {
                            from: agent_id.clone(),
                            // The replacement is not chosen until the next
                            // routing pass; naming the outgoing agent twice is
                            // honest about that.
                            to: agent_id.clone(),
                            reason,
                        },
                    ));
                }

                RetryDecision::Replan { reason } => {
                    match self.planner.replan(graph, &task, &reason).await? {
                        Some(revised) => {
                            *graph = revised;
                            return Ok(true);
                        }
                        None => {
                            self.fail_task(session_id, graph, task_id, &reason);
                            return Ok(false);
                        }
                    }
                }

                RetryDecision::GiveUp { reason } => {
                    self.fail_task(session_id, graph, task_id, &reason);
                    return Ok(false);
                }
            }
        }
    }

    /// Route one task, excluding targets the resilience layer has abandoned.
    async fn route(
        &self,
        task: &Task,
        abandoned: &[(AgentId, Option<ModelId>)],
    ) -> Result<cuma_router::RoutingDecision> {
        let snapshot = self.agents.snapshot().await;
        let history = self.history.lock().await.clone();
        let spent = self.usage.lock().await.spent_usd();

        let router = Router::new(self.config.router.clone())
            .with_breakers(self.breakers.clone())
            .with_history(history);

        router.route(
            &RouteRequest::new(task, &snapshot)
                .excluding(abandoned)
                .with_budget(spent, self.config.limits.max_cost_usd),
        )
    }

    /// Assemble context and hand the task to its adapter.
    #[allow(clippy::too_many_arguments)]
    async fn invoke_adapter(
        &self,
        session_id: &SessionId,
        task: &Task,
        graph: &TaskGraph,
        agent_id: &AgentId,
        model_id: Option<&ModelId>,
        handoff: Option<&AgentHandoff>,
    ) -> Result<ExecutionOutcome> {
        let Some(adapter) = self.adapters.get(agent_id) else {
            return Err(MetaAgentError::Configuration(format!(
                "agent {agent_id} is registered but has no adapter to reach it"
            )));
        };

        let token_budget = self
            .agents
            .get(agent_id)
            .await
            .and_then(|agent| {
                model_id
                    .and_then(|id| agent.model(id).cloned())
                    .and_then(|model| model.context_window.value())
            })
            // Leave headroom for the agent's own tool output and reasoning.
            .map_or(100_000, |window| window * 6 / 10);

        let prompt = self
            .context_manager
            .assemble(task, graph, handoff, token_budget)
            .await?;

        let request = cuma_core::ports::ExecutionRequest {
            task: task.clone(),
            model: model_id.cloned(),
            prompt,
            workspace: self.workspace.clone(),
            handoff: handoff.cloned(),
            timeout_ms: self.config.limits.task_timeout_secs.saturating_mul(1000),
        };

        let (tx, mut rx) = tokio::sync::mpsc::channel(256);

        // Forward streamed updates onto the event bus while the adapter runs,
        // so a UI sees progress rather than a frozen screen.
        let events = self.events.clone();
        let session = session_id.clone();
        let task_id = task.id.clone();
        let pump = tokio::spawn(async move {
            while let Some(update) = rx.recv().await {
                if let ExecutionUpdate::Text { content } = update {
                    events.publish(Event::task(
                        session.clone(),
                        task_id.clone(),
                        EventKind::AgentOutputReceived { chunk: content },
                    ));
                }
            }
        });

        let deadline = std::time::Duration::from_millis(request.timeout_ms);
        let result = tokio::time::timeout(deadline, adapter.execute(request, tx)).await;

        pump.abort();

        match result {
            Ok(outcome) => outcome,
            // The orchestrator enforces the deadline itself rather than
            // trusting every adapter to honour it. An adapter that hangs must
            // not hang the session.
            Err(_) => Err(MetaAgentError::Timeout {
                operation: format!("agent {agent_id} on task {}", task.id),
                elapsed_ms: self.config.limits.task_timeout_secs.saturating_mul(1000),
            }),
        }
    }

    /// Write one attempt into the usage ledger and the routing history.
    #[allow(clippy::too_many_arguments)]
    async fn record_attempt(
        &self,
        session_id: &SessionId,
        task: &Task,
        attempt_id: &AttemptId,
        agent_id: &AgentId,
        model_id: Option<&ModelId>,
        started_at: chrono::DateTime<chrono::Utc>,
        latency_ms: u64,
        tokens: TokenUsage,
        success: bool,
        failure_class: Option<cuma_core::ErrorClass>,
        retry_count: u32,
    ) {
        let cost = match (self.agents.get(agent_id).await, model_id) {
            (Some(agent), Some(model_id)) => agent
                .model(model_id)
                .map(|m| &m.cost)
                .and_then(|profile| cuma_usage::estimate_cost(profile, tokens)),
            (Some(agent), None) => cuma_usage::estimate_cost(&agent.cost_profile, tokens),
            _ => None,
        };

        self.usage.lock().await.record(UsageRecord {
            attempt_id: attempt_id.clone(),
            session_id: session_id.clone(),
            task_id: task.id.clone(),
            task_type: task.spec.task_type,
            agent_id: agent_id.clone(),
            model_id: model_id.cloned(),
            provider: None,
            started_at,
            latency_ms,
            tokens,
            estimated_cost_usd: cost,
            success,
            failure_class,
            retry_count,
        });

        let mut record = if success {
            OutcomeRecord::success(
                agent_id.clone(),
                model_id.cloned(),
                task.spec.task_type,
                latency_ms,
                tokens.total(),
            )
        } else {
            OutcomeRecord::failure(
                agent_id.clone(),
                model_id.cloned(),
                task.spec.task_type,
                failure_class.unwrap_or(cuma_core::ErrorClass::Unknown),
                latency_ms,
            )
        };
        record.estimated_cost_usd = cost;
        record.retry_count = retry_count;

        self.history.lock().await.record(&record);

        self.events.publish(
            Event::task(
                session_id.clone(),
                task.id.clone(),
                EventKind::UsageRecorded {
                    tokens,
                    estimated_cost_usd: cost,
                },
            )
            .with_attempt(attempt_id.clone()),
        );
    }

    /// Summarize what a failing agent got done, for the agent taking over.
    fn build_handoff(task: &Task, from: &AgentId, reason: &str) -> AgentHandoff {
        let mut handoff =
            AgentHandoff::new(task.id.clone(), task.spec.description.clone(), from.clone(), reason);

        for attempt in &task.attempts {
            for file in &attempt.changed_files {
                handoff.changed_files.push(file.clone());
            }

            if attempt.success {
                handoff.completed_work.push(attempt.output.clone());
            } else if let Some(failure) = &attempt.failure_reason {
                handoff
                    .warnings
                    .push(format!("{} failed: {failure}", attempt.agent_id));
            }
        }

        if handoff.completed_work.is_empty() {
            handoff
                .remaining_work
                .push(task.spec.description.clone());
        }

        handoff
    }

    /// Mark a task failed and announce it.
    fn fail_task(
        &self,
        session_id: &SessionId,
        graph: &mut TaskGraph,
        task_id: &TaskId,
        reason: &str,
    ) {
        if let Some(task) = graph.get_mut(task_id) {
            task.status = TaskStatus::Failed;
        }

        self.events.publish(Event::task(
            session_id.clone(),
            task_id.clone(),
            EventKind::TaskFailed {
                reason: reason.to_owned(),
            },
        ));
    }

    /// Record what worked, so a later session can reuse it.
    async fn remember_success(&self, task: &Task, agent: &AgentId) {
        let Some(store) = &self.memory else {
            return;
        };
        if !store.is_available().await {
            return;
        }

        let content = format!(
            "Task '{}' ({:?}) was completed by {agent}",
            task.spec.description, task.spec.task_type
        );

        if let Err(err) = store.remember(&content, "task_outcome").await {
            tracing::warn!(error = %err, "failed to persist a memory; continuing");
        }
    }

    /// Render a short human-readable session summary.
    fn summarize(graph: &TaskGraph, totals: &cuma_usage::UsageTotals) -> String {
        let completed = graph
            .iter()
            .filter(|t| t.status == TaskStatus::Completed)
            .count();
        let failed = graph.iter().filter(|t| t.status == TaskStatus::Failed).count();
        let skipped = graph.iter().filter(|t| t.status == TaskStatus::Skipped).count();

        let mut summary = format!(
            "{completed}/{} tasks completed",
            graph.len()
        );
        if failed > 0 {
            summary.push_str(&format!(", {failed} failed"));
        }
        if skipped > 0 {
            summary.push_str(&format!(", {skipped} skipped"));
        }
        summary.push_str(&format!(
            " — {} tokens, {}",
            totals.total_tokens(),
            totals.render_cost()
        ));
        summary
    }
}
