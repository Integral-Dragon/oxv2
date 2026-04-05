use anyhow::{Context, Result};
use chrono::Utc;
use ox_core::events::{EventEnvelope, EventType};
use ox_core::types::Seq;
use rusqlite::Connection;
use std::sync::Mutex;
use tokio::sync::broadcast;

use crate::db;
use crate::projections::Projections;

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
