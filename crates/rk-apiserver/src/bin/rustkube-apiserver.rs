use clap::Parser;
use rk_apiserver::ApiServerConfig;
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "rustkube-apiserver", about = "RustKube API Server")]
struct Cli {
    /// Bind address
    #[arg(long, default_value = "0.0.0.0")]
    bind_addr: String,

    /// HTTPS port
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

    let config = ApiServerConfig {
        bind_addr: cli.bind_addr,
        secure_port: cli.secure_port,
        data_dir: cli.data_dir,
        service_cidr: cli.service_cidr,
        cluster_domain: cli.cluster_domain,
        ..Default::default()
    };

    rk_apiserver::run(config).await
}
