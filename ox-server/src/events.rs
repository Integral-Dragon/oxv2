use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use ox_core::events::{EventEnvelope, EventType, SourceEventData};
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
/// `cursor_after` is the new value to persist on commit.
#[derive(Debug, Clone)]
pub struct IngestBatch {
    pub source: String,
    pub cursor_before: Option<String>,
    pub cursor_after: String,
    pub events: Vec<SourceEventData>,
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

        // Replay events to rebuild projections
        let projections = Projections::default();
        let mut max_seq: u64 = 0;

        let events = db::read_events_after(&conn, 0)?;
        for (seq, ts, event_type_str, data_str) in events {
            let event_type: EventType =
                serde_json::from_value(serde_json::Value::String(event_type_str.clone()))
                    .with_context(|| format!("parsing event type: {event_type_str}"))?;
            let data: serde_json::Value = serde_json::from_str(&data_str)
                .with_context(|| format!("parsing event data at seq {seq}"))?;
            let ts = chrono::DateTime::parse_from_rfc3339(&ts)
                .with_context(|| format!("parsing timestamp at seq {seq}"))?
                .with_timezone(&Utc);

            let envelope = EventEnvelope {
                seq: Seq(seq),
                ts,
                event_type,
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
    /// Returns the assigned sequence number.
    pub fn append(
        &self,
        event_type: EventType,
        data: serde_json::Value,
    ) -> Result<EventEnvelope> {
        let mut next_seq = self.next_seq.lock().unwrap();
        let seq = *next_seq;
        let ts = Utc::now();

        let event_type_str = serde_json::to_value(&event_type)
            .unwrap()
            .as_str()
            .unwrap()
            .to_string();
        let data_str = serde_json::to_string(&data)?;

        // Write to SQLite
        {
            let conn = self.conn.lock().unwrap();
            db::append_event(
                &conn,
                seq,
                &ts.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                &event_type_str,
                &data_str,
            )?;
        }

        let envelope = EventEnvelope {
            seq: Seq(seq),
            ts,
            event_type,
            data,
        };

        // Update projections
        self.projections.apply(&envelope);

        // Broadcast redacted version to SSE subscribers
        let _ = self.tx.send(envelope.redacted_for_sse());

        *next_seq = seq + 1;

        Ok(envelope)
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

        for (seq, ts, event_type_str, data_str) in rows {
            let event_type: EventType =
                serde_json::from_value(serde_json::Value::String(event_type_str))?;
            let data: serde_json::Value = serde_json::from_str(&data_str)?;
            let ts = chrono::DateTime::parse_from_rfc3339(&ts)?
                .with_timezone(&Utc);

            let envelope = EventEnvelope {
                seq: Seq(seq),
                ts,
                event_type,
                data,
            };
            // Redact secrets for SSE delivery
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
    pub fn get_watcher_cursor(&self, _source: &str) -> Result<Option<WatcherCursor>> {
        unimplemented!("slice 1: get_watcher_cursor")
    }

    /// List all known watcher cursors. Used by `GET /api/watchers`.
    pub fn list_watcher_cursors(&self) -> Result<Vec<WatcherCursor>> {
        unimplemented!("slice 1: list_watcher_cursors")
    }

    /// Ingest a batch of source events from a watcher. One SQLite
    /// transaction: CAS the cursor, dedup via `ingest_idempotency`,
    /// append `EventType::Source` rows, update `watcher_cursors`.
    /// Projections and SSE broadcast happen after commit.
    pub fn ingest_batch(&self, _batch: IngestBatch) -> Result<IngestResult, IngestError> {
        unimplemented!("slice 1: ingest_batch")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ox_core::events::{RunnerRegisteredData, SecretSetData};
    use ox_core::types::RunnerId;
    use std::collections::HashMap;

    fn test_bus() -> EventBus {
        let conn = Connection::open_in_memory().unwrap();
        EventBus::new(conn).unwrap()
    }

    #[test]
    fn append_assigns_sequential_ids() {
        let bus = test_bus();
        let e1 = bus
            .append(
                EventType::RunnerRegistered,
                serde_json::to_value(RunnerRegisteredData {
                    runner_id: RunnerId("run-0001".into()),
                    environment: "test".into(),
                    labels: HashMap::new(),
                })
                .unwrap(),
            )
            .unwrap();
        let e2 = bus
            .append(
                EventType::RunnerRegistered,
                serde_json::to_value(RunnerRegisteredData {
                    runner_id: RunnerId("run-0002".into()),
                    environment: "test".into(),
                    labels: HashMap::new(),
                })
                .unwrap(),
            )
            .unwrap();

        assert_eq!(e1.seq, Seq(1));
        assert_eq!(e2.seq, Seq(2));
    }

    #[test]
    fn broadcast_receives_events() {
        let bus = test_bus();
        let mut rx = bus.subscribe();

        bus.append(
            EventType::RunnerRegistered,
            serde_json::to_value(RunnerRegisteredData {
                runner_id: RunnerId("run-0001".into()),
                environment: "test".into(),
                labels: HashMap::new(),
            })
            .unwrap(),
        )
        .unwrap();

        let received = rx.try_recv().unwrap();
        assert_eq!(received.seq, Seq(1));
    }

    #[test]
    fn broadcast_redacts_secrets() {
        let bus = test_bus();
        let mut rx = bus.subscribe();

        bus.append(
            EventType::SecretSet,
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
        assert!(!obj.contains_key("value")); // redacted
    }

    #[test]
    fn replay_after() {
        let bus = test_bus();
        for i in 0..5 {
            bus.append(
                EventType::RunnerRegistered,
                serde_json::to_value(RunnerRegisteredData {
                    runner_id: RunnerId(format!("run-{i:04x}")),
                    environment: "test".into(),
                    labels: HashMap::new(),
                })
                .unwrap(),
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
        bus.append(
            EventType::SecretSet,
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

    // ── Slice 1: watcher ingest ────────────────────────────────────────

    fn sample_event(key: &str) -> SourceEventData {
        SourceEventData {
            source: "cx".into(),
            kind: "node.ready".into(),
            subject_id: "Q6cY".into(),
            idempotency_key: key.into(),
            tags: vec!["workflow:code-task".into()],
            data: serde_json::json!({ "title": "test", "state": "ready" }),
        }
    }

    #[test]
    fn get_watcher_cursor_missing_returns_none() {
        let bus = test_bus();
        let got = bus.get_watcher_cursor("cx").unwrap();
        assert!(got.is_none(), "empty db should return None for cursor");
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
        assert_eq!(result.appended, 1, "one new event should be appended");
        assert_eq!(result.deduped, 0, "no dupes on first call");

        let cursor = bus
            .get_watcher_cursor("cx")
            .unwrap()
            .expect("cursor row should exist after first ingest");
        assert_eq!(cursor.source, "cx");
        assert_eq!(cursor.cursor.as_deref(), Some("sha-abc"));
        assert_eq!(cursor.last_error, None);

        // The event log grew by exactly one Source event.
        let tail = bus.replay_after(start_seq).unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].event_type, EventType::Source);
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

        // Replaying the exact same batch should dedupe the event.
        // The watcher would send cursor_before = current cursor on retry.
        let r2 = bus.ingest_batch(batch(Some("sha-abc"), "sha-abc")).unwrap();
        assert_eq!(r2.appended, 0, "replayed event should be deduped");
        assert_eq!(r2.deduped, 1);

        assert_eq!(
            bus.current_seq(),
            seq_after_first,
            "no new events appended on replay"
        );

        let cursor = bus.get_watcher_cursor("cx").unwrap().unwrap();
        assert_eq!(cursor.cursor.as_deref(), Some("sha-abc"));
    }

    #[test]
    fn ingest_batch_rejects_wrong_cursor_before() {
        let bus = test_bus();

        // Seed the cursor at "sha-abc".
        bus.ingest_batch(IngestBatch {
            source: "cx".into(),
            cursor_before: None,
            cursor_after: "sha-abc".into(),
            events: vec![],
        })
        .unwrap();

        let seq_before = bus.current_seq();

        // Now post with a stale cursor_before.
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

        // No new events committed.
        assert_eq!(bus.current_seq(), seq_before);

        // Cursor unchanged.
        let cursor = bus.get_watcher_cursor("cx").unwrap().unwrap();
        assert_eq!(cursor.cursor.as_deref(), Some("sha-abc"));
    }

    #[test]
    fn rebuild_on_new() {
        // Create a bus, append events, then create a new bus from the same db
        let conn = Connection::open("file::memory:?cache=shared")
            .unwrap();
        let bus = EventBus::new(conn).unwrap();
        bus.append(
            EventType::RunnerRegistered,
            serde_json::to_value(RunnerRegisteredData {
                runner_id: RunnerId("run-0001".into()),
                environment: "test".into(),
                labels: HashMap::new(),
            })
            .unwrap(),
        )
        .unwrap();

        // Verify projection was updated
        assert_eq!(bus.projections.pool().runners.len(), 1);

        // New bus from same db should rebuild projections
        let conn2 = Connection::open("file::memory:?cache=shared")
            .unwrap();
        let bus2 = EventBus::new(conn2).unwrap();
        assert_eq!(bus2.projections.pool().runners.len(), 1);
        assert_eq!(bus2.current_seq(), 1);
    }
}
