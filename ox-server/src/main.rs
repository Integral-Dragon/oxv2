mod api;
mod db;
mod events;
mod projections;
mod sse;
mod state;

use anyhow::Result;
use axum::Router;
use clap::Parser;
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

    let state = Arc::new(server_state);

    let app = Router::new()
        .merge(api::router())
        .merge(sse::router())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr = format!("0.0.0.0:{}", args.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("ox-server listening on {addr}");
    axum::serve(listener, app).await?;

    Ok(())
}
