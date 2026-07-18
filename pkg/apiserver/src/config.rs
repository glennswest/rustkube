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
    /// Serve HTTPS with an auto-generated self-signed cert when no cert is
    /// configured (dev/bootstrap). Ignored if `tls_cert`/`tls_key` are set.
    pub tls_auto: bool,
    /// CA bundle (PEM) to verify client certificates for x509 authentication.
    pub client_ca: Option<PathBuf>,
    /// External etcd/fastetcd endpoints (e.g. `https://127.0.0.1:2379`).
    /// Required — RustKube uses an external datastore (kube architecture).
    pub etcd_servers: Vec<String>,
    /// CA certificate (PEM) to verify the etcd/fastetcd server.
    pub etcd_cacert: Option<PathBuf>,
    /// Client certificate (PEM) for mutual TLS to etcd/fastetcd.
    pub etcd_cert: Option<PathBuf>,
    /// Client private key (PEM) for mutual TLS to etcd/fastetcd.
    pub etcd_key: Option<PathBuf>,
    /// Data directory (TLS material, misc runtime state).
    pub data_dir: PathBuf,
    /// Cluster CIDR for service IPs.
    pub service_cidr: String,
    /// Cluster DNS domain.
    pub cluster_domain: String,
    /// Public key (SPKI PEM) used to *verify* ServiceAccount tokens.
    pub service_account_key: Option<PathBuf>,
    /// Private key (PKCS#1/PKCS#8 PEM) used to *sign* ServiceAccount tokens.
    /// Must be the counterpart of `service_account_key`, and identical on every
    /// replica so tokens validate cluster-wide (#11).
    pub service_account_signing_key: Option<PathBuf>,
    /// Allow anonymous authentication (default true for dev).
    pub anonymous_auth: bool,
    /// Address this apiserver advertises to in-cluster clients. Registered as an
    /// endpoint of the `default/kubernetes` Service (#30). Falls back to
    /// `bind_addr` when it is a concrete address.
    pub advertise_address: Option<String>,
}

impl Default for ApiServerConfig {
    fn default() -> Self {
        Self {
            bind_addr: "0.0.0.0".into(),
            secure_port: 6443,
            tls_cert: None,
            tls_key: None,
            tls_auto: false,
            client_ca: None,
            etcd_servers: Vec::new(),
            etcd_cacert: None,
            etcd_cert: None,
            etcd_key: None,
            data_dir: PathBuf::from("/var/lib/kubernetes"),
            service_cidr: "10.96.0.0/12".into(),
            cluster_domain: "cluster.local".into(),
            service_account_key: None,
            service_account_signing_key: None,
            anonymous_auth: true,
            advertise_address: None,
        }
    }
}
