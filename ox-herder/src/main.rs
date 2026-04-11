mod herder;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "ox-herder")]
struct Args {
    /// ox-server URL.
    #[arg(long, env = "OX_SERVER", default_value = "http://localhost:4840")]
    server: String,

    /// Target pool size. Herder drains runners above this count.
    #[arg(long, default_value = "0")]
    pool_target: usize,

    /// Heartbeat grace period in seconds before re-dispatching.
    /// Falls back to config.toml heartbeat_grace, then 30s.
    #[arg(long)]
    heartbeat_grace: Option<u64>,

    /// Tick interval in seconds for periodic checks.
    #[arg(long, default_value = "5")]
    tick_interval: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let args = Args::parse();

    let search_path = ox_core::config::resolve_search_path(std::path::Path::new("."));
    let config = ox_core::config::load_config(&search_path);
    let heartbeat_grace = args.heartbeat_grace.unwrap_or(config.heartbeat_grace);

    tracing::info!(
        server = %args.server,
        pool_target = args.pool_target,
        heartbeat_grace = heartbeat_grace,
        "ox-herder starting"
    );

    let mut h = herder::Herder::new(
        &args.server,
        args.pool_target,
        heartbeat_grace,
        args.tick_interval,
    );

    tokio::select! {
        result = h.run() => result,
        _ = shutdown_signal() => {
            tracing::info!("ox-herder shut down gracefully");
            Ok(())
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
        _ = ctrl_c => { tracing::info!("received Ctrl+C"); }
        _ = terminate => { tracing::info!("received SIGTERM"); }
    }
}
