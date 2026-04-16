use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use ox_core::events::{EventEnvelope, IngestEventData};
use ox_core::types::Seq;
use rusqlite::Connection;
use std::sync::Mutex;
use thiserror::Error;
use tokio::sync::broadcast;

use crate::db;
use crate::projections::Projections;

/// Server-side row for a watcher's cursor. One row per source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatcherCursor {
    pub source: String,
    /// Opaque string the watcher last committed. `None` before first write.
    pub cursor: Option<String>,
    pub updated_at: DateTime<Utc>,
    pub updated_seq: Option<u64>,
    pub last_error: Option<String>,
}

/// A watcher's batch ingest request. `cursor_before` is the CAS guard;
/// `cursor_after` is the new value to persist on commit. Each event
/// carries its `kind`, `subject_id`, `idempotency_key`, and free-form
/// `data`; `source` is taken from the batch once and stamped onto
/// every envelope.
#[derive(Debug, Clone)]
pub struct IngestBatch {
    pub source: String,
    pub cursor_before: Option<String>,
    pub cursor_after: String,
    pub events: Vec<IngestEventData>,
}

/// Result of a successful `ingest_batch` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestResult {
    /// Number of events actually appended to the bus, after dedup.
    pub appended: u32,
    /// Number of events dropped as duplicates (same `idempotency_key`).
    pub deduped: u32,
}

/// Errors returned by `ingest_batch`. Mapped to HTTP status codes by
/// the API handler.
#[derive(Debug, Error)]
pub enum IngestError {
    /// `cursor_before` did not match the stored cursor. 409 Conflict.
    #[error("cursor CAS mismatch: expected {expected:?}, stored {actual:?}")]
    CursorConflict {
        expected: Option<String>,
        actual: Option<String>,
    },
    #[error("storage error: {0}")]
    Storage(#[from] anyhow::Error),
}

/// The event bus. Serializes event appends, updates projections, broadcasts to SSE.
pub struct EventBus {
    /// Write-serialized database connection.
    conn: Mutex<Connection>,
    /// Next sequence number to assign.
    next_seq: Mutex<u64>,
    /// Broadcast channel for SSE subscribers.
    tx: broadcast::Sender<EventEnvelope>,
    /// In-memory projections.
    pub projections: Projections,
}

impl EventBus {
    /// Create a new event bus, replaying the event log to rebuild projections.
    pub fn new(conn: Connection) -> Result<Self> {
        db::migrate(&conn)?;

        let (tx, _) = broadcast::channel(1024);

        let projections = Projections::default();
        let mut max_seq: u64 = 0;

        let events = db::read_events_after(&conn, 0)?;
        for (seq, ts, source, kind, subject_id, data_str) in events {
            let data: serde_json::Value = serde_json::from_str(&data_str)
                .with_context(|| format!("parsing event data at seq {seq}"))?;
            let ts = chrono::DateTime::parse_from_rfc3339(&ts)
                .with_context(|| format!("parsing timestamp at seq {seq}"))?
                .with_timezone(&Utc);

            let envelope = EventEnvelope {
                seq: Seq(seq),
                ts,
                source,
                kind,
                subject_id,
                data,
            };
            projections.apply(&envelope);
            max_seq = seq;
        }

        Ok(Self {
            conn: Mutex::new(conn),
            next_seq: Mutex::new(max_seq + 1),
            tx,
            projections,
        })
    }

    /// Append an event to the log, update projections, broadcast to SSE.
    /// Returns the envelope as persisted.
    pub fn append(
        &self,
        source: &str,
        kind: &str,
        subject_id: &str,
        data: serde_json::Value,
    ) -> Result<EventEnvelope> {
        let mut next_seq = self.next_seq.lock().unwrap();
        let seq = *next_seq;
        let ts = Utc::now();
        let data_str = serde_json::to_string(&data)?;

        {
            let conn = self.conn.lock().unwrap();
            db::append_event(
                &conn,
                seq,
                &ts.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                source,
                kind,
                subject_id,
                &data_str,
            )?;
        }

        let envelope = EventEnvelope {
            seq: Seq(seq),
            ts,
            source: source.to_string(),
            kind: kind.to_string(),
            subject_id: subject_id.to_string(),
            data,
        };

        self.projections.apply(&envelope);
        let _ = self.tx.send(envelope.redacted_for_sse());

        *next_seq = seq + 1;

        Ok(envelope)
    }

