//! `ox-cx-watcher` — reference cx event source for Ox.
//!
//! Observes a cx-enabled repo, maps cx facts into source events, and
//! posts them to `ox-server`'s `/api/events/ingest` endpoint. The
//! cursor lives on the server — this binary is stateless on disk.

use anyhow::Result;
use clap::Parser;
use ox_cx_watcher::client::WatcherClient;
use std::path::PathBuf;
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
    tracing::info!(
        server = %client.base_url(),
        repo = %args.repo.display(),
        "ox-cx-watcher starting"
    );

    // TODO(slice 3 next commit): wire the tick loop. For now the binary
    // just exits after printing its config — slice 4 launches it via
    // ox-ctl up and exercises the real driver. This main.rs compiles as
    // a smoke test for the crate surface; the interesting logic lives
    // in mapping.rs and client.rs.
    let _ = args.interval_secs;

    Ok(())
}
