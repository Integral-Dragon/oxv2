mod proxy;
mod pty;
mod runner;
mod scan;
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

    /// Directory for step workspaces. Defaults to `/work`, which in the
    /// seguro VM layout is a host-backed virtiofs share (see
    /// `docs/vm-layout.md`). Avoid tmpfs paths like `/tmp/...` — cargo
    /// targets run into multi-GB and will exhaust guest RAM.
    #[arg(long, env = "OX_WORKSPACE_DIR", default_value = "/work")]
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

    tokio::select! {
        result = r.run() => result,
        _ = shutdown_signal() => {
            tracing::info!("ox-runner shut down gracefully");
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
