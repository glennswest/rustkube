use std::path::PathBuf;

/// API server configuration.
#[derive(Debug, Clone)]
pub struct ApiServerConfig {
    /// Address to bind to.
    pub bind_addr: String,
    /// Port for HTTPS.
    pub secure_port: u16,
    /// Path to TLS certificate PEM.
    pub tls_cert: Option<PathBuf>,
    /// Path to TLS private key PEM.
    pub tls_key: Option<PathBuf>,
    /// Data directory for the embedded store.
    pub data_dir: PathBuf,
    /// Cluster CIDR for service IPs.
    pub service_cidr: String,
    /// Cluster DNS domain.
    pub cluster_domain: String,
    /// Path to JWT signing key for ServiceAccount tokens.
    pub service_account_key: Option<PathBuf>,
    /// Allow anonymous authentication (default true for dev).
    pub anonymous_auth: bool,
}

impl Default for ApiServerConfig {
    fn default() -> Self {
        Self {
            bind_addr: "0.0.0.0".into(),
            secure_port: 6443,
            tls_cert: None,
            tls_key: None,
            data_dir: PathBuf::from("/var/lib/rustkube"),
            service_cidr: "10.96.0.0/12".into(),
            cluster_domain: "cluster.local".into(),
            service_account_key: None,
            anonymous_auth: true,
        }
    }
}
