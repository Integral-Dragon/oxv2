mod api;
mod artifacts;
mod cx;
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
use ox_core::events::*;
use rusqlite::Connection;
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

    // Emit server.ready — signals that migrations and projections are complete.
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
        if let Some(hash) = result.latest_hash
            && let Err(e) = state.bus.with_conn(|conn| db::set_kv(conn, CX_CURSOR_KEY, &hash)) {
                tracing::warn!(err = %e, "failed to update cx cursor");
            }
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
