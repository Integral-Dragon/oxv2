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
    #[arg(long, default_value = "30")]
    heartbeat_grace: u64,

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

    tracing::info!(
        server = %args.server,
        pool_target = args.pool_target,
        heartbeat_grace = args.heartbeat_grace,
        "ox-herder starting"
    );

    let mut h = herder::Herder::new(
        &args.server,
        args.pool_target,
        args.heartbeat_grace,
        args.tick_interval,
    );

    h.run().await
}
