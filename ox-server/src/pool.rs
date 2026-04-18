//! Runner pool management — registration, heartbeats, drain, staleness detection.
//!
//! All runner lifecycle concerns live here. The API handlers are thin
//! wrappers that delegate to Pool methods.

use chrono::{DateTime, Utc};
use ox_core::events::*;
use ox_core::types::{ExecutionId, RunnerId};
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
    bus.append_ox(
        kinds::RUNNER_REGISTERED,
        &runner_id.0,
        serde_json::to_value(data).unwrap(),
    )
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
    bus.append_ox(
        kinds::RUNNER_DRAINED,
        runner_id,
        serde_json::to_value(data).unwrap(),
    )
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

// ── Startup orphan sweep ───────────────────────────────────────────

/// One-shot startup reconciliation — for each non-drained runner in the
/// replayed pool projection, if there is no heartbeat row or the last
/// heartbeat is older than `grace_secs`, emit a synthetic
/// `RUNNER_DRAINED` with reason `"orphan at startup"`.
///
/// This catches ghosts left by crashes, SIGKILLs, or shutdowns that
/// bypassed the drain API (e.g. an older `ox-ctl down` that just sent
/// SIGTERM). Without this sweep, `RUNNER_REGISTERED` events from a
/// previous server lifetime are resurrected by event replay and
/// permanently inflate the pool projection.
///
/// Intended to run exactly once at server startup, after projections
/// are built and before `server.ready` is emitted.
pub fn sweep_orphans(bus: &EventBus, now: DateTime<Utc>, grace_secs: u64) {
    let grace = chrono::Duration::seconds(grace_secs as i64);
    let heartbeats = read_heartbeats(bus);

    let orphans: Vec<String> = {
        let pool = bus.projections.pool();
        pool.runners
            .values()
            .filter(|r| r.status != crate::projections::RunnerStatus::Drained)
            .filter(|r| match heartbeats.get(&r.id.0) {
                None => true,
                Some(hb) => match hb.last_seen.parse::<DateTime<Utc>>() {
                    Ok(last_seen) => now - last_seen > grace,
                    Err(_) => true,
                },
            })
            .map(|r| r.id.0.clone())
            .collect()
    };

    for id in orphans {
        drain(bus, &id, "orphan at startup");
    }
}

// ── Background check loop ──────────────────────────────────────────

/// Detect stale runners and step mismatches. Runs as a background task.
pub async fn check_loop(bus: &EventBus, grace_secs: u64) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut already_missed: HashSet<String> = HashSet::new();
    let mut already_timed_out: HashSet<String> = HashSet::new(); // keyed by "exec_id/step/attempt"

    loop {
        interval.tick().await;
        check_tick(bus, &mut already_missed, &mut already_timed_out, Utc::now(), grace_secs);
    }
}

