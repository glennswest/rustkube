//! kube-controller-manager — runs the built-in controllers (Deployment,
//! ReplicaSet, Service, Namespace, Node, Job, CronJob, StatefulSet, DaemonSet,
//! HPA, ...) against the API server. Drop-in upstream process name.

use clap::Parser;
use controller_manager::{ClientConfig, ControllerManager};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "kube-controller-manager", about = "Kubernetes controller manager (Rust)")]
struct Cli {
    /// API server URL (http:// or https://).
    #[arg(long, env = "APISERVER_URL", default_value = "http://127.0.0.1:6443")]
    apiserver: String,

    /// Elect a leader before running controllers (only one instance is active).
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    leader_elect: bool,

    /// Bearer token for authenticating to the API server.
    #[arg(long, env = "APISERVER_TOKEN")]
    token: Option<String>,

    /// File containing a bearer token.
    #[arg(long)]
    token_file: Option<PathBuf>,

    /// CA bundle (PEM) to verify the API server certificate.
    #[arg(long = "certificate-authority")]
    ca: Option<PathBuf>,

    /// Client certificate (PEM) for mutual TLS.
    #[arg(long = "client-certificate")]
    client_cert: Option<PathBuf>,

    /// Client private key (PEM) for mutual TLS.
    #[arg(long = "client-key")]
    client_key: Option<PathBuf>,

    /// Skip API server certificate verification (dev only).
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
    tracing::info!("kube-controller-manager starting — apiserver={}", cli.apiserver);

    // Use a TLS/auth client when the server is HTTPS or any auth flag is set.
    let needs_config = cli.apiserver.starts_with("https://")
        || cli.token.is_some()
        || cli.token_file.is_some()
        || cli.ca.is_some()
        || cli.client_cert.is_some()
        || cli.insecure;

    let cm = if needs_config {
        let token = match (cli.token, &cli.token_file) {
            (Some(t), _) => Some(t),
            (None, Some(f)) => Some(std::fs::read_to_string(f)?.trim().to_string()),
            _ => None,
        };
        let cfg = ClientConfig {
            ca_pem: cli.ca.map(std::fs::read).transpose()?,
            client_cert_pem: cli.client_cert.map(std::fs::read).transpose()?,
            client_key_pem: cli.client_key.map(std::fs::read).transpose()?,
            token,
            insecure: cli.insecure,
        };
        ControllerManager::connect(&cli.apiserver, cfg)?
    } else {
        ControllerManager::new(&cli.apiserver)
    }
    .with_leader_election(cli.leader_elect);

    if let Err(e) = cm.run().await {
        anyhow::bail!("controller-manager failed: {e}");
    }
    Ok(())
}
