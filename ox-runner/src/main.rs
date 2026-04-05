mod runner;
mod socket;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "ox-runner")]
struct Args {
    /// ox-server URL.
    #[arg(long, env = "OX_SERVER", default_value = "http://localhost:4840")]
    server: String,

    /// Environment label reported on registration.
    #[arg(long, env = "OX_ENVIRONMENT", default_value = "local")]
    environment: String,

    /// Directory for step workspaces.
    #[arg(long, env = "OX_WORKSPACE_DIR", default_value = "/tmp/ox-work")]
    workspace_dir: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let args = Args::parse();

    tracing::info!(
        server = %args.server,
        environment = %args.environment,
        workspace_dir = %args.workspace_dir,
        "ox-runner starting"
    );

    let mut r = runner::Runner::new(&args.server, &args.environment, &args.workspace_dir);
    r.run().await
}
