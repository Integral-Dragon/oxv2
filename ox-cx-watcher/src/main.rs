//! `ox-cx-watcher` — reference cx event source for Ox.
//!
//! Observes a cx-enabled repo, maps cx facts into source events, and
//! posts them to `ox-server`'s `/api/events/ingest` endpoint. The
//! cursor lives on the server — this binary is stateless on disk.
//!
//! The loop:
//!
//! 1. On boot, `GET /api/watchers/cx/cursor` to learn where to resume.
//! 2. Every tick: if cursor is `None`, snapshot current cx state and
//!    POST a cold-start batch. Otherwise run `cx log --since <cursor>`,
//!    fetch each touched node's current snapshot, map both node
//!    snapshots and comment entries into `IngestEventData`, and POST
//!    one batch with the old cursor as `cursor_before` and the new
//!    HEAD as `cursor_after`.
//! 3. On 200, update the in-memory cursor.
//! 4. On 409, re-fetch the cursor from the server and retry next tick.
//! 5. On network/5xx error, log and retry next tick with the same batch.

use anyhow::{Context, Result};
use clap::Parser;
use ox_core::events::IngestEventData;
use ox_cx_watcher::client::{IngestBody, IngestOutcome, WatcherClient};
use ox_cx_watcher::{cx, mapping};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::signal;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "ox-cx-watcher", version)]
struct Args {
    /// ox-server base URL — e.g. `http://127.0.0.1:4840`.
    #[arg(long, env = "OX_SERVER")]
    server: String,

    /// Path to the repo containing `.complex/`.
    #[arg(long, default_value = ".")]
    repo: PathBuf,

    /// Poll interval in seconds.
    #[arg(long, default_value = "10")]
    interval_secs: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let args = Args::parse();
    let client = WatcherClient::new(args.server.clone(), "cx");
    let repo = args.repo.clone();
    let interval = Duration::from_secs(args.interval_secs.max(1));

    tracing::info!(
        server = %client.base_url(),
        repo = %repo.display(),
        interval_secs = args.interval_secs,
        "ox-cx-watcher starting"
    );

    // In-memory cursor view. `None` means cold-start on the next tick.
    let mut cursor: Option<String> = client
        .fetch_cursor()
        .await
        .context("initial GET /api/watchers/cx/cursor")?;
    tracing::info!(cursor = ?cursor, "resumed from server cursor");

    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                match tick(&client, &repo, cursor.clone()).await {
                    Ok(TickOutcome::Advanced(new_cursor)) => {
                        cursor = Some(new_cursor);
                    }
                    Ok(TickOutcome::NoChange) => {
                        // Nothing new — liveness refreshed on the server.
                    }
                    Ok(TickOutcome::CasConflict) => {
                        // Another writer won. Re-fetch the cursor and
                        // retry the full diff next tick.
                        tracing::warn!("cursor CAS conflict — re-fetching");
                        match client.fetch_cursor().await {
                            Ok(c) => cursor = c,
                            Err(e) => tracing::warn!(err = %e, "re-fetch cursor failed"),
                        }
                    }
                    Err(e) => {
                        tracing::warn!(err = ?e, "tick failed — cursor unchanged, will retry");
                    }
                }
            }
            _ = shutdown_signal() => {
                tracing::info!("ox-cx-watcher shutting down");
                return Ok(());
            }
        }
    }
}

enum TickOutcome {
    /// A batch was committed and the cursor advanced to this value.
    Advanced(String),
    /// The diff window was empty and no batch was sent. The server's
    /// cursor stays the same; this is functionally a no-op tick.
    NoChange,
    /// The server rejected our CAS. Re-fetch before the next tick.
    CasConflict,
}

/// Run one watcher tick. Pure-ish — takes the current cursor as input
/// and returns the new cursor (or a conflict signal). Does not mutate
/// any shared state.
async fn tick(
    client: &WatcherClient,
    repo: &Path,
    cursor: Option<String>,
) -> Result<TickOutcome> {
    let batch = match cursor.as_deref() {
        None => build_cold_start_batch(repo).await?,
        Some(since) => build_incremental_batch(repo, since).await?,
    };

    let Some(batch) = batch else {
        return Ok(TickOutcome::NoChange);
    };

    let new_cursor = batch.cursor_after.clone();
    match client.post_batch(&batch).await? {
        IngestOutcome::Committed(resp) => {
            tracing::info!(
                appended = resp.appended,
                deduped = resp.deduped,
                cursor = %new_cursor,
                "batch committed"
            );
            Ok(TickOutcome::Advanced(new_cursor))
        }
        IngestOutcome::Conflict { expected, actual } => {
            tracing::warn!(
                expected = ?expected,
                actual = ?actual,
                "ingest_batch rejected — cursor CAS mismatch"
            );
            Ok(TickOutcome::CasConflict)
        }
    }
}

/// Cold start: server has no cursor for cx yet. Snapshot current
/// state via `cx list --json`, emit events for every actionable node,
/// and stamp `cursor_before: None, cursor_after: <HEAD>`.
async fn build_cold_start_batch(repo: &Path) -> Result<Option<IngestBody>> {
    let repo = repo.to_path_buf();
    let (snap, head) = tokio::task::spawn_blocking(move || -> Result<_> {
        let head = cx::current_head(&repo)?;
        let snap = cx::fetch_cx_state(&repo).unwrap_or_default();
        Ok((snap, head))
    })
    .await
    .context("cold-start snapshot task")??;

    let mut events: Vec<IngestEventData> = Vec::new();
    for node in snap.nodes.values() {
        if let Some(ev) = mapping::snapshot_to_event(node, &head) {
            events.push(ev);
        }
    }

    tracing::info!(
        event_count = events.len(),
        head = %head,
        "cold-start snapshot"
    );

    Ok(Some(IngestBody {
        source: mapping::SOURCE.to_string(),
        cursor_before: None,
        cursor_after: head,
        events,
    }))
}

/// Incremental tick: run `cx log --since <cursor>`, fetch the current
/// snapshot for each touched node, and build one batch. Returns
/// `None` when the diff window was empty (no events, no cursor
/// advance).
async fn build_incremental_batch(
    repo: &Path,
    since: &str,
) -> Result<Option<IngestBody>> {
    let repo_owned = repo.to_path_buf();
    let since_owned = since.to_string();

    let (diff, snapshots) = tokio::task::spawn_blocking(move || -> Result<_> {
        let diff = cx::poll_cx_log(&repo_owned, &since_owned)?;
        let mut snapshots = Vec::with_capacity(diff.touched.len());
        for node_id in &diff.touched {
            if let Some(snap) = cx::fetch_node(&repo_owned, node_id) {
                snapshots.push(snap);
            }
        }
        Ok((diff, snapshots))
    })
    .await
    .context("poll_cx_log task")??;

    let Some(latest_hash) = diff.latest_hash else {
        return Ok(None);
    };

    let mut events: Vec<IngestEventData> = Vec::new();
    for snap in &snapshots {
        if let Some(ev) = mapping::snapshot_to_event(snap, &latest_hash) {
            events.push(ev);
        }
    }
    for comment in &diff.comments {
        events.push(mapping::comment_to_event(comment));
    }

    tracing::info!(
        touched = snapshots.len(),
        comments = diff.comments.len(),
        event_count = events.len(),
        cursor_before = %since,
        cursor_after = %latest_hash,
        "incremental diff batch"
    );

    Ok(Some(IngestBody {
        source: mapping::SOURCE.to_string(),
        cursor_before: Some(since.to_string()),
        cursor_after: latest_hash,
        events,
    }))
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c().await.expect("install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}
