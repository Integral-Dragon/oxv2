mod api;
mod artifacts;
mod cx;
mod db;
mod events;
mod git;
mod merge;
mod projections;
mod sse;
mod state;

use anyhow::Result;
use axum::Router;
use chrono::{DateTime, Utc};
use clap::Parser;
use ox_core::events::*;
use ox_core::types::RunnerId;
use rusqlite::Connection;
use std::collections::HashSet;
use std::sync::Arc;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "ox-server")]
struct Args {
    /// Port to listen on.
    #[arg(long, default_value = "4840")]
    port: u16,

    /// Path to SQLite database.
    #[arg(long, default_value = "ox.db")]
    db: String,

    /// Path to the managed repository.
    #[arg(long, default_value = ".")]
    repo: String,

    /// Seconds without a heartbeat before a runner is considered stale.
    #[arg(long, default_value = "60")]
    heartbeat_grace: u64,
}

pub type AppState = Arc<state::ServerState>;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let args = Args::parse();

    let conn = Connection::open(&args.db)?;
    let server_state = state::ServerState::new(conn, &args.repo)?;

    tracing::info!(
        seq = server_state.bus.current_seq(),
        pool = server_state.bus.projections.pool().runners.len(),
        workflows = server_state.workflows.len(),
        "ox-server started"
    );

    // Configure repo for git HTTP serving
    if let Err(e) = git::init_repo_for_http(&server_state.repo_path) {
        tracing::warn!(err = %e, "failed to configure repo for HTTP serving (git operations may not work)");
    }

    let state = Arc::new(server_state);

    // Emit server.ready — signals that migrations and projections are complete.
    // Background tasks and external subscribers (herder, runners) react to this.
    state
        .bus
        .append(EventType::ServerReady, serde_json::json!({}))
        .expect("failed to emit server.ready");

    // Background cx poll loop
    {
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            cx_poll_loop(state).await;
        });
    }

    // Background heartbeat checker
    {
        let state = Arc::clone(&state);
        let grace = args.heartbeat_grace;
        tokio::spawn(async move {
            heartbeat_check_loop(state, grace).await;
        });
    }

    let app = Router::new()
        .merge(api::router())
        .merge(sse::router())
        .merge(git::router())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr = format!("0.0.0.0:{}", args.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("ox-server listening on {addr}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    tracing::info!("ox-server shut down gracefully");
    Ok(())
}

const CX_CURSOR_KEY: &str = "cx_log_cursor";

async fn cx_poll_loop(state: AppState) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        interval.tick().await;

        // Read cursor from db
        let cursor = match state.bus.with_conn(|conn| db::get_kv(conn, CX_CURSOR_KEY)) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(err = %e, "failed to read cx cursor");
                continue;
            }
        };

        let result = match cx::poll_cx_log(&state.repo_path, cursor.as_deref()) {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(err = %e, "cx poll failed (repo may not have .complex/)");
                continue;
            }
        };

        // Append derived events
        for ev in result.events {
            if let Err(e) = state.bus.append(ev.event_type, ev.data) {
                tracing::warn!(err = %e, "failed to append cx event");
            }
        }

        // Update cursor
        if let Some(hash) = result.latest_hash {
            if let Err(e) = state.bus.with_conn(|conn| db::set_kv(conn, CX_CURSOR_KEY, &hash)) {
                tracing::warn!(err = %e, "failed to update cx cursor");
            }
        }
    }
}

