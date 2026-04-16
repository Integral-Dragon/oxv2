use anyhow::{Context, Result, bail};
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

    // Reject pre-unification event schemas. The canonical envelope is
    // (seq, ts, source, kind, subject_id, data). If an older layout
    // (with `event_type`) is present, refuse to start rather than
    // silently recreate it alongside.
    let existing_columns = events_table_columns(conn)?;
    if !existing_columns.is_empty() && !existing_columns.iter().any(|c| c == "source") {
        bail!(
            "old ox event schema detected in events table (columns: {}). \
             Run `ox-ctl reset` to wipe the database and start fresh.",
            existing_columns.join(", ")
        );
    }

    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS events (
            seq         INTEGER PRIMARY KEY,
            ts          TEXT    NOT NULL,
            source      TEXT    NOT NULL,
            kind        TEXT    NOT NULL,
            subject_id  TEXT    NOT NULL,
            data        TEXT    NOT NULL
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

        CREATE TABLE IF NOT EXISTS watcher_cursors (
            source      TEXT PRIMARY KEY,
            cursor      TEXT,
            updated_at  TEXT NOT NULL,
            updated_seq INTEGER,
            last_error  TEXT
        );

        CREATE TABLE IF NOT EXISTS ingest_idempotency (
            source          TEXT    NOT NULL,
            idempotency_key TEXT    NOT NULL,
            first_seen_seq  INTEGER NOT NULL,
            first_seen_ts   TEXT    NOT NULL,
            PRIMARY KEY (source, idempotency_key)
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

/// One row from the events table, in the canonical envelope layout.
pub type EventRow = (u64, String, String, String, String, String);

/// Append an event to the log. Returns the assigned seq.
pub fn append_event(
    conn: &Connection,
    seq: u64,
    ts: &str,
    source: &str,
    kind: &str,
    subject_id: &str,
    data: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO events (seq, ts, source, kind, subject_id, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![seq, ts, source, kind, subject_id, data],
    )
    .context("appending event")?;
    Ok(())
}

/// Read all events after a given seq. Each row is
/// `(seq, ts, source, kind, subject_id, data)`.
pub fn read_events_after(conn: &Connection, after_seq: u64) -> Result<Vec<EventRow>> {
    let mut stmt = conn
        .prepare(
            "SELECT seq, ts, source, kind, subject_id, data
             FROM events WHERE seq > ?1 ORDER BY seq ASC",
        )
        .context("preparing event query")?;

    let rows = stmt
        .query_map(rusqlite::params![after_seq], |row| {
            Ok((
                row.get::<_, u64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })
        .context("querying events")?;

    let mut results = vec![];
    for row in rows {
        results.push(row.context("reading event row")?);
    }
    Ok(results)
}

/// Return the column names currently declared on the `events` table,
/// or an empty vec if the table does not yet exist. Used by `migrate`
/// to reject pre-unification schemas.
fn events_table_columns(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare("PRAGMA table_info(events)")
        .context("preparing table_info(events)")?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .context("querying table_info(events)")?;
    let mut cols = Vec::new();
    for row in rows {
        cols.push(row.context("reading table_info row")?);
    }
    Ok(cols)
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

/// Remove a runner from the heartbeat table.
/// Called by pool::drain.
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
        append_event(
            &conn,
            1,
            "2026-01-01T00:00:00Z",
            "ox",
            "test.event",
            "subj-1",
            r#"{"a":1}"#,
        )
        .unwrap();
        append_event(
            &conn,
            2,
            "2026-01-01T00:00:01Z",
            "ox",
            "test.event",
            "subj-2",
            r#"{"a":2}"#,
        )
        .unwrap();

        let events = read_events_after(&conn, 0).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].0, 1);
        assert_eq!(events[1].0, 2);

        let events = read_events_after(&conn, 1).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, 2);
    }

    /// Startup on an old-schema DB must refuse with a clear message.
    #[test]
    fn migrate_rejects_old_event_schema() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE events (
                seq        INTEGER PRIMARY KEY,
                ts         TEXT NOT NULL,
                event_type TEXT NOT NULL,
                data       TEXT NOT NULL
            );",
        )
        .unwrap();

        let err = migrate(&conn).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("old ox event schema"), "got: {msg}");
        assert!(msg.contains("ox-ctl reset"), "got: {msg}");
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
