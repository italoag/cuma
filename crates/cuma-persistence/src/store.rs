//! The runtime store.

use crate::schema;
use cuma_core::error::{MetaAgentError, Result};
use cuma_core::{AgentId, ModelId, SessionId, Task, TaskId, TaskType};
use cuma_router::{AdaptiveStats, RoutingHistory};
use cuma_usage::UsageRecord;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Persistent runtime state.
///
/// A single connection behind a mutex rather than a pool: writes are
/// low-volume (one row per attempt) and SQLite serializes them anyway. A pool
/// would add contention management for a workload that has none.
#[derive(Clone)]
pub struct RuntimeStore {
    connection: Arc<Mutex<Connection>>,
}

impl RuntimeStore {
    /// Open (or create) the database at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|err| {
                MetaAgentError::Persistence(format!("cannot create {}: {err}", parent.display()))
            })?;
        }

        let connection = Connection::open(path).map_err(|err| {
            MetaAgentError::Persistence(format!("cannot open {}: {err}", path.display()))
        })?;

        schema::migrate(&connection).map_err(|err| {
            MetaAgentError::Persistence(format!("schema migration failed: {err}"))
        })?;

        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    /// An in-memory database, for tests.
    pub fn in_memory() -> Result<Self> {
        let connection = Connection::open_in_memory().map_err(|err| {
            MetaAgentError::Persistence(format!("cannot open an in-memory database: {err}"))
        })?;

        schema::migrate(&connection).map_err(|err| {
            MetaAgentError::Persistence(format!("schema migration failed: {err}"))
        })?;

        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    /// Run `operation` against the connection.
    ///
    /// A poisoned mutex is reported as a persistence error rather than a
    /// panic: losing the ability to record history degrades routing quality,
    /// but it must not take the harness down mid-session.
    fn with_connection<T>(
        &self,
        operation: impl FnOnce(&Connection) -> rusqlite::Result<T>,
    ) -> Result<T> {
        let guard = self
            .connection
            .lock()
            .map_err(|_| MetaAgentError::Persistence("the database lock is poisoned".to_owned()))?;

        operation(&guard).map_err(|err| MetaAgentError::Persistence(err.to_string()))
    }

    /// Record the start of a session.
    pub fn begin_session(&self, session_id: &SessionId, goal: &str) -> Result<()> {
        self.with_connection(|connection| {
            connection.execute(
                "INSERT OR REPLACE INTO sessions (id, goal, started_at) VALUES (?1, ?2, ?3)",
                params![session_id.as_str(), goal, chrono::Utc::now().to_rfc3339()],
            )?;
            Ok(())
        })
    }

    /// Record the end of a session.
    pub fn finish_session(
        &self,
        session_id: &SessionId,
        success: bool,
        summary: &str,
    ) -> Result<()> {
        self.with_connection(|connection| {
            connection.execute(
                "UPDATE sessions SET finished_at = ?2, success = ?3, summary = ?4 WHERE id = ?1",
                params![
                    session_id.as_str(),
                    chrono::Utc::now().to_rfc3339(),
                    i64::from(success),
                    summary
                ],
            )?;
            Ok(())
        })
    }

    /// Record or update a task.
    pub fn save_task(&self, session_id: &SessionId, task: &Task) -> Result<()> {
        self.with_connection(|connection| {
            connection.execute(
                "INSERT OR REPLACE INTO tasks
                 (id, session_id, parent_id, description, task_type, status, risk, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    task.id.as_str(),
                    session_id.as_str(),
                    task.parent_id.as_ref().map(TaskId::as_str),
                    task.spec.description,
                    format!("{:?}", task.spec.task_type),
                    format!("{:?}", task.status),
                    format!("{:?}", task.spec.risk),
                    chrono::Utc::now().to_rfc3339(),
                ],
            )?;
            Ok(())
        })
    }

    /// Record one attempt, and fold it into the aggregated history.
    ///
    /// Both writes happen in one transaction: history that disagrees with the
    /// attempts it summarizes would make `cuma usage` and the router
    /// contradict each other.
    pub fn record_attempt(&self, record: &UsageRecord) -> Result<()> {
        self.with_connection(|connection| {
            let transaction = connection.unchecked_transaction()?;

            transaction.execute(
                "INSERT OR REPLACE INTO attempts
                 (id, task_id, session_id, agent_id, model_id, started_at, latency_ms,
                  input_tokens, output_tokens, cached_tokens, tokens_reported, cost_usd,
                  success, failure_class, retry_count)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
                params![
                    record.attempt_id.as_str(),
                    record.task_id.as_str(),
                    record.session_id.as_str(),
                    record.agent_id.as_str(),
                    record.model_id.as_ref().map(ModelId::as_str),
                    record.started_at.to_rfc3339(),
                    record.latency_ms as i64,
                    record.tokens.input as i64,
                    record.tokens.output as i64,
                    record.tokens.cached as i64,
                    i64::from(record.tokens.reported),
                    record.estimated_cost_usd,
                    i64::from(record.success),
                    record.failure_class.map(|c| format!("{c:?}")),
                    i64::from(record.retry_count),
                ],
            )?;

            let model_key = record.model_id.as_ref().map_or("*", ModelId::as_str);
            let task_type = format!("{:?}", record.task_type);

            transaction.execute(
                "INSERT INTO routing_history
                 (agent_id, model_id, task_type, attempts, successes, total_latency_ms, total_tokens, updated_at)
                 VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, ?7)
                 ON CONFLICT(agent_id, model_id, task_type) DO UPDATE SET
                   attempts = attempts + 1,
                   successes = successes + ?4,
                   total_latency_ms = total_latency_ms + ?5,
                   total_tokens = total_tokens + ?6,
                   updated_at = ?7",
                params![
                    record.agent_id.as_str(),
                    model_key,
                    task_type,
                    i64::from(record.success),
                    record.latency_ms as i64,
                    record.tokens.total() as i64,
                    chrono::Utc::now().to_rfc3339(),
                ],
            )?;

            transaction.commit()?;
            Ok(())
        })
    }

    /// Record a routing decision and its explanation.
    ///
    /// The explanation is stored verbatim: being able to answer "why did it
    /// pick that, three days ago?" is the whole point of explainable routing.
    pub fn record_routing_decision(
        &self,
        session_id: &SessionId,
        task_id: &TaskId,
        agent_id: &AgentId,
        model_id: Option<&ModelId>,
        score: f64,
        explanation: &str,
    ) -> Result<()> {
        self.with_connection(|connection| {
            connection.execute(
                "INSERT INTO routing_decisions
                 (session_id, task_id, agent_id, model_id, score, explanation, decided_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params![
                    session_id.as_str(),
                    task_id.as_str(),
                    agent_id.as_str(),
                    model_id.map(ModelId::as_str),
                    score,
                    explanation,
                    chrono::Utc::now().to_rfc3339(),
                ],
            )?;
            Ok(())
        })
    }

    /// The stored explanation for a task's most recent routing decision.
    pub fn routing_explanation(&self, task_id: &TaskId) -> Result<Option<String>> {
        self.with_connection(|connection| {
            connection
                .query_row(
                    "SELECT explanation FROM routing_decisions
                     WHERE task_id = ?1 ORDER BY id DESC LIMIT 1",
                    params![task_id.as_str()],
                    |row| row.get(0),
                )
                .optional()
        })
    }

    /// Rebuild the router's history from the database.
    ///
    /// Called at startup, so a fresh process routes with everything previous
    /// sessions learned rather than starting from a blank slate.
    pub fn load_routing_history(&self) -> Result<RoutingHistory> {
        let rows: Vec<(String, String, String, u32, u32, u64, u64)> =
            self.with_connection(|connection| {
                let mut statement = connection.prepare(
                    "SELECT agent_id, model_id, task_type, attempts, successes,
                            total_latency_ms, total_tokens
                     FROM routing_history",
                )?;

                let mapped = statement.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)? as u32,
                        row.get::<_, i64>(4)? as u32,
                        row.get::<_, i64>(5)? as u64,
                        row.get::<_, i64>(6)? as u64,
                    ))
                })?;

                mapped.collect::<rusqlite::Result<Vec<_>>>()
            })?;

        let mut history = RoutingHistory::new();

        for (agent, model, task_type, attempts, successes, total_latency, total_tokens) in rows {
            let Some(task_type) = parse_task_type(&task_type) else {
                // A task type this build does not recognize came from a newer
                // version. Skipping it loses one bucket; guessing would put
                // the wrong evidence behind a routing decision.
                tracing::debug!(
                    task_type,
                    "skipping a routing-history bucket of unknown type"
                );
                continue;
            };

            let stats = AdaptiveStats {
                attempts,
                successes,
                mean_latency_ms: if attempts == 0 {
                    0
                } else {
                    total_latency / u64::from(attempts)
                },
                mean_tokens: if successes == 0 {
                    0
                } else {
                    total_tokens / u64::from(successes)
                },
            };

            history.restore(
                AgentId::new(agent),
                (model != "*").then(|| ModelId::new(model)),
                task_type,
                stats,
            );
        }

        Ok(history)
    }

    /// Usage totals grouped by agent, across every session.
    ///
    /// Backs the `AGENT USAGE` dashboard. Aggregation happens in SQL rather
    /// than by loading every attempt, so a long-lived database does not make
    /// `cuma usage` slow.
    pub fn usage_by_agent(&self) -> Result<Vec<(String, cuma_usage::UsageTotals)>> {
        self.usage_grouped_by("agent_id")
    }

    /// Usage totals grouped by agent and model.
    pub fn usage_by_model(&self) -> Result<Vec<(String, cuma_usage::UsageTotals)>> {
        self.usage_grouped_by("agent_id || '/' || COALESCE(model_id, '*')")
    }

    fn usage_grouped_by(&self, expression: &str) -> Result<Vec<(String, cuma_usage::UsageTotals)>> {
        let sql = format!(
            "SELECT {expression} AS label,
                    COUNT(*),
                    SUM(success),
                    SUM(input_tokens),
                    SUM(output_tokens),
                    SUM(cached_tokens),
                    SUM(COALESCE(cost_usd, 0.0)),
                    SUM(CASE WHEN cost_usd IS NULL THEN 1 ELSE 0 END),
                    SUM(CASE WHEN tokens_reported = 0 THEN 1 ELSE 0 END),
                    SUM(latency_ms),
                    SUM(retry_count)
             FROM attempts
             GROUP BY label
             ORDER BY label"
        );

        self.with_connection(|connection| {
            let mut statement = connection.prepare(&sql)?;

            let mapped = statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    cuma_usage::UsageTotals {
                        attempts: row.get::<_, i64>(1)? as u32,
                        successes: row.get::<_, i64>(2)? as u32,
                        input_tokens: row.get::<_, i64>(3)? as u64,
                        output_tokens: row.get::<_, i64>(4)? as u64,
                        cached_tokens: row.get::<_, i64>(5)? as u64,
                        estimated_cost_usd: row.get::<_, f64>(6)?,
                        attempts_without_pricing: row.get::<_, i64>(7)? as u32,
                        attempts_with_estimated_tokens: row.get::<_, i64>(8)? as u32,
                        total_latency_ms: row.get::<_, i64>(9)? as u64,
                        retries: row.get::<_, i64>(10)? as u32,
                    },
                ))
            })?;

            mapped.collect::<rusqlite::Result<Vec<_>>>()
        })
    }

    /// Total attempts recorded, across every session.
    pub fn attempt_count(&self) -> Result<u64> {
        self.with_connection(|connection| {
            connection.query_row("SELECT COUNT(*) FROM attempts", [], |row| {
                row.get::<_, i64>(0).map(|n| n as u64)
            })
        })
    }

    /// Total USD recorded, across every session.
    pub fn total_spend_usd(&self) -> Result<f64> {
        self.with_connection(|connection| {
            connection.query_row(
                "SELECT COALESCE(SUM(cost_usd), 0.0) FROM attempts",
                [],
                |row| row.get(0),
            )
        })
    }

    /// How many sessions have been recorded.
    pub fn session_count(&self) -> Result<u64> {
        self.with_connection(|connection| {
            connection.query_row("SELECT COUNT(*) FROM sessions", [], |row| {
                row.get::<_, i64>(0).map(|n| n as u64)
            })
        })
    }
}