/// One iteration of `check_loop` — pure enough to test. Reads pool
/// state and heartbeats from `bus`, emits events for stale/recovered
/// runners and timed-out steps, and updates the dedup sets in place.
pub(crate) fn check_tick(
    bus: &EventBus,
    already_missed: &mut HashSet<String>,
    already_timed_out: &mut HashSet<String>,
    now: DateTime<Utc>,
    grace_secs: u64,
) {
    {
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
                if let Err(e) = bus.append_ox(
                    kinds::RUNNER_HEARTBEAT_MISSED,
                    runner_id,
                    serde_json::to_value(data).unwrap(),
                ) {
                    tracing::error!(err = %e, "failed to emit heartbeat_missed");
                }

                already_missed.insert(runner_id.clone());
            }
        }

        // Clear runners that are no longer problematic, emitting
        // `runner.recovered` for each one that went stale → healthy
        // (the exit transition symmetric to `runner.heartbeat_missed`).
        let mut recovered: Vec<(String, DateTime<Utc>)> = Vec::new();
        already_missed.retain(|id| {
            if !pool.runners.contains_key(id) { return false; }
            let Some(hb) = heartbeats.get(id) else { return false; };
            let last_seen = hb.last_seen.parse::<DateTime<Utc>>().ok();
            let stale = last_seen.map(|dt| now - dt > grace).unwrap_or(true);
            if stale { return true; }
            let projected_step = pool.runners.get(id).and_then(|pr| pr.current_step.as_ref());
            let still_mismatched = if let Some(ps) = projected_step {
                match (&hb.execution_id, &hb.step) {
                    (Some(e), Some(s)) => e != &ps.execution_id.0 || s != &ps.step,
                    _ => true,
                }
            } else {
                false
            };
            if still_mismatched { return true; }
            if let Some(ls) = last_seen {
                recovered.push((id.clone(), ls));
            }
            false
        });

        for (id, last_seen) in recovered {
            tracing::info!(runner = %id, "runner recovered");
            let data = RunnerRecoveredData {
                runner_id: RunnerId(id.clone()),
                last_seen,
            };
            if let Err(e) = bus.append_ox(
                kinds::RUNNER_RECOVERED,
                &id,
                serde_json::to_value(data).unwrap(),
            ) {
                tracing::error!(err = %e, "failed to emit runner.recovered");
            }
        }

        // Step timeout detection — independent of runner health.
        // A runner can be healthy but a step can exceed its timeout.
        for runner in pool.runners.values() {
            let (Some(step_id), Some(dispatched), Some(timeout_secs)) =
                (&runner.current_step, runner.dispatched_at, runner.step_timeout_secs)
            else {
                continue;
            };

            let key = format!("{}/{}/{}", step_id.execution_id.0, step_id.step, step_id.attempt);
            if already_timed_out.contains(&key) {
                continue;
            }

            let timeout = chrono::Duration::seconds(timeout_secs as i64);
            if now - dispatched > timeout {
                tracing::warn!(
                    runner = %runner.id,
                    exec = %step_id.execution_id,
                    step = %step_id.step,
                    attempt = step_id.attempt,
                    timeout_secs,
                    "step timeout exceeded"
                );

                let subject = step_id.execution_id.0.clone();
                let data = StepTimeoutData {
                    execution_id: step_id.execution_id.clone(),
                    step: step_id.step.clone(),
                    attempt: step_id.attempt,
                    timeout_secs,
                    runner_id: runner.id.clone(),
                };
                if let Err(e) = bus.append_ox(
                    kinds::STEP_TIMEOUT,
                    &subject,
                    serde_json::to_value(data).unwrap(),
                ) {
                    tracing::error!(err = %e, "failed to emit step.timeout");
                }

                already_timed_out.insert(key);
            }
        }

        // Clear timed-out steps that are no longer in-flight
        already_timed_out.retain(|key| {
            pool.runners.values().any(|r| {
                r.current_step.as_ref().is_some_and(|s| {
                    format!("{}/{}/{}", s.execution_id.0, s.step, s.attempt) == *key
                })
            })
        });
    }
}

// ── Orphan-attempt sweep ───────────────────────────────────────────

