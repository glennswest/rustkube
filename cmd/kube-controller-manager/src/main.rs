//! kube-controller-manager — runs the built-in controllers (Deployment,
//! ReplicaSet, Service, Namespace, Node, Job, CronJob, StatefulSet, DaemonSet,
//! HPA, ...) against the API server. Drop-in upstream process name.

use clap::Parser;
use controller_manager::ControllerManager;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "kube-controller-manager", about = "Kubernetes controller manager (Rust)")]
struct Cli {
    /// API server URL to reconcile against.
    #[arg(long, env = "APISERVER_URL", default_value = "http://127.0.0.1:6443")]
    apiserver: String,

    /// Elect a leader before running controllers (only one instance is active).
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    leader_elect: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    tracing::info!("kube-controller-manager starting — apiserver={}", cli.apiserver);

    let cm = ControllerManager::new(&cli.apiserver).with_leader_election(cli.leader_elect);
    if let Err(e) = cm.run().await {
        anyhow::bail!("controller-manager failed: {e}");
    }
    Ok(())
}
