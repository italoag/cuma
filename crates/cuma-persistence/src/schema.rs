//! Schema definition and migration.

use rusqlite::Connection;

/// The schema version this build expects.
pub const SCHEMA_VERSION: i64 = 1;

/// Create or migrate the schema.
///
/// Migrations run inside a transaction, so a partially applied schema is not a
/// state the harness can end up in.
pub fn migrate(connection: &Connection) -> rusqlite::Result<()> {
    // WAL keeps readers from blocking the writer, which matters because the
    // TUI reads while the orchestrator writes.
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;

    let current: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap_or(0);

    if current >= SCHEMA_VERSION {
        return Ok(());
    }

    connection.execute_batch(
        r"
        BEGIN;

        CREATE TABLE IF NOT EXISTS sessions (
            id           TEXT PRIMARY KEY,
            goal         TEXT NOT NULL,
            started_at   TEXT NOT NULL,
            finished_at  TEXT,
            success      INTEGER,
            summary      TEXT
        );

        CREATE TABLE IF NOT EXISTS tasks (
            id           TEXT PRIMARY KEY,
            session_id   TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            parent_id    TEXT,
            description  TEXT NOT NULL,
            task_type    TEXT NOT NULL,
            status       TEXT NOT NULL,
            risk         TEXT NOT NULL,
            created_at   TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_tasks_session ON tasks(session_id);

        CREATE TABLE IF NOT EXISTS attempts (
            id             TEXT PRIMARY KEY,
            task_id        TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
            session_id     TEXT NOT NULL,
            agent_id       TEXT NOT NULL,
            model_id       TEXT,
            started_at     TEXT NOT NULL,
            latency_ms     INTEGER NOT NULL,
            input_tokens   INTEGER NOT NULL,
            output_tokens  INTEGER NOT NULL,
            cached_tokens  INTEGER NOT NULL,
            tokens_reported INTEGER NOT NULL,
            cost_usd       REAL,
            success        INTEGER NOT NULL,
            failure_class  TEXT,
            retry_count    INTEGER NOT NULL DEFAULT 0
        );

        CREATE INDEX IF NOT EXISTS idx_attempts_task ON attempts(task_id);
        CREATE INDEX IF NOT EXISTS idx_attempts_agent ON attempts(agent_id);

        CREATE TABLE IF NOT EXISTS routing_decisions (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id   TEXT NOT NULL,
            task_id      TEXT NOT NULL,
            agent_id     TEXT NOT NULL,
            model_id     TEXT,
            score        REAL NOT NULL,
            explanation  TEXT NOT NULL,
            decided_at   TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_routing_task ON routing_decisions(task_id);

        -- Aggregated history, keyed the same way the router buckets it.
        CREATE TABLE IF NOT EXISTS routing_history (
            agent_id     TEXT NOT NULL,
            model_id     TEXT NOT NULL DEFAULT '*',
            task_type    TEXT NOT NULL,
            attempts     INTEGER NOT NULL DEFAULT 0,
            successes    INTEGER NOT NULL DEFAULT 0,
            total_latency_ms INTEGER NOT NULL DEFAULT 0,
            total_tokens INTEGER NOT NULL DEFAULT 0,
            updated_at   TEXT NOT NULL,
            PRIMARY KEY (agent_id, model_id, task_type)
        );

        CREATE TABLE IF NOT EXISTS agent_health (
            agent_id     TEXT PRIMARY KEY,
            state        TEXT NOT NULL,
            consecutive_failures INTEGER NOT NULL DEFAULT 0,
            last_latency_ms INTEGER,
            last_success TEXT,
            last_error   TEXT,
            updated_at   TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS installed_skills (
            id           TEXT PRIMARY KEY,
            name         TEXT NOT NULL,
            version      TEXT NOT NULL,
            source       TEXT NOT NULL,
            trust        TEXT NOT NULL,
            capabilities TEXT NOT NULL,
            installed_at TEXT NOT NULL
        );

        COMMIT;
        ",
    )?;

    connection.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}
