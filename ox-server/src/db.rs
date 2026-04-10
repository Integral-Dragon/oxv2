use anyhow::{Context, Result};
use rusqlite::Connection;

/// Run all schema migrations.
pub fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = NORMAL;
        PRAGMA foreign_keys = ON;
        PRAGMA busy_timeout = 5000;
        ",
    )
    .context("setting pragmas")?;

    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS events (
            seq        INTEGER PRIMARY KEY,
            ts         TEXT    NOT NULL,
            event_type TEXT    NOT NULL,
            data       TEXT    NOT NULL
        );

        CREATE TABLE IF NOT EXISTS runners (
            runner_id      TEXT PRIMARY KEY,
            last_seen      TEXT NOT NULL,
            execution_id   TEXT,
            step           TEXT,
            attempt        INTEGER
        );

        CREATE TABLE IF NOT EXISTS artifacts_meta (
            execution_id TEXT    NOT NULL,
            step         TEXT    NOT NULL,
            attempt      INTEGER NOT NULL,
            name         TEXT    NOT NULL,
            source       TEXT    NOT NULL,
            streaming    INTEGER NOT NULL,
            status       TEXT    NOT NULL,
            size         INTEGER,
            sha256       TEXT,
            PRIMARY KEY (execution_id, step, attempt, name)
        );

        CREATE TABLE IF NOT EXISTS kv (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS step_logs (
            execution_id TEXT    NOT NULL,
            step         TEXT    NOT NULL,
            attempt      INTEGER NOT NULL,
            offset       INTEGER NOT NULL,
            data         TEXT    NOT NULL,
            PRIMARY KEY (execution_id, step, attempt, offset)
        );

        CREATE TABLE IF NOT EXISTS artifact_chunks (
            execution_id TEXT    NOT NULL,
            step         TEXT    NOT NULL,
            attempt      INTEGER NOT NULL,
            name         TEXT    NOT NULL,
            offset       INTEGER NOT NULL,
            data         BLOB    NOT NULL,
            PRIMARY KEY (execution_id, step, attempt, name, offset)
        );
        ",
    )
    .context("running migrations")?;

    // Incremental migrations — each is idempotent (ignore "duplicate column" errors)
    for col in ["execution_id TEXT", "step TEXT", "attempt INTEGER"] {
        match conn.execute(&format!("ALTER TABLE runners ADD COLUMN {col}"), []) {
            Ok(_) => tracing::info!("migration: added column {col} to runners"),
            Err(e) => tracing::warn!("migration: runners ADD COLUMN {col}: {e}"),
        }
    }

    Ok(())
}

/// Append an event to the log. Returns the assigned seq.
pub fn append_event(
    conn: &Connection,
    seq: u64,
    ts: &str,
    event_type: &str,
    data: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO events (seq, ts, event_type, data) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![seq, ts, event_type, data],
    )
    .context("appending event")?;
    Ok(())
}

/// Read all events after a given seq.
pub fn read_events_after(
    conn: &Connection,
    after_seq: u64,
) -> Result<Vec<(u64, String, String, String)>> {
    let mut stmt = conn
        .prepare("SELECT seq, ts, event_type, data FROM events WHERE seq > ?1 ORDER BY seq ASC")
        .context("preparing event query")?;

    let rows = stmt
        .query_map(rusqlite::params![after_seq], |row| {
            Ok((
                row.get::<_, u64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .context("querying events")?;

    let mut results = vec![];
    for row in rows {
        results.push(row.context("reading event row")?);
    }
    Ok(results)
}

/// Update runner heartbeat timestamp and current step.
pub fn upsert_runner_heartbeat(
    conn: &Connection,
    runner_id: &str,
    ts: &str,
    execution_id: Option<&str>,
    step: Option<&str>,
    attempt: Option<u32>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO runners (runner_id, last_seen, execution_id, step, attempt)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(runner_id) DO UPDATE SET
           last_seen = excluded.last_seen,
           execution_id = excluded.execution_id,
           step = excluded.step,
           attempt = excluded.attempt",
        rusqlite::params![runner_id, ts, execution_id, step, attempt],
    )
    .context("upserting runner heartbeat")?;
    Ok(())
}

/// Get a value from the kv table.
pub fn get_kv(conn: &Connection, key: &str) -> Result<Option<String>> {
    let mut stmt = conn
        .prepare("SELECT value FROM kv WHERE key = ?1")
        .context("preparing kv query")?;
    let mut rows = stmt
        .query_map(rusqlite::params![key], |row| row.get::<_, String>(0))
        .context("querying kv")?;
    match rows.next() {
        Some(row) => Ok(Some(row.context("reading kv row")?)),
        None => Ok(None),
    }
}

/// Set a value in the kv table.
pub fn set_kv(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO kv (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![key, value],
    )
    .context("upserting kv")?;
    Ok(())
}

/// Remove a runner from the heartbeat table.
pub fn remove_runner(conn: &Connection, runner_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM runners WHERE runner_id = ?1",
        rusqlite::params![runner_id],
    )
    .context("removing runner")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn
    }

    #[test]
    fn migrate_idempotent() {
        let conn = test_conn();
        migrate(&conn).unwrap(); // second call is fine
    }

    #[test]
    fn append_and_read() {
        let conn = test_conn();
        append_event(&conn, 1, "2026-01-01T00:00:00Z", "test.event", r#"{"a":1}"#).unwrap();
        append_event(&conn, 2, "2026-01-01T00:00:01Z", "test.event", r#"{"a":2}"#).unwrap();

        let events = read_events_after(&conn, 0).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].0, 1);
        assert_eq!(events[1].0, 2);

        let events = read_events_after(&conn, 1).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, 2);
    }

    #[test]
    fn heartbeat_upsert() {
        let conn = test_conn();
        upsert_runner_heartbeat(&conn, "run-0001", "2026-01-01T00:00:00Z", None, None, None).unwrap();
        upsert_runner_heartbeat(&conn, "run-0001", "2026-01-01T00:00:10Z", Some("exec-1"), Some("propose"), Some(1)).unwrap();

        let ts: String = conn
            .query_row(
                "SELECT last_seen FROM runners WHERE runner_id = 'run-0001'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(ts, "2026-01-01T00:00:10Z");
    }
}
