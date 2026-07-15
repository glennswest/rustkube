use clap::Parser;
use apiserver::ApiServerConfig;
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "kube-apiserver", about = "Kubernetes API server (Rust, external fastetcd store)")]
struct Cli {
    /// Bind address
    #[arg(long, default_value = "0.0.0.0")]
    bind_addr: String,

    /// HTTPS port
    #[arg(long, default_value_t = 6443)]
    secure_port: u16,

    /// External etcd/fastetcd endpoints (comma-separated or repeated),
    /// e.g. https://127.0.0.1:2379
    #[arg(long = "etcd-servers", value_delimiter = ',', env = "ETCD_SERVERS", required = true)]
    etcd_servers: Vec<String>,

    /// CA certificate (PEM) to verify the etcd/fastetcd server
    #[arg(long, env = "ETCD_CACERT")]
    etcd_cacert: Option<PathBuf>,

    /// Client certificate (PEM) for mutual TLS to etcd/fastetcd
    #[arg(long, env = "ETCD_CERT")]
    etcd_cert: Option<PathBuf>,

    /// Client private key (PEM) for mutual TLS to etcd/fastetcd
    #[arg(long, env = "ETCD_KEY")]
    etcd_key: Option<PathBuf>,

    /// Serve HTTPS with an auto-generated self-signed cert (dev/bootstrap)
    #[arg(long)]
    tls: bool,

    /// TLS server certificate (PEM); enables HTTPS
    #[arg(long = "tls-cert-file")]
    tls_cert: Option<PathBuf>,

    /// TLS server private key (PEM)
    #[arg(long = "tls-private-key-file")]
    tls_key: Option<PathBuf>,

    /// CA bundle (PEM) to verify client certificates for x509 authentication
    #[arg(long = "client-ca-file")]
    client_ca: Option<PathBuf>,

    /// Data directory (TLS material, misc runtime state)
    #[arg(long, default_value = "/var/lib/kubernetes")]
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
        etcd_servers: cli.etcd_servers,
        etcd_cacert: cli.etcd_cacert,
        etcd_cert: cli.etcd_cert,
        etcd_key: cli.etcd_key,
        tls_cert: cli.tls_cert,
        tls_key: cli.tls_key,
        tls_auto: cli.tls,
        client_ca: cli.client_ca,
        data_dir: cli.data_dir,
        service_cidr: cli.service_cidr,
        cluster_domain: cli.cluster_domain,
        ..Default::default()
    };

    apiserver::run(config).await
}