/// A step attempt whose runner binding is stale — the runner has been
/// reassigned, drained, or re-registered without a terminal event
/// (`step.confirmed`, `step.failed`, `step.timeout`) closing the attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // wired into check_tick + startup sweep in later slices
pub(crate) struct OrphanAttempt {
    pub execution_id: ExecutionId,
    pub step: String,
    pub attempt: u32,
    pub runner_id: RunnerId,
    pub reason: OrphanReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // wired into check_tick + startup sweep in later slices
pub(crate) enum OrphanReason {
    /// Runner is no longer in the pool projection, or is Drained.
    Gone,
    /// Runner is present and has a different `(exec, step, attempt)` as
    /// its `current_step` — the prior attempt was silently overwritten.
    Reassigned,
    /// Runner is present but `current_step` is `None` — the runner was
    /// re-registered or its state cleared without a terminal event for
    /// the prior attempt.
    Cleared,
}

/// Scan for step attempts whose runner binding is stale. Pure function —
/// reads `pool` and `execs` projections, returns orphan descriptors
/// without mutating anything. The caller emits `step.failed` events.
///
/// The invariant being checked: a step attempt with `status ∈
/// {Dispatched, Running}` must have a runner whose projected
/// `current_step` points back to that same `(exec, step, attempt)`.
/// Any other pool state for that runner means the attempt has been
/// abandoned and no terminal event has closed it.
#[allow(dead_code)] // wired into check_tick + startup sweep in later slices
pub(crate) fn scan_orphan_attempts(
    pool: &crate::projections::PoolState,
    execs: &crate::projections::ExecutionsState,
) -> Vec<OrphanAttempt> {
    use crate::projections::{ExecutionStatus, RunnerStatus, StepStatus};

    let mut orphans = Vec::new();

    for exec in execs.executions.values() {
        if exec.status != ExecutionStatus::Running {
            continue;
        }

        for att in &exec.attempts {
            if !matches!(att.status, StepStatus::Dispatched | StepStatus::Running) {
                continue;
            }
            let Some(runner_id) = att.runner_id.as_ref() else {
                continue;
            };

            let reason = match pool.runners.get(&runner_id.0) {
                None => OrphanReason::Gone,
                Some(r) if r.status == RunnerStatus::Drained => OrphanReason::Gone,
                Some(r) => match r.current_step.as_ref() {
                    None => OrphanReason::Cleared,
                    Some(cs) => {
                        if cs.execution_id == exec.id
                            && cs.step == att.step
                            && cs.attempt == att.attempt
                        {
                            continue; // healthy — runner's current_step points back to us
                        }
                        OrphanReason::Reassigned
                    }
                },
            };

            orphans.push(OrphanAttempt {
                execution_id: exec.id.clone(),
                step: att.step.clone(),
                attempt: att.attempt,
                runner_id: runner_id.clone(),
                reason,
            });
        }
    }

    orphans
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use ox_core::events::kinds;
    use rusqlite::Connection;

    fn test_bus() -> EventBus {
        let conn = Connection::open_in_memory().unwrap();
        EventBus::new(conn).unwrap()
    }

    fn set_heartbeat(bus: &EventBus, runner_id: &str, last_seen: DateTime<Utc>) {
        bus.with_conn(|conn| {
            db::upsert_runner_heartbeat(
                conn,
                runner_id,
                &last_seen.to_rfc3339(),
                None,
                None,
                None,
            )
            .unwrap();
        });
    }

    /// A runner that was flagged `runner.heartbeat_missed` must emit a
    /// matching `runner.recovered` event once its heartbeat catches up
    /// within the grace period. Without this exit-transition event,
    /// herder-style projections that track runner liveness have no way
    /// to observe recovery and can strand the runner permanently (see
    /// sKFs).
    #[test]
    fn emits_runner_recovered_when_stale_runner_heartbeats_again() {
        let bus = test_bus();
        let runner = register(&bus, "test-env".into(), HashMap::new());
        let runner_id = runner.0.as_str();
        let grace_secs = 60;

        // 1. Runner is stale (last heartbeat 2 minutes ago).
        let now = Utc::now();
        set_heartbeat(&bus, runner_id, now - chrono::Duration::seconds(120));

        let mut missed: HashSet<String> = HashSet::new();
        let mut timed_out: HashSet<String> = HashSet::new();
        check_tick(&bus, &mut missed, &mut timed_out, now, grace_secs);
        assert!(
            missed.contains(runner_id),
            "runner should be flagged as stale after tick 1"
        );

        // 2. Runner recovers — fresh heartbeat.
        set_heartbeat(&bus, runner_id, now);
        check_tick(&bus, &mut missed, &mut timed_out, now, grace_secs);
        assert!(
            !missed.contains(runner_id),
            "runner should be cleared from missed set after tick 2"
        );

        // 3. A runner.recovered event must have been appended to the log.
        let events = bus.replay_after(0).unwrap();
        let recovered_events: Vec<_> = events
            .iter()
            .filter(|e| e.kind == kinds::RUNNER_RECOVERED && e.subject_id == runner_id)
            .collect();
        assert_eq!(
            recovered_events.len(),
            1,
            "exactly one runner.recovered event expected, got {} events total: {:?}",
            recovered_events.len(),
            events.iter().map(|e| &e.kind).collect::<Vec<_>>()
        );
    }

    // ── sweep_orphans ──────────────────────────────────────────────────

    fn drained_events_for(bus: &EventBus, runner_id: &str) -> usize {
        bus.replay_after(0)
            .unwrap()
            .iter()
            .filter(|e| e.kind == kinds::RUNNER_DRAINED && e.subject_id == runner_id)
            .count()
    }

    /// Event-replay resurrection scenario: a runner was registered in a
    /// prior server lifetime and killed without draining. On startup
    /// the projection rebuilds with that runner present but its
    /// heartbeat row is old. The sweep must emit RUNNER_DRAINED so the
    /// projection ends boot reflecting reality.
    #[test]
    fn sweep_orphans_drains_runner_with_stale_heartbeat() {
        let bus = test_bus();
        let runner = register(&bus, "test-env".into(), HashMap::new());
        let runner_id = runner.0.as_str();
        let grace_secs = 60;

        let now = Utc::now();
        set_heartbeat(&bus, runner_id, now - chrono::Duration::seconds(120));

        sweep_orphans(&bus, now, grace_secs);

        assert_eq!(
            drained_events_for(&bus, runner_id),
            1,
            "expected one runner.drained event for the orphan"
        );
        let pool = bus.projections.pool();
        assert_eq!(
            pool.runners.get(runner_id).map(|r| &r.status),
            Some(&crate::projections::RunnerStatus::Drained),
            "orphan should be marked Drained in the projection"
        );
    }

    /// A runner whose heartbeat is within the grace window is alive —
    /// it must not be drained. Covers the common case where the server
    /// simply restarted quickly and its runners' heartbeats never
    /// lapsed.
    #[test]
    fn sweep_orphans_skips_fresh_runner() {
        let bus = test_bus();
        let runner = register(&bus, "test-env".into(), HashMap::new());
        let runner_id = runner.0.as_str();
        let grace_secs = 60;

        let now = Utc::now();
        set_heartbeat(&bus, runner_id, now - chrono::Duration::seconds(5));

        sweep_orphans(&bus, now, grace_secs);

        assert_eq!(
            drained_events_for(&bus, runner_id),
            0,
            "fresh runner must not be drained"
        );
    }

    // ── scan_orphan_attempts tests ─────────────────────────────────

    /// Helpers for building pool/execution projection state inline —
    /// the scan is pure, so tests construct state directly rather than
    /// driving events through a bus.
    fn pool_with(runners: Vec<crate::projections::RunnerState>) -> crate::projections::PoolState {
        let mut state = crate::projections::PoolState::default();
        for r in runners {
            state.runners.insert(r.id.0.clone(), r);
        }
        state
    }

    fn runner(
        id: &str,
        status: crate::projections::RunnerStatus,
        current_step: Option<(&str, &str, u32)>,
    ) -> crate::projections::RunnerState {
        crate::projections::RunnerState {
            id: RunnerId(id.into()),
            environment: "test".into(),
            labels: HashMap::new(),
            status,
            current_step: current_step.map(|(e, s, a)| ox_core::types::StepAttemptId {
                execution_id: ExecutionId(e.into()),
                step: s.into(),
                attempt: a,
            }),
            registered_at: Utc::now(),
            dispatched_at: None,
            step_timeout_secs: None,
        }
    }

    fn execs_with(execs: Vec<crate::projections::ExecutionState>) -> crate::projections::ExecutionsState {
        let mut state = crate::projections::ExecutionsState::default();
        for e in execs {
            state.executions.insert(e.id.0.clone(), e);
        }
        state
    }

    fn exec_running(
        id: &str,
        attempts: Vec<crate::projections::StepAttemptState>,
    ) -> crate::projections::ExecutionState {
        crate::projections::ExecutionState {
            id: ExecutionId(id.into()),
            workflow: "wf".into(),
            status: crate::projections::ExecutionStatus::Running,
            vars: HashMap::new(),
            origin: Default::default(),
            attempts,
            current_step: None,
            current_attempt: 0,
            visit_counts: HashMap::new(),
            created_at: Utc::now(),
        }
    }

    fn attempt(
        step: &str,
        attempt: u32,
        runner_id: Option<&str>,
        status: crate::projections::StepStatus,
    ) -> crate::projections::StepAttemptState {
        crate::projections::StepAttemptState {
            step: step.into(),
            attempt,
            runner_id: runner_id.map(|r| RunnerId(r.into())),
            status,
            output: None,
            signals: vec![],
            error: None,
            transition: None,
            connect_addr: None,
            started_at: Utc::now(),
            completed_at: None,
        }
    }

    /// Baseline: attempt's runner agrees with pool projection → not an
    /// orphan. This is the only case that must NOT emit step.failed.
    #[test]
    fn scan_finds_no_orphan_when_runner_current_step_matches_attempt() {
        let pool = pool_with(vec![runner(
            "run-0000",
            crate::projections::RunnerStatus::Executing,
            Some(("e-1", "implement", 1)),
        )]);
        let execs = execs_with(vec![exec_running(
            "e-1",
            vec![attempt(
                "implement",
                1,
                Some("run-0000"),
                crate::projections::StepStatus::Running,
            )],
        )]);

        let orphans = scan_orphan_attempts(&pool, &execs);

        assert_eq!(orphans, vec![], "healthy attempt must not be flagged");
    }

    /// Bug scenario A: after a runner is drained, its in-flight attempt
    /// has no matching current_step. The sweep must flag it.
    #[test]
    fn scan_flags_attempt_when_runner_is_drained() {
        let pool = pool_with(vec![runner(
            "run-0000",
            crate::projections::RunnerStatus::Drained,
            None,
        )]);
        let execs = execs_with(vec![exec_running(
            "e-1",
            vec![attempt(
                "implement",
                1,
                Some("run-0000"),
                crate::projections::StepStatus::Running,
            )],
        )]);

        let orphans = scan_orphan_attempts(&pool, &execs);

        assert_eq!(
            orphans,
            vec![OrphanAttempt {
                execution_id: ExecutionId("e-1".into()),
                step: "implement".into(),
                attempt: 1,
                runner_id: RunnerId("run-0000".into()),
                reason: OrphanReason::Gone,
            }]
        );
    }

    /// If the runner is missing from the pool projection entirely
    /// (e.g. event log was truncated or a test fixture left an orphan),
    /// the attempt is still orphaned — treat it as RunnerGone.
    #[test]
    fn scan_flags_attempt_when_runner_not_in_projection() {
        let pool = pool_with(vec![]);
        let execs = execs_with(vec![exec_running(
            "e-1",
            vec![attempt(
                "implement",
                1,
                Some("run-0000"),
                crate::projections::StepStatus::Running,
            )],
        )]);

        let orphans = scan_orphan_attempts(&pool, &execs);

        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].reason, OrphanReason::Gone);
    }

    /// Bug scenario B (the one that motivated this sweep): a runner is
    /// dispatched to a new (exec, step, attempt) while a previous
    /// attempt still claims it. The older attempt must be flagged.
    #[test]
    fn scan_flags_attempt_when_runner_reassigned_to_different_step() {
        let pool = pool_with(vec![runner(
            "run-0000",
            crate::projections::RunnerStatus::Executing,
            Some(("e-2", "implement", 1)), // runner is on exec-2 now
        )]);
        let execs = execs_with(vec![
            exec_running(
                "e-1",
                vec![attempt(
                    "implement",
                    1,
                    Some("run-0000"), // but attempt still thinks it's on run-0000
                    crate::projections::StepStatus::Running,
                )],
            ),
            exec_running(
                "e-2",
                vec![attempt(
                    "implement",
                    1,
                    Some("run-0000"),
                    crate::projections::StepStatus::Running,
                )],
            ),
        ]);

        let orphans = scan_orphan_attempts(&pool, &execs);

        assert_eq!(orphans.len(), 1, "only e-1's attempt is orphaned");
        assert_eq!(orphans[0].execution_id, ExecutionId("e-1".into()));
        assert_eq!(orphans[0].reason, OrphanReason::Reassigned);
    }

    /// Bug scenario C: runner was re-registered (possibly after a
    /// server restart), clearing `current_step` to None while the
    /// attempt still believes it's running on this runner.
    #[test]
    fn scan_flags_attempt_when_runner_current_step_cleared() {
        let pool = pool_with(vec![runner(
            "run-0000",
            crate::projections::RunnerStatus::Idle,
            None, // re-registered, no current step
        )]);
        let execs = execs_with(vec![exec_running(
            "e-1",
            vec![attempt(
                "implement",
                1,
                Some("run-0000"),
                crate::projections::StepStatus::Running,
            )],
        )]);

        let orphans = scan_orphan_attempts(&pool, &execs);

        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].reason, OrphanReason::Cleared);
    }

    /// Terminal-state attempts must never be flagged, even when the
    /// runner state looks wrong — the attempt is already closed.
    #[test]
    fn scan_ignores_attempts_in_terminal_states() {
        let pool = pool_with(vec![runner(
            "run-0000",
            crate::projections::RunnerStatus::Drained,
            None,
        )]);
        let execs = execs_with(vec![exec_running(
            "e-1",
            vec![
                attempt("propose", 1, Some("run-0000"), crate::projections::StepStatus::Confirmed),
                attempt("review", 1, Some("run-0000"), crate::projections::StepStatus::Failed),
                attempt("build", 1, Some("run-0000"), crate::projections::StepStatus::Done),
            ],
        )]);

        let orphans = scan_orphan_attempts(&pool, &execs);

        assert_eq!(orphans, vec![], "terminal attempts must never be flagged");
    }

    /// Non-running executions (completed/escalated/cancelled) may carry
    /// historical attempts with non-terminal status in rare edge cases;
    /// the sweep should skip them — only live executions matter.
    #[test]
    fn scan_ignores_attempts_in_non_running_executions() {
        let pool = pool_with(vec![runner(
            "run-0000",
            crate::projections::RunnerStatus::Drained,
            None,
        )]);
        let mut exec = exec_running(
            "e-1",
            vec![attempt(
                "implement",
                1,
                Some("run-0000"),
                crate::projections::StepStatus::Running,
            )],
        );
        exec.status = crate::projections::ExecutionStatus::Escalated;
        let execs = execs_with(vec![exec]);

        let orphans = scan_orphan_attempts(&pool, &execs);

        assert_eq!(orphans, vec![]);
    }

    /// Idempotency shape: Dispatched status behaves the same as Running
    /// for sweep purposes — both are pre-terminal.
    #[test]
    fn scan_flags_dispatched_attempts_same_as_running() {
        let pool = pool_with(vec![runner(
            "run-0000",
            crate::projections::RunnerStatus::Drained,
            None,
        )]);
        let execs = execs_with(vec![exec_running(
            "e-1",
            vec![attempt(
                "implement",
                1,
                Some("run-0000"),
                crate::projections::StepStatus::Dispatched,
            )],
        )]);

        let orphans = scan_orphan_attempts(&pool, &execs);

        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].reason, OrphanReason::Gone);
    }

    // ── check_tick orphan-attempt emission ─────────────────────────

    /// Helper: emit execution.created so the execution appears in
    /// projections with status=Running.
    fn seed_execution(bus: &EventBus, exec_id: &str) {
        use ox_core::events::ExecutionCreatedData;
        let data = ExecutionCreatedData {
            execution_id: ExecutionId(exec_id.into()),
            workflow: "test-wf".into(),
            trigger: "manual".into(),
            vars: HashMap::new(),
            origin: Default::default(),
            start_step: None,
        };
        bus.append_ox(
            kinds::EXECUTION_CREATED,
            exec_id,
            serde_json::to_value(data).unwrap(),
        )
        .unwrap();
    }

    /// Helper: emit step.dispatched + step.running to put an attempt
    /// into Running state bound to the given runner.
    fn seed_running_attempt(bus: &EventBus, exec_id: &str, step: &str, attempt: u32, runner_id: &str) {
        use ox_core::events::{StepDispatchedData, StepRunningData};
        bus.append_ox(
            kinds::STEP_DISPATCHED,
            exec_id,
            serde_json::to_value(StepDispatchedData {
                execution_id: ExecutionId(exec_id.into()),
                step: step.into(),
                attempt,
                runner_id: RunnerId(runner_id.into()),
                secret_refs: vec![],
                runtime: serde_json::json!({}),
                workspace: serde_json::json!({}),
                artifacts: vec![],
            })
            .unwrap(),
        )
        .unwrap();
        bus.append_ox(
            kinds::STEP_RUNNING,
            exec_id,
            serde_json::to_value(StepRunningData {
                execution_id: ExecutionId(exec_id.into()),
                step: step.into(),
                attempt,
                connect_addr: None,
            })
            .unwrap(),
        )
        .unwrap();
    }

    fn failed_events_for(bus: &EventBus, exec_id: &str, step: &str) -> Vec<serde_json::Value> {
        bus.replay_after(0)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == kinds::STEP_FAILED && e.subject_id == exec_id)
            .filter_map(|e| {
                let d = e.data.as_object()?;
                if d.get("step")?.as_str()? == step {
                    Some(e.data)
                } else {
                    None
                }
            })
            .collect()
    }

    /// The live-bug reproduction: runner gets dispatched to exec-B
    /// while exec-A's attempt still claims it. After a check_tick, the
    /// orphaned exec-A attempt must be closed with step.failed, and no
    /// further ticks should re-emit.
    #[test]
    fn check_tick_emits_step_failed_when_runner_reassigned() {
        let bus = test_bus();
        let runner = register(&bus, "test-env".into(), HashMap::new());
        let runner_id = runner.0.as_str();
        let now = Utc::now();
        set_heartbeat(&bus, runner_id, now);

        seed_execution(&bus, "e-A");
        seed_running_attempt(&bus, "e-A", "implement", 1, runner_id);

        // Runner is now reassigned to exec-B without a terminal event for e-A.
        seed_execution(&bus, "e-B");
        seed_running_attempt(&bus, "e-B", "implement", 1, runner_id);

        let mut missed: HashSet<String> = HashSet::new();
        let mut timed_out: HashSet<String> = HashSet::new();
        check_tick(&bus, &mut missed, &mut timed_out, now, 60);

        let failed = failed_events_for(&bus, "e-A", "implement");
        assert_eq!(failed.len(), 1, "exactly one step.failed for orphaned e-A");
        assert!(
            failed[0]["error"].as_str().unwrap().contains("orphan"),
            "error message should mention orphan: got {:?}",
            failed[0]["error"]
        );

        let execs = bus.projections.executions();
        let att = execs
            .executions
            .get("e-A")
            .unwrap()
            .attempts
            .iter()
            .find(|a| a.step == "implement" && a.attempt == 1)
            .unwrap();
        assert_eq!(
            att.status,
            crate::projections::StepStatus::Failed,
            "e-A attempt must be Failed after the tick"
        );

        // Idempotency: a second tick must not re-emit because the
        // attempt is now terminal.
        check_tick(&bus, &mut missed, &mut timed_out, now, 60);
        let failed_again = failed_events_for(&bus, "e-A", "implement");
        assert_eq!(failed_again.len(), 1, "must not re-emit on repeat tick");
    }

    /// When the runner is drained mid-step, the next tick must close
    /// the orphan. This covers the manual-drain path.
    #[test]
    fn check_tick_emits_step_failed_when_runner_drained_mid_step() {
        let bus = test_bus();
        let runner = register(&bus, "test-env".into(), HashMap::new());
        let runner_id = runner.0.as_str();
        let now = Utc::now();
        set_heartbeat(&bus, runner_id, now);

        seed_execution(&bus, "e-A");
        seed_running_attempt(&bus, "e-A", "implement", 1, runner_id);

        drain(&bus, runner_id, "manual");

        let mut missed: HashSet<String> = HashSet::new();
        let mut timed_out: HashSet<String> = HashSet::new();
        check_tick(&bus, &mut missed, &mut timed_out, now, 60);

        let failed = failed_events_for(&bus, "e-A", "implement");
        assert_eq!(failed.len(), 1, "drain-orphaned attempt must be closed");
    }

    /// Healthy runner + attempt must not be spuriously flagged.
    #[test]
    fn check_tick_does_not_emit_step_failed_for_healthy_attempt() {
        let bus = test_bus();
        let runner = register(&bus, "test-env".into(), HashMap::new());
        let runner_id = runner.0.as_str();
        let now = Utc::now();
        set_heartbeat(&bus, runner_id, now);

        seed_execution(&bus, "e-A");
        seed_running_attempt(&bus, "e-A", "implement", 1, runner_id);

        let mut missed: HashSet<String> = HashSet::new();
        let mut timed_out: HashSet<String> = HashSet::new();
        check_tick(&bus, &mut missed, &mut timed_out, now, 60);

        let failed = failed_events_for(&bus, "e-A", "implement");
        assert_eq!(failed.len(), 0, "healthy attempt must not be flagged");
    }

    /// Idempotency: running the sweep when a runner is already drained
    /// must not emit a second `runner.drained`. Guards against startup
    /// loops or multiple invocations inflating the event log.
    #[test]
    fn sweep_orphans_skips_already_drained_runner() {
        let bus = test_bus();
        let runner = register(&bus, "test-env".into(), HashMap::new());
        let runner_id = runner.0.as_str();
        let grace_secs = 60;

        drain(&bus, runner_id, "manual");
        let before = drained_events_for(&bus, runner_id);

        let now = Utc::now();
        sweep_orphans(&bus, now, grace_secs);

        assert_eq!(
            drained_events_for(&bus, runner_id),
            before,
            "already-drained runner should not accrue another drain event"
        );
    }
}