    /// Convenience wrapper: append an event with `source = "ox"`.
    pub fn append_ox(
        &self,
        kind: &str,
        subject_id: &str,
        data: serde_json::Value,
    ) -> Result<EventEnvelope> {
        self.append(ox_core::events::SOURCE_OX, kind, subject_id, data)
    }

    /// Subscribe to the event broadcast channel.
    pub fn subscribe(&self) -> broadcast::Receiver<EventEnvelope> {
        self.tx.subscribe()
    }

    /// Replay events after a given seq (for SSE reconnection).
    pub fn replay_after(&self, after_seq: u64) -> Result<Vec<EventEnvelope>> {
        let conn = self.conn.lock().unwrap();
        let rows = db::read_events_after(&conn, after_seq)?;
        let mut envelopes = Vec::with_capacity(rows.len());

        for (seq, ts, source, kind, subject_id, data_str) in rows {
            let data: serde_json::Value = serde_json::from_str(&data_str)?;
            let ts = chrono::DateTime::parse_from_rfc3339(&ts)?.with_timezone(&Utc);

            let envelope = EventEnvelope {
                seq: Seq(seq),
                ts,
                source,
                kind,
                subject_id,
                data,
            };
            envelopes.push(envelope.redacted_for_sse());
        }

        Ok(envelopes)
    }

    /// Get direct access to the database connection (for heartbeats and other non-event writes).
    pub fn with_conn<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Connection) -> R,
    {
        let conn = self.conn.lock().unwrap();
        f(&conn)
    }

    /// Current sequence number (the last assigned).
    pub fn current_seq(&self) -> u64 {
        let next = self.next_seq.lock().unwrap();
        if *next == 0 { 0 } else { *next - 1 }
    }

    /// Read a single watcher's cursor row. Returns `None` if the
    /// watcher has never posted a batch — the first-boot cold start.
    pub fn get_watcher_cursor(&self, source: &str) -> Result<Option<WatcherCursor>> {
        let conn = self.conn.lock().unwrap();
        read_watcher_cursor(&conn, source)
    }

