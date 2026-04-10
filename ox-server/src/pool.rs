//! Runner pool management — registration, heartbeats, drain, staleness detection.
//!
//! All runner lifecycle concerns live here. The API handlers are thin
//! wrappers that delegate to Pool methods.

use chrono::{DateTime, Utc};
use ox_core::events::*;
use ox_core::types::RunnerId;
use std::collections::{HashMap, HashSet};

use crate::db;
use crate::events::EventBus;

// ── Public API (called from API handlers) ──────────────────────────

/// Register a new runner. Returns the assigned runner ID.
pub fn register(bus: &EventBus, environment: String, labels: HashMap<String, String>) -> RunnerId {
    let runner_id = RunnerId::generate();

    let data = RunnerRegisteredData {
        runner_id: runner_id.clone(),
        environment,
        labels,
    };
    bus.append(EventType::RunnerRegistered, serde_json::to_value(data).unwrap())
        .unwrap();

    let ts = Utc::now().to_rfc3339();
    bus.with_conn(|conn| {
        db::upsert_runner_heartbeat(conn, &runner_id.0, &ts, None, None, None).unwrap();
    });

    runner_id
}

/// Process a heartbeat from a runner.
pub fn heartbeat(
    bus: &EventBus,
    runner_id: &str,
    execution_id: Option<&str>,
    step: Option<&str>,
    attempt: Option<u32>,
) {
    let ts = Utc::now().to_rfc3339();
    bus.with_conn(|conn| {
        db::upsert_runner_heartbeat(conn, runner_id, &ts, execution_id, step, attempt).unwrap();
    });
}

/// Drain a runner — emit event and remove from DB.
pub fn drain(bus: &EventBus, runner_id: &str, reason: &str) {
    let data = RunnerDrainedData {
        runner_id: RunnerId(runner_id.to_string()),
        reason: reason.to_string(),
    };
    bus.append(EventType::RunnerDrained, serde_json::to_value(data).unwrap())
        .unwrap();

    bus.with_conn(|conn| {
        let _ = db::remove_runner(conn, runner_id);
    });
}

/// Get the pool state enriched with last_seen from the DB.
pub fn state(bus: &EventBus) -> serde_json::Value {
    let pool = bus.projections.pool();
    let heartbeats = read_heartbeats(bus);

    let runners: Vec<serde_json::Value> = pool
        .runners
        .values()
        .map(|r| {
            serde_json::json!({
                "id": r.id.0,
                "environment": r.environment,
                "labels": r.labels,
                "status": format!("{:?}", r.status).to_lowercase(),
                "current_step": r.current_step.as_ref().map(|s| s.to_string()),
                "registered_at": r.registered_at.to_rfc3339(),
                "last_seen": heartbeats.get(&r.id.0).map(|h| &h.last_seen),
            })
        })
        .collect();

    serde_json::json!({ "runners": runners })
}

// ── Background check loop ──────────────────────────────────────────

/// Detect stale runners and step mismatches. Runs as a background task.
pub async fn check_loop(bus: &EventBus, grace_secs: u64) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut already_missed: HashSet<String> = HashSet::new();

    loop {
        interval.tick().await;

        let now = Utc::now();
        let grace = chrono::Duration::seconds(grace_secs as i64);
        let pool = bus.projections.pool();
        let heartbeats = read_heartbeats(bus);

        for (runner_id, hb) in &heartbeats {
            if !pool.runners.contains_key(runner_id) {
                already_missed.remove(runner_id);
                continue;
            }
            if already_missed.contains(runner_id) {
                continue;
            }

            let last_seen = match hb.last_seen.parse::<DateTime<Utc>>() {
                Ok(dt) => dt,
                Err(_) => continue,
            };

            let projected_step = pool.runners.get(runner_id)
                .and_then(|r| r.current_step.as_ref());

            // Case 1: heartbeat stale — runner stopped heartbeating
            let stale = now - last_seen > grace;

            // Case 2: pool says executing, heartbeat says idle or different step.
            // Only flag if the dispatch happened longer ago than the grace period,
            // giving the runner time to receive the dispatch and heartbeat with it.
            let runner_proj = pool.runners.get(runner_id);
            let dispatched_at = runner_proj.and_then(|r| r.dispatched_at);
            let dispatch_old_enough = dispatched_at
                .map(|dt| now - dt > grace)
                .unwrap_or(false);

            let mismatch = if dispatch_old_enough {
                if let Some(ps) = projected_step {
                    match (&hb.execution_id, &hb.step) {
                        (Some(e), Some(s)) => e != &ps.execution_id.0 || s != &ps.step,
                        _ => true,
                    }
                } else {
                    false
                }
            } else {
                false
            };

            if stale || mismatch {
                let reason = if stale { "heartbeat stale" } else { "step mismatch" };

                let (orphan_exec, orphan_step, orphan_attempt) = if mismatch {
                    if let Some(ps) = projected_step {
                        (Some(ps.execution_id.0.clone()), Some(ps.step.clone()), Some(ps.attempt))
                    } else {
                        (hb.execution_id.clone(), hb.step.clone(), hb.attempt)
                    }
                } else {
                    (hb.execution_id.clone(), hb.step.clone(), hb.attempt)
                };

                tracing::warn!(
                    runner = %runner_id,
                    reason,
                    last_seen = %hb.last_seen,
                    orphan_exec = ?orphan_exec,
                    orphan_step = ?orphan_step,
                    "runner heartbeat missed"
                );

                let data = RunnerHeartbeatMissedData {
                    runner_id: RunnerId(runner_id.clone()),
                    last_seen,
                    grace_period_secs: grace_secs,
                    execution_id: orphan_exec,
                    step: orphan_step,
                    attempt: orphan_attempt,
                };
                if let Err(e) = bus.append(
                    EventType::RunnerHeartbeatMissed,
                    serde_json::to_value(data).unwrap(),
                ) {
                    tracing::error!(err = %e, "failed to emit heartbeat_missed");
                }

                already_missed.insert(runner_id.clone());
            }
        }

        // Clear runners that are no longer problematic
        already_missed.retain(|id| {
            if !pool.runners.contains_key(id) { return false; }
            let Some(hb) = heartbeats.get(id) else { return false; };
            let stale = hb.last_seen.parse::<DateTime<Utc>>()
                .map(|dt| now - dt > grace)
                .unwrap_or(true);
            if stale { return true; }
            let projected_step = pool.runners.get(id).and_then(|pr| pr.current_step.as_ref());
            if let Some(ps) = projected_step {
                match (&hb.execution_id, &hb.step) {
                    (Some(e), Some(s)) => e != &ps.execution_id.0 || s != &ps.step,
                    _ => true,
                }
            } else {
                false
            }
        });
    }
}

// ── Internal ───────────────────────────────────────────────────────

struct HeartbeatRow {
    last_seen: String,
    execution_id: Option<String>,
    step: Option<String>,
    attempt: Option<u32>,
}

fn read_heartbeats(bus: &EventBus) -> HashMap<String, HeartbeatRow> {
    bus.with_conn(|conn| {
        let mut stmt = conn
            .prepare("SELECT runner_id, last_seen, execution_id, step, attempt FROM runners")
            .unwrap();
        stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                HeartbeatRow {
                    last_seen: row.get(1)?,
                    execution_id: row.get(2)?,
                    step: row.get(3)?,
                    attempt: row.get::<_, Option<i64>>(4)?.map(|v| v as u32),
                },
            ))
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect()
    })
}