/// Parse a task type from its stored `Debug` representation.
fn parse_task_type(raw: &str) -> Option<TaskType> {
    Some(match raw {
        "Inspection" => TaskType::Inspection,
        "Research" => TaskType::Research,
        "Design" => TaskType::Design,
        "Implementation" => TaskType::Implementation,
        "BugFix" => TaskType::BugFix,
        "Refactor" => TaskType::Refactor,
        "Testing" => TaskType::Testing,
        "Validation" => TaskType::Validation,
        "Documentation" => TaskType::Documentation,
        "Review" => TaskType::Review,
        "General" => TaskType::General,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use cuma_core::{AttemptId, TaskSpec, TokenUsage};

    fn record(agent: &str, success: bool, cost: Option<f64>) -> UsageRecord {
        UsageRecord {
            attempt_id: AttemptId::generate(),
            session_id: SessionId::new("s1"),
            task_id: TaskId::new("t1"),
            task_type: TaskType::Implementation,
            agent_id: AgentId::new(agent),
            model_id: Some(ModelId::new("m1")),
            provider: None,
            started_at: chrono::Utc::now(),
            latency_ms: 1000,
            tokens: TokenUsage::reported(100, 50),
            estimated_cost_usd: cost,
            success,
            failure_class: (!success).then_some(cuma_core::ErrorClass::RateLimit),
            retry_count: 0,
        }
    }

    fn store_with_session() -> RuntimeStore {
        let store = RuntimeStore::in_memory().unwrap();
        let session = SessionId::new("s1");
        store.begin_session(&session, "do the thing").unwrap();
        store
            .save_task(
                &session,
                &cuma_core::Task::with_id(
                    TaskId::new("t1"),
                    TaskSpec::new("do it", TaskType::Implementation),
                ),
            )
            .unwrap();
        store
    }

    #[test]
    fn a_fresh_database_migrates_and_is_empty() {
        let store = RuntimeStore::in_memory().unwrap();
        assert_eq!(store.session_count().unwrap(), 0);
        assert_eq!(store.attempt_count().unwrap(), 0);
    }

    #[test]
    fn migration_is_idempotent() {
        let store = RuntimeStore::in_memory().unwrap();
        store.with_connection(schema::migrate).unwrap();
        assert_eq!(store.session_count().unwrap(), 0);
    }

    #[test]
    fn a_session_round_trips() {
        let store = RuntimeStore::in_memory().unwrap();
        let session = SessionId::new("s1");

        store.begin_session(&session, "implement OAuth").unwrap();
        assert_eq!(store.session_count().unwrap(), 1);

        store.finish_session(&session, true, "4/4 done").unwrap();
        assert_eq!(store.session_count().unwrap(), 1);
    }

    #[test]
    fn attempts_are_recorded_with_their_cost() {
        let store = store_with_session();

        store
            .record_attempt(&record("claude", true, Some(0.25)))
            .unwrap();
        store
            .record_attempt(&record("claude", false, Some(0.10)))
            .unwrap();

        assert_eq!(store.attempt_count().unwrap(), 2);
        assert!((store.total_spend_usd().unwrap() - 0.35).abs() < 1e-9);
    }

    #[test]
    fn an_unpriced_attempt_does_not_inflate_total_spend() {
        let store = store_with_session();

        store
            .record_attempt(&record("claude", true, Some(0.25)))
            .unwrap();
        store.record_attempt(&record("claude", true, None)).unwrap();

        assert_eq!(store.attempt_count().unwrap(), 2);
        assert!(
            (store.total_spend_usd().unwrap() - 0.25).abs() < 1e-9,
            "an unknown cost must contribute nothing, not zero-as-a-fact"
        );
    }

    #[test]
    fn routing_history_survives_a_restart() {
        let store = store_with_session();

        for _ in 0..8 {
            store
                .record_attempt(&record("claude", true, Some(0.1)))
                .unwrap();
        }
        for _ in 0..2 {
            store
                .record_attempt(&record("claude", false, Some(0.1)))
                .unwrap();
        }

        // A fresh process reloads what previous sessions learned.
        let history = store.load_routing_history().unwrap();
        let stats = history
            .stats(
                &AgentId::new("claude"),
                Some(&ModelId::new("m1")),
                TaskType::Implementation,
            )
            .unwrap();

        assert_eq!(stats.attempts, 10);
        assert_eq!(stats.successes, 8);
        assert_eq!(stats.success_rate(), Some(0.8));
        assert_eq!(stats.mean_latency_ms, 1000);
    }

    #[test]
    fn a_history_bucket_of_an_unrecognised_task_type_is_skipped_not_guessed_at() {
        let store = RuntimeStore::in_memory().unwrap();

        store
            .with_connection(|connection| {
                connection.execute(
                    "INSERT INTO routing_history
                     (agent_id, model_id, task_type, attempts, successes,
                      total_latency_ms, total_tokens, updated_at)
                     VALUES ('a', '*', 'SomethingFromTheFuture', 5, 5, 100, 100, '2026-01-01T00:00:00Z')",
                    [],
                )
            })
            .unwrap();

        assert!(store.load_routing_history().unwrap().is_empty());
    }

    #[test]
    fn a_routing_explanation_can_be_recovered_long_after_the_decision() {
        let store = store_with_session();
        let session = SessionId::new("s1");
        let task = TaskId::new("t1");

        store
            .record_routing_decision(
                &session,
                &task,
                &AgentId::new("claude"),
                Some(&ModelId::new("m1")),
                0.87,
                "Selected: claude\nReasons:\n  quality 0.94",
            )
            .unwrap();

        let recovered = store.routing_explanation(&task).unwrap().unwrap();
        assert!(recovered.contains("quality 0.94"));
    }

    #[test]
    fn the_most_recent_decision_wins_when_a_task_was_rerouted() {
        let store = store_with_session();
        let session = SessionId::new("s1");
        let task = TaskId::new("t1");

        for agent in ["first", "second"] {
            store
                .record_routing_decision(
                    &session,
                    &task,
                    &AgentId::new(agent),
                    None,
                    0.5,
                    &format!("chose {agent}"),
                )
                .unwrap();
        }

        assert!(
            store
                .routing_explanation(&task)
                .unwrap()
                .unwrap()
                .contains("second")
        );
    }

    #[test]
    fn a_task_with_no_recorded_decision_has_no_explanation() {
        let store = RuntimeStore::in_memory().unwrap();
        assert!(
            store
                .routing_explanation(&TaskId::new("never"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn usage_groups_by_agent_and_by_model() {
        let store = store_with_session();

        store
            .record_attempt(&record("claude", true, Some(0.10)))
            .unwrap();
        store
            .record_attempt(&record("claude", false, Some(0.05)))
            .unwrap();
        store.record_attempt(&record("codex", true, None)).unwrap();

        let by_agent = store.usage_by_agent().unwrap();
        assert_eq!(by_agent.len(), 2);

        let claude = by_agent.iter().find(|(l, _)| l == "claude").unwrap();
        assert_eq!(claude.1.attempts, 2);
        assert_eq!(claude.1.successes, 1);
        assert!((claude.1.estimated_cost_usd - 0.15).abs() < 1e-9);

        let codex = by_agent.iter().find(|(l, _)| l == "codex").unwrap();
        assert_eq!(
            codex.1.attempts_without_pricing, 1,
            "an unpriced attempt must be flagged, not silently costed at zero"
        );
        assert_eq!(codex.1.render_cost(), "unknown");

        assert_eq!(store.usage_by_model().unwrap().len(), 2);
        assert!(
            store
                .usage_by_model()
                .unwrap()
                .iter()
                .any(|(label, _)| label == "claude/m1")
        );
    }

    #[test]
    fn usage_grouping_on_an_empty_database_returns_nothing_rather_than_erroring() {
        let store = RuntimeStore::in_memory().unwrap();
        assert!(store.usage_by_agent().unwrap().is_empty());
    }

    #[test]
    fn deleting_a_session_cascades_to_its_tasks_and_attempts() {
        let store = store_with_session();
        store
            .record_attempt(&record("claude", true, Some(0.1)))
            .unwrap();

        store
            .with_connection(|connection| {
                connection.execute("DELETE FROM sessions WHERE id = 's1'", [])
            })
            .unwrap();

        assert_eq!(
            store.attempt_count().unwrap(),
            0,
            "orphaned attempts would skew every later report"
        );
    }
}
