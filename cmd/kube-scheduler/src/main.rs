//! kube-scheduler — watches unscheduled pods and binds them to nodes
//! (filter/score plugin framework). Drop-in upstream process name.

use clap::Parser;
use scheduler::scheduler::ClientConfig;
use scheduler::Scheduler;
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "kube-scheduler", about = "Kubernetes scheduler (Rust)")]
struct Cli {
    /// API server URL to schedule against.
    #[arg(long, env = "APISERVER_URL", default_value = "http://127.0.0.1:6443")]
    apiserver: String,

    /// Elect a leader before scheduling (so 3 masters don't double-bind).
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    leader_elect: bool,

    /// CA bundle (PEM) to verify the apiserver (HTTPS).
    #[arg(long = "certificate-authority")]
    ca: Option<PathBuf>,

    /// Client certificate (PEM) for mutual TLS.
    #[arg(long = "client-certificate")]
    client_cert: Option<PathBuf>,

    /// Client private key (PEM) for mutual TLS.
    #[arg(long = "client-key")]
    client_key: Option<PathBuf>,

    /// Bearer token to authenticate to the apiserver.
    #[arg(long, env = "APISERVER_TOKEN")]
    token: Option<String>,

    /// Skip apiserver certificate verification.
    #[arg(long = "insecure-skip-tls-verify")]
    insecure: bool,
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

    // Use a TLS/auth client when the server is HTTPS or any auth flag is set.
    let needs_config = cli.apiserver.starts_with("https://")
        || cli.token.is_some()
        || cli.ca.is_some()
        || cli.client_cert.is_some()
        || cli.insecure;

    let sched = if needs_config {
        let cfg = ClientConfig {
            ca_pem: cli.ca.map(std::fs::read).transpose()?,
            client_cert_pem: cli.client_cert.map(std::fs::read).transpose()?,
            client_key_pem: cli.client_key.map(std::fs::read).transpose()?,
            token: cli.token,
            insecure: cli.insecure,
        };
        Scheduler::connect(&cli.apiserver, cfg)?
    } else {
        Scheduler::new(&cli.apiserver)
    }
    .with_leader_election(cli.leader_elect);

    if let Err(e) = sched.run().await {
        anyhow::bail!("scheduler failed: {e}");
    }
    Ok(())
}
