use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct ArtifactMeta {
    pub execution_id: String,
    pub step: String,
    pub attempt: u32,
    pub name: String,
    pub source: String,
    pub streaming: bool,
    pub status: String,
    pub size: Option<u64>,
    pub sha256: Option<String>,
}

/// Declare an artifact (insert metadata with status "pending").
pub fn declare_artifact(
    conn: &Connection,
    execution_id: &str,
    step: &str,
    attempt: u32,
    name: &str,
    source: &str,
    streaming: bool,
) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO artifacts_meta
         (execution_id, step, attempt, name, source, streaming, status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending')",
        rusqlite::params![execution_id, step, attempt, name, source, streaming as i32],
    )
    .context("declaring artifact")?;
    Ok(())
}

/// Write a chunk of artifact content.
pub fn write_chunk(
    conn: &Connection,
    execution_id: &str,
    step: &str,
    attempt: u32,
    name: &str,
    offset: u64,
    data: &[u8],
) -> Result<()> {
    // Update status to streaming
    conn.execute(
        "UPDATE artifacts_meta SET status = 'streaming'
         WHERE execution_id = ?1 AND step = ?2 AND attempt = ?3 AND name = ?4
         AND status = 'pending'",
        rusqlite::params![execution_id, step, attempt, name],
    )?;

    conn.execute(
        "INSERT INTO artifact_chunks
         (execution_id, step, attempt, name, offset, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![execution_id, step, attempt, name, offset, data],
    )
    .context("writing artifact chunk")?;
    Ok(())
}

/// Close an artifact (mark as closed with size and sha256).
pub fn close_artifact(
    conn: &Connection,
    execution_id: &str,
    step: &str,
    attempt: u32,
    name: &str,
    size: u64,
    sha256: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE artifacts_meta SET status = 'closed', size = ?5, sha256 = ?6
         WHERE execution_id = ?1 AND step = ?2 AND attempt = ?3 AND name = ?4",
        rusqlite::params![execution_id, step, attempt, name, size, sha256],
    )
    .context("closing artifact")?;
    Ok(())
}

/// List artifacts for a step.
pub fn list_artifacts(
    conn: &Connection,
    execution_id: &str,
    step: &str,
    attempt: Option<u32>,
) -> Result<Vec<ArtifactMeta>> {
    // If attempt is not specified, find the latest
    let attempt = match attempt {
        Some(a) => a,
        None => {
            let a: Option<u32> = conn
                .query_row(
                    "SELECT MAX(attempt) FROM artifacts_meta
                     WHERE execution_id = ?1 AND step = ?2",
                    rusqlite::params![execution_id, step],
                    |row| row.get(0),
                )
                .ok()
                .flatten();
            a.unwrap_or(1)
        }
    };

    let mut stmt = conn.prepare(
        "SELECT execution_id, step, attempt, name, source, streaming, status, size, sha256
         FROM artifacts_meta
         WHERE execution_id = ?1 AND step = ?2 AND attempt = ?3
         ORDER BY name",
    )?;

    let rows = stmt.query_map(rusqlite::params![execution_id, step, attempt], |row| {
        Ok(ArtifactMeta {
            execution_id: row.get(0)?,
            step: row.get(1)?,
            attempt: row.get(2)?,
            name: row.get(3)?,
            source: row.get(4)?,
            streaming: row.get::<_, i32>(5)? != 0,
            status: row.get(6)?,
            size: row.get(7)?,
            sha256: row.get(8)?,
        })
    })?;

    let mut results = vec![];
    for row in rows {
        results.push(row?);
    }
    Ok(results)
}

/// Fetch full artifact content.
pub fn fetch_artifact(
    conn: &Connection,
    execution_id: &str,
    step: &str,
    attempt: u32,
    name: &str,
) -> Result<Vec<u8>> {
    let mut stmt = conn.prepare(
        "SELECT data FROM artifact_chunks
         WHERE execution_id = ?1 AND step = ?2 AND attempt = ?3 AND name = ?4
         ORDER BY offset ASC",
    )?;

    let rows = stmt.query_map(
        rusqlite::params![execution_id, step, attempt, name],
        |row| row.get::<_, Vec<u8>>(0),
    )?;

    let mut content = vec![];
    for row in rows {
        content.extend_from_slice(&row?);
    }
    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        db::migrate(&conn).unwrap();
        conn
    }

    #[test]
    fn artifact_lifecycle() {
        let conn = test_conn();

        // Declare
        declare_artifact(&conn, "e1", "step1", 1, "log", "log", true).unwrap();

        // Write chunks
        write_chunk(&conn, "e1", "step1", 1, "log", 0, b"hello ").unwrap();
        write_chunk(&conn, "e1", "step1", 1, "log", 6, b"world").unwrap();

        // Close
        close_artifact(&conn, "e1", "step1", 1, "log", 11, "abc123").unwrap();

        // List
        let artifacts = list_artifacts(&conn, "e1", "step1", Some(1)).unwrap();
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].name, "log");
        assert_eq!(artifacts[0].status, "closed");
        assert_eq!(artifacts[0].size, Some(11));

        // Fetch
        let content = fetch_artifact(&conn, "e1", "step1", 1, "log").unwrap();
        assert_eq!(content, b"hello world");
    }
}
