//! kube-scheduler — watches unscheduled pods and binds them to nodes
//! (filter/score plugin framework). Drop-in upstream process name.

use clap::Parser;
use rk_scheduler::Scheduler;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "kube-scheduler", about = "Kubernetes scheduler (Rust)")]
struct Cli {
    /// API server URL to schedule against.
    #[arg(long, env = "APISERVER_URL", default_value = "http://127.0.0.1:6443")]
    apiserver: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    tracing::info!("kube-scheduler starting — apiserver={}", cli.apiserver);

    let sched = Scheduler::new(&cli.apiserver);
    if let Err(e) = sched.run().await {
        anyhow::bail!("scheduler failed: {e}");
    }
    Ok(())
}
