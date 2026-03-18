//! rustkube — single-binary control plane.
//!
//! Runs API server + controller manager + scheduler in one process.

use clap::Parser;
use rk_apiserver::ApiServerConfig;
use rk_controllers::ControllerManager;
use rk_scheduler::Scheduler;
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "rustkube", about = "RustKube — K8s-compatible orchestrator")]
struct Cli {
    /// Bind address
    #[arg(long, default_value = "0.0.0.0")]
    bind_addr: String,

    /// API server port
    #[arg(long, default_value_t = 6443)]
    secure_port: u16,

    /// Data directory for embedded store
    #[arg(long, default_value = "/var/lib/rustkube")]
    data_dir: PathBuf,

    /// Service CIDR
    #[arg(long, default_value = "10.96.0.0/12")]
    service_cidr: String,

    /// Cluster domain
    #[arg(long, default_value = "cluster.local")]
    cluster_domain: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let internal_url = format!("http://127.0.0.1:{}", cli.secure_port);

    let config = ApiServerConfig {
        bind_addr: cli.bind_addr,
        secure_port: cli.secure_port,
        data_dir: cli.data_dir,
        service_cidr: cli.service_cidr,
        cluster_domain: cli.cluster_domain,
        ..Default::default()
    };

    // Start API server in the background
    let api_handle = tokio::spawn(async move {
        if let Err(e) = rk_apiserver::run(config).await {
            tracing::error!("API server failed: {e}");
        }
    });

    // Wait for API server to be ready
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    // Start controller manager
    let cm = ControllerManager::new(&internal_url);
    let cm_handle = tokio::spawn(async move {
        if let Err(e) = cm.run().await {
            tracing::error!("Controller manager failed: {e}");
        }
    });

    // Start scheduler
    let sched = Scheduler::new(&internal_url);
    let sched_handle = tokio::spawn(async move {
        if let Err(e) = sched.run().await {
            tracing::error!("Scheduler failed: {e}");
        }
    });

    tracing::info!("RustKube control plane running (apiserver + controllers + scheduler)");

    // Wait for any component to exit
    tokio::select! {
        r = api_handle => { tracing::error!("API server exited: {r:?}"); }
        r = cm_handle => { tracing::error!("Controller manager exited: {r:?}"); }
        r = sched_handle => { tracing::error!("Scheduler exited: {r:?}"); }
    }

    Ok(())
}