    /// List all known watcher cursors. Used by `GET /api/watchers`.
    pub fn list_watcher_cursors(&self) -> Result<Vec<WatcherCursor>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT source, cursor, updated_at, updated_seq, last_error
                 FROM watcher_cursors ORDER BY source ASC",
            )
            .context("preparing watcher_cursors list query")?;
        let rows = stmt
            .query_map([], row_to_watcher_cursor)
            .context("querying watcher_cursors")?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.context("reading watcher_cursors row")?);
        }
        Ok(out)
    }

    /// Ingest a batch of source events from a watcher. One SQLite
    /// transaction: CAS the cursor, dedup via `ingest_idempotency`,
    /// append canonical envelopes, update `watcher_cursors`.
    /// Projections and SSE broadcast happen after commit.
    pub fn ingest_batch(&self, batch: IngestBatch) -> Result<IngestResult, IngestError> {
        let IngestBatch {
            source,
            cursor_before,
            cursor_after,
            events,
        } = batch;

        let mut conn = self.conn.lock().unwrap();
        let mut next_seq = self.next_seq.lock().unwrap();
        // Local candidate — the in-memory counter in `*next_seq` only
        // advances after `tx.commit()` succeeds below. If any write in
        // the transaction fails and SQLite rolls back, the counter
        // stays where it was, so retries don't create gaps in the
        // event log.
        let mut next_seq_candidate: u64 = *next_seq;

        // CAS: compare stored cursor against cursor_before.
        let existing = read_watcher_cursor(&conn, &source).map_err(IngestError::Storage)?;
        let stored_cursor = existing.as_ref().and_then(|r| r.cursor.clone());
        if stored_cursor != cursor_before {
            let ts = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
            let last_error = format!("cas: expected {cursor_before:?}, stored {stored_cursor:?}");
            let _ = conn.execute(
                "INSERT INTO watcher_cursors (source, cursor, updated_at, updated_seq, last_error)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(source) DO UPDATE SET
                   updated_at = excluded.updated_at,
                   last_error = excluded.last_error",
                rusqlite::params![&source, &stored_cursor, &ts, None::<i64>, &last_error],
            );
            return Err(IngestError::CursorConflict {
                expected: cursor_before,
                actual: stored_cursor,
            });
        }

        let tx = conn
            .transaction()
            .context("starting ingest transaction")
            .map_err(IngestError::Storage)?;

        let mut appended_envelopes: Vec<EventEnvelope> = Vec::with_capacity(events.len());
        let mut deduped: u32 = 0;
        let mut last_appended_seq: Option<u64> = existing.as_ref().and_then(|r| r.updated_seq);
        let ts = Utc::now();
        let ts_str = ts.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

        for event in events {
            let seq = next_seq_candidate;
            let inserted = tx
                .execute(
                    "INSERT OR IGNORE INTO ingest_idempotency
                     (source, idempotency_key, first_seen_seq, first_seen_ts)
                     VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![&source, &event.idempotency_key, seq, &ts_str],
                )
                .context("insert ingest_idempotency")
                .map_err(IngestError::Storage)?;
            if inserted == 0 {
                deduped += 1;
                continue;
            }

            let data_str = serde_json::to_string(&event.data)
                .context("encoding ingest event data")
                .map_err(IngestError::Storage)?;

            tx.execute(
                "INSERT INTO events (seq, ts, source, kind, subject_id, data)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    seq,
                    &ts_str,
                    &source,
                    &event.kind,
                    &event.subject_id,
                    &data_str,
                ],
            )
            .context("appending ingest event")
            .map_err(IngestError::Storage)?;

            next_seq_candidate = seq + 1;
            last_appended_seq = Some(seq);

            appended_envelopes.push(EventEnvelope {
                seq: Seq(seq),
                ts,
                source: source.clone(),
                kind: event.kind,
                subject_id: event.subject_id,
                data: event.data,
            });
        }

        // Upsert watcher_cursors with the new cursor, clearing last_error.
        tx.execute(
            "INSERT INTO watcher_cursors
             (source, cursor, updated_at, updated_seq, last_error)
             VALUES (?1, ?2, ?3, ?4, NULL)
             ON CONFLICT(source) DO UPDATE SET
               cursor      = excluded.cursor,
               updated_at  = excluded.updated_at,
               updated_seq = excluded.updated_seq,
               last_error  = NULL",
            rusqlite::params![
                &source,
                &cursor_after,
                &ts_str,
                last_appended_seq.map(|s| s as i64),
            ],
        )
        .context("upserting watcher_cursors")
        .map_err(IngestError::Storage)?;

        tx.commit()
            .context("committing ingest transaction")
            .map_err(IngestError::Storage)?;

        *next_seq = next_seq_candidate;

        drop(conn);
        drop(next_seq);
        for envelope in &appended_envelopes {
            self.projections.apply(envelope);
            let _ = self.tx.send(envelope.redacted_for_sse());
        }

        Ok(IngestResult {
            appended: appended_envelopes.len() as u32,
            deduped,
        })
    }
}

fn read_watcher_cursor(conn: &Connection, source: &str) -> Result<Option<WatcherCursor>> {
    let mut stmt = conn
        .prepare(
            "SELECT source, cursor, updated_at, updated_seq, last_error
             FROM watcher_cursors WHERE source = ?1",
        )
        .context("preparing watcher_cursors query")?;
    let mut rows = stmt
        .query_map(rusqlite::params![source], row_to_watcher_cursor)
        .context("querying watcher_cursors")?;
    match rows.next() {
        Some(row) => Ok(Some(row.context("reading watcher_cursors row")?)),
        None => Ok(None),
    }
}

