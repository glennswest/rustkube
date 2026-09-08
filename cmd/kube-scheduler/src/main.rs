//! kube-scheduler — watches unscheduled pods and binds them to nodes
//! (filter/score plugin framework). Drop-in upstream process name.

use apimachinery::startup;
use clap::Parser;
use scheduler::scheduler::ClientConfig;
use scheduler::Scheduler;
use std::path::PathBuf;
use std::time::Duration;
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

    /// File containing a bearer token.
    #[arg(long)]
    token_file: Option<PathBuf>,

    /// Skip apiserver certificate verification.
    #[arg(long = "insecure-skip-tls-verify")]
    insecure: bool,

    /// Seconds to wait for a credential file to be written, and for the
    /// apiserver to start serving, before giving up. The whole control plane
    /// starts at once, so these are normally races, not failures.
    #[arg(long = "startup-timeout", env = "STARTUP_TIMEOUT", default_value_t = 120)]
    startup_timeout: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    if let Err(e) = run().await {
        // Say the last words through tracing too: the supervisor captures
        // stderr, but an exit whose reason is only in `Error:` on the way out
        // reads as an unexplained status 1 in the console.
        tracing::error!("kube-scheduler exiting: {e:#}");
        return Err(e);
    }
    Ok(())
}

async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    tracing::info!("kube-scheduler starting — apiserver={}", cli.apiserver);
    let wait = Duration::from_secs(cli.startup_timeout);

    // Use a TLS/auth client when the server is HTTPS or any auth flag is set.
    let needs_config = cli.apiserver.starts_with("https://")
        || cli.token.is_some()
        || cli.token_file.is_some()
        || cli.ca.is_some()
        || cli.client_cert.is_some()
        || cli.insecure;

    let sched = if needs_config {
        // Credentials are written by whatever bootstraps the node, often while
        // this process is already running: wait for each file to exist and be
        // complete rather than exiting and being restarted into working.
        let token = match (cli.token, &cli.token_file) {
            (Some(t), _) => Some(t),
            (None, Some(f)) => {
                Some(startup::token_file(f, "apiserver bearer token", wait).await?)
            }
            _ => None,
        };
        let cfg = ClientConfig {
            ca_pem: read_pem(cli.ca.as_deref(), "apiserver CA bundle", wait).await?,
            client_cert_pem: read_pem(cli.client_cert.as_deref(), "client certificate", wait)
                .await?,
            client_key_pem: read_pem(cli.client_key.as_deref(), "client key", wait).await?,
            token,
            insecure: cli.insecure,
        };
        Scheduler::connect(&cli.apiserver, cfg)?
    } else {
        Scheduler::new(&cli.apiserver)
    }
    .with_leader_election(cli.leader_elect)
    .with_startup_timeout(wait);

    if let Err(e) = sched.run().await {
        anyhow::bail!("scheduler failed: {e}");
    }
    Ok(())
}

/// Read an optional PEM file, waiting for it to be written.
async fn read_pem(
    path: Option<&std::path::Path>,
    what: &str,
    wait: Duration,
) -> anyhow::Result<Option<Vec<u8>>> {
    match path {
        Some(p) => Ok(Some(startup::pem_file(p, what, wait).await?)),
        None => Ok(None),
    }
}
