mod api;
mod artifacts;
mod db;
mod events;
mod git;
mod merge;
mod pool;
mod projections;
mod pty_relay;
mod sse;
mod state;

use anyhow::Result;
use axum::Router;
use clap::Parser;
use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::Arc;

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}
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
    /// Falls back to config.toml heartbeat_grace, then 60s.
    #[arg(long)]
    heartbeat_grace: Option<u64>,
}

pub type AppState = Arc<state::ServerState>;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let args = Args::parse();

    // First-run: extract embedded defaults to ~/.ox/defaults (no-op on
    // subsequent runs if the fingerprint matches). Must happen before
    // ServerState::new, which resolves the config search path.
    if let Some(home) = dirs_home() {
        let ox_home = home.join(".ox");
        match ox_core::config::ensure_defaults_extracted(&ox_home) {
            Ok(path) => tracing::debug!(path = %path.display(), "defaults ready"),
            Err(e) => {
                tracing::warn!(err = %e, "failed to extract embedded defaults; using search path as-is");
            }
        }
    }

    let conn = Connection::open(&args.db)?;
    let server_state = state::ServerState::new(conn, &args.repo)?;

    tracing::info!(
        seq = server_state.bus.current_seq(),
        pool = server_state.bus.projections.pool().runners.len(),
        workflows = server_state.hot.load().workflows.len(),
        "ox-server started"
    );

    // Configure repo for git HTTP serving
    if let Err(e) = git::init_repo_for_http(&server_state.repo_path) {
        tracing::warn!(err = %e, "failed to configure repo for HTTP serving (git operations may not work)");
    }

    let state = Arc::new(server_state);

    // Reconcile runners resurrected by event-log replay. Any runner in
    // the projection whose heartbeat has lapsed past `grace` belongs to
    // a prior server lifetime that didn't drain cleanly (crash,
    // SIGKILL, or an old `ox-ctl down` that skipped drain). Emit
    // synthetic drains so the projection reflects reality before
    // `server.ready` fires.
    let grace = args.heartbeat_grace.unwrap_or(state.hot.load().config.heartbeat_grace);
    pool::sweep_orphans(&state.bus, chrono::Utc::now(), grace);

    // Close any step attempts orphaned by the runner sweep above, or
    // by prior-lifetime drift (runner reassigned to a new exec without
    // a terminal event for the old one). Order matters: this runs
    // after sweep_orphans so the drains it emits are already folded
    // into the pool projection.
    pool::sweep_orphan_attempts(&state.bus);

    // Emit server.ready — signals that migrations and projections are complete.
    state
        .bus
        .append_ox(ox_core::events::kinds::SERVER_READY, "", serde_json::json!({}))
        .expect("failed to emit server.ready");

    // Event ingestion is out-of-process — watcher binaries launched
    // by `ox-ctl up` POST source events to `/api/events/ingest`. The
    // server has no source-specific polling of any kind.

    // SIGHUP config reload
    #[cfg(unix)]
    {
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            use tokio::signal::unix::SignalKind;
            let mut sig = tokio::signal::unix::signal(SignalKind::hangup())
                .expect("failed to install SIGHUP handler");
            loop {
                sig.recv().await;
                tracing::info!("received SIGHUP, reloading config");
                match state::HotConfig::load(&state.repo_path) {
                    Ok(new) => {
                        tracing::info!(
                            workflows = new.workflows.len(),
                            runtimes = new.runtimes.len(),
                            personas = new.personas.len(),
                            triggers = new.triggers.len(),
                            "config reloaded"
                        );
                        state.hot.store(Arc::new(new));
                    }
                    Err(e) => {
                        tracing::error!(err = %e, "config reload failed, keeping old config");
                    }
                }
            }
        });
    }

    // Background heartbeat check loop
    {
        let state = Arc::clone(&state);
        let grace = args.heartbeat_grace.unwrap_or(state.hot.load().config.heartbeat_grace);
        tokio::spawn(async move {
            pool::check_loop(&state.bus, grace).await;
        });
    }

    let app = Router::new()
        .merge(api::router())
        .merge(sse::router())
        .merge(git::router())
        .merge(pty_relay::router())
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