async fn heartbeat_check_loop(state: AppState, grace_secs: u64) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Track which runners we've already emitted heartbeat_missed for,
    // so we don't spam the event log.
    let mut already_missed: HashSet<String> = HashSet::new();

    loop {
        interval.tick().await;

        let now = Utc::now();
        let grace = chrono::Duration::seconds(grace_secs as i64);

        // Read all runners and their last_seen + current step from DB
        #[derive(Debug)]
        struct RunnerRow {
            runner_id: String,
            last_seen: String,
            execution_id: Option<String>,
            step: Option<String>,
            attempt: Option<u32>,
        }
        let runners: Vec<RunnerRow> = state.bus.with_conn(|conn| {
            let mut stmt = conn
                .prepare("SELECT runner_id, last_seen, execution_id, step, attempt FROM runners")
                .unwrap();
            stmt.query_map([], |row| {
                Ok(RunnerRow {
                    runner_id: row.get(0)?,
                    last_seen: row.get(1)?,
                    execution_id: row.get(2)?,
                    step: row.get(3)?,
                    attempt: row.get::<_, Option<i64>>(4)?.map(|v| v as u32),
                })
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
        });

        // Check which runners are known to the pool projection (still registered)
        let pool = state.bus.projections.pool();

        for row in &runners {
            // Only check runners that are still in the pool
            if !pool.runners.contains_key(&row.runner_id) {
                already_missed.remove(&row.runner_id);
                continue;
            }

            // Skip if we already emitted for this runner
            if already_missed.contains(&row.runner_id) {
                continue;
            }

            let last_seen = match row.last_seen.parse::<DateTime<Utc>>() {
                Ok(dt) => dt,
                Err(_) => continue,
            };

            // What the pool projection thinks this runner is doing
            let projected = pool.runners.get(&row.runner_id);
            let projected_step = projected.and_then(|r| r.current_step.as_ref());

            // Case 1: heartbeat stale — runner stopped heartbeating entirely
            let stale = now - last_seen > grace;

            // Case 2: mismatch — pool says runner is executing a step,
            // but the runner's heartbeat says it's idle or on a different step
            let mismatch = if let Some(proj_step) = projected_step {
                // Runner should be working on proj_step, check if heartbeat agrees
                match (&row.execution_id, &row.step) {
                    (Some(hb_exec), Some(hb_step)) => {
                        hb_exec != &proj_step.execution_id.0 || hb_step != &proj_step.step
                    }
                    // Heartbeat says idle but projection says executing
                    _ => true,
                }
            } else {
                false // projection says idle, nothing to check
            };

            if stale || mismatch {
                let reason = if stale { "heartbeat stale" } else { "step mismatch" };

                // For mismatch, use the orphaned step from the projection
                let (orphan_exec, orphan_step, orphan_attempt) = if mismatch {
                    if let Some(ps) = projected_step {
                        (Some(ps.execution_id.0.clone()), Some(ps.step.clone()), Some(ps.attempt))
                    } else {
                        (row.execution_id.clone(), row.step.clone(), row.attempt)
                    }
                } else {
                    // For stale, use whatever the last heartbeat reported
                    (row.execution_id.clone(), row.step.clone(), row.attempt)
                };

                tracing::warn!(
                    runner = %row.runner_id,
                    reason,
                    last_seen = %row.last_seen,
                    orphan_exec = ?orphan_exec,
                    orphan_step = ?orphan_step,
                    "runner heartbeat missed"
                );

                let data = RunnerHeartbeatMissedData {
                    runner_id: RunnerId(row.runner_id.clone()),
                    last_seen,
                    grace_period_secs: grace_secs,
                    execution_id: orphan_exec,
                    step: orphan_step,
                    attempt: orphan_attempt,
                };
                if let Err(e) = state.bus.append(
                    EventType::RunnerHeartbeatMissed,
                    serde_json::to_value(data).unwrap(),
                ) {
                    tracing::error!(err = %e, "failed to emit heartbeat_missed");
                }

                already_missed.insert(row.runner_id.clone());
            }
        }

        // Clear runners that are no longer problematic (heartbeat resumed
        // and step matches, or runner no longer in pool)
        already_missed.retain(|id| {
            let still_in_pool = pool.runners.contains_key(id);
            if !still_in_pool { return false; }

            // Check if still stale or mismatched
            runners.iter().any(|r| {
                if &r.runner_id != id { return false; }
                let stale = r.last_seen.parse::<DateTime<Utc>>()
                    .map(|dt| now - dt > grace)
                    .unwrap_or(true);
                if stale { return true; }

                // Check mismatch
                let projected_step = pool.runners.get(id).and_then(|pr| pr.current_step.as_ref());
                if let Some(ps) = projected_step {
                    match (&r.execution_id, &r.step) {
                        (Some(e), Some(s)) => e != &ps.execution_id.0 || s != &ps.step,
                        _ => true,
                    }
                } else {
                    false
                }
            })
        });
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => { tracing::info!("received Ctrl+C, shutting down"); }
        _ = terminate => { tracing::info!("received SIGTERM, shutting down"); }
    }
}
