mod api;
mod db;
mod events;
mod projections;
mod sse;

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
}

pub type AppState = Arc<events::EventBus>;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let args = Args::parse();

    let conn = Connection::open(&args.db)?;
    let bus = Arc::new(events::EventBus::new(conn)?);

    tracing::info!(
        seq = bus.current_seq(),
        pool = bus.projections.pool().runners.len(),
        "projections rebuilt from event log"
    );

    let app = Router::new()
        .merge(api::router())
        .merge(sse::router())
        .layer(TraceLayer::new_for_http())
        .with_state(bus.clone());

    let addr = format!("0.0.0.0:{}", args.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("ox-server listening on {addr}");
    axum::serve(listener, app).await?;

    Ok(())
}