fn row_to_watcher_cursor(row: &rusqlite::Row<'_>) -> rusqlite::Result<WatcherCursor> {
    let source: String = row.get(0)?;
    let cursor: Option<String> = row.get(1)?;
    let updated_at_str: String = row.get(2)?;
    let updated_seq: Option<i64> = row.get(3)?;
    let last_error: Option<String> = row.get(4)?;
    let updated_at = chrono::DateTime::parse_from_rfc3339(&updated_at_str)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e))
        })?;
    Ok(WatcherCursor {
        source,
        cursor,
        updated_at,
        updated_seq: updated_seq.map(|n| n as u64),
        last_error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ox_core::events::{RunnerRegisteredData, SecretSetData, kinds};
    use ox_core::types::RunnerId;
    use std::collections::HashMap;

    fn test_bus() -> EventBus {
        let conn = Connection::open_in_memory().unwrap();
        EventBus::new(conn).unwrap()
    }

    fn runner_registered(name: &str) -> serde_json::Value {
        serde_json::to_value(RunnerRegisteredData {
            runner_id: RunnerId(name.into()),
            environment: "test".into(),
            labels: HashMap::new(),
        })
        .unwrap()
    }

    #[test]
    fn append_assigns_sequential_ids() {
        let bus = test_bus();
        let e1 = bus
            .append_ox(kinds::RUNNER_REGISTERED, "run-0001", runner_registered("run-0001"))
            .unwrap();
        let e2 = bus
            .append_ox(kinds::RUNNER_REGISTERED, "run-0002", runner_registered("run-0002"))
            .unwrap();
        assert_eq!(e1.seq, Seq(1));
        assert_eq!(e2.seq, Seq(2));
    }

    #[test]
    fn broadcast_receives_events() {
        let bus = test_bus();
        let mut rx = bus.subscribe();
        bus.append_ox(kinds::RUNNER_REGISTERED, "run-0001", runner_registered("run-0001"))
            .unwrap();
        let received = rx.try_recv().unwrap();
        assert_eq!(received.seq, Seq(1));
        assert_eq!(received.source, ox_core::events::SOURCE_OX);
        assert_eq!(received.kind, kinds::RUNNER_REGISTERED);
    }

    #[test]
    fn broadcast_redacts_secrets() {
        let bus = test_bus();
        let mut rx = bus.subscribe();
        bus.append_ox(
            kinds::SECRET_SET,
            "key",
            serde_json::to_value(SecretSetData {
                name: "key".into(),
                value: "secret-value".into(),
            })
            .unwrap(),
        )
        .unwrap();
        let received = rx.try_recv().unwrap();
        let obj = received.data.as_object().unwrap();
        assert!(obj.contains_key("name"));
        assert!(!obj.contains_key("value"));
    }

    #[test]
    fn replay_after() {
        let bus = test_bus();
        for i in 0..5 {
            bus.append_ox(
                kinds::RUNNER_REGISTERED,
                &format!("run-{i:04x}"),
                runner_registered(&format!("run-{i:04x}")),
            )
            .unwrap();
        }
        let replayed = bus.replay_after(3).unwrap();
        assert_eq!(replayed.len(), 2);
        assert_eq!(replayed[0].seq, Seq(4));
        assert_eq!(replayed[1].seq, Seq(5));
    }

    #[test]
    fn replay_redacts_secrets() {
        let bus = test_bus();
        bus.append_ox(
            kinds::SECRET_SET,
            "key",
            serde_json::to_value(SecretSetData {
                name: "key".into(),
                value: "secret".into(),
            })
            .unwrap(),
        )
        .unwrap();
        let replayed = bus.replay_after(0).unwrap();
        let obj = replayed[0].data.as_object().unwrap();
        assert!(!obj.contains_key("value"));
    }

    // ── Watcher ingest ────────────────────────────────────────────────

    fn sample_event(key: &str) -> IngestEventData {
        IngestEventData {
            kind: "node.ready".into(),
            subject_id: "Q6cY".into(),
            idempotency_key: key.into(),
            data: serde_json::json!({
                "title": "test",
                "state": "ready",
                "tags": ["workflow:code-task"]
            }),
        }
    }

    #[test]
    fn get_watcher_cursor_missing_returns_none() {
        let bus = test_bus();
        let got = bus.get_watcher_cursor("cx").unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn ingest_batch_appends_events_and_advances_cursor() {
        let bus = test_bus();
        let start_seq = bus.current_seq();

        let batch = IngestBatch {
            source: "cx".into(),
            cursor_before: None,
            cursor_after: "sha-abc".into(),
            events: vec![sample_event("Q6cY:node.ready:sha-abc")],
        };

        let result = bus.ingest_batch(batch).expect("ingest should succeed");
        assert_eq!(result.appended, 1);
        assert_eq!(result.deduped, 0);

        let cursor = bus.get_watcher_cursor("cx").unwrap().unwrap();
        assert_eq!(cursor.source, "cx");
        assert_eq!(cursor.cursor.as_deref(), Some("sha-abc"));
        assert_eq!(cursor.last_error, None);

        let tail = bus.replay_after(start_seq).unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].source, "cx");
        assert_eq!(tail[0].kind, "node.ready");
        assert_eq!(tail[0].subject_id, "Q6cY");
    }

    #[test]
    fn ingest_batch_is_idempotent_on_replay() {
        let bus = test_bus();

        let batch = |cursor_before: Option<&str>, cursor_after: &str| IngestBatch {
            source: "cx".into(),
            cursor_before: cursor_before.map(str::to_string),
            cursor_after: cursor_after.into(),
            events: vec![sample_event("Q6cY:node.ready:sha-abc")],
        };

        let r1 = bus.ingest_batch(batch(None, "sha-abc")).unwrap();
        assert_eq!(r1.appended, 1);
        let seq_after_first = bus.current_seq();

        let r2 = bus.ingest_batch(batch(Some("sha-abc"), "sha-abc")).unwrap();
        assert_eq!(r2.appended, 0);
        assert_eq!(r2.deduped, 1);
        assert_eq!(bus.current_seq(), seq_after_first);
    }

    #[test]
    fn ingest_batch_keeps_current_seq_in_sync_with_disk() {
        let bus = test_bus();

        bus.ingest_batch(IngestBatch {
            source: "cx".into(),
            cursor_before: None,
            cursor_after: "sha-1".into(),
            events: vec![sample_event("k1")],
        })
        .unwrap();

        bus.ingest_batch(IngestBatch {
            source: "cx".into(),
            cursor_before: Some("sha-1".into()),
            cursor_after: "sha-2".into(),
            events: vec![sample_event("k2"), sample_event("k3")],
        })
        .unwrap();

        let max_seq: u64 = bus.with_conn(|conn| {
            conn.query_row("SELECT MAX(seq) FROM events", [], |row| row.get(0))
                .unwrap()
        });
        assert_eq!(bus.current_seq(), max_seq);
    }

    #[test]
    fn ingest_batch_rejects_wrong_cursor_before() {
        let bus = test_bus();

        bus.ingest_batch(IngestBatch {
            source: "cx".into(),
            cursor_before: None,
            cursor_after: "sha-abc".into(),
            events: vec![],
        })
        .unwrap();

        let seq_before = bus.current_seq();

        let bad = IngestBatch {
            source: "cx".into(),
            cursor_before: Some("sha-WRONG".into()),
            cursor_after: "sha-xyz".into(),
            events: vec![sample_event("should-not-land")],
        };

        match bus.ingest_batch(bad) {
            Err(IngestError::CursorConflict { expected, actual }) => {
                assert_eq!(expected.as_deref(), Some("sha-WRONG"));
                assert_eq!(actual.as_deref(), Some("sha-abc"));
            }
            other => panic!("expected CursorConflict, got {other:?}"),
        }

        assert_eq!(bus.current_seq(), seq_before);
        let cursor = bus.get_watcher_cursor("cx").unwrap().unwrap();
        assert_eq!(cursor.cursor.as_deref(), Some("sha-abc"));
    }

    #[test]
    fn rebuild_on_new() {
        let conn = Connection::open("file::memory:?cache=shared").unwrap();
        let bus = EventBus::new(conn).unwrap();
        bus.append_ox(kinds::RUNNER_REGISTERED, "run-0001", runner_registered("run-0001"))
            .unwrap();
        assert_eq!(bus.projections.pool().runners.len(), 1);

        let conn2 = Connection::open("file::memory:?cache=shared").unwrap();
        let bus2 = EventBus::new(conn2).unwrap();
        assert_eq!(bus2.projections.pool().runners.len(), 1);
        assert_eq!(bus2.current_seq(), 1);
    }
}
