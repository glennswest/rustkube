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
    /// The client certificate this apiserver presents to aggregated API
    /// servers (#83), upstream's front-proxy identity; followed on disk.
    pub proxy_client_cert: Option<PathBuf>,
    pub proxy_client_key: Option<PathBuf>,
    /// The CA that signed `proxy_client_cert`, published for aggregated API
    /// servers in `kube-system/extension-apiserver-authentication`.
    pub requestheader_client_ca: Option<PathBuf>,
    /// Common names a front-proxy certificate may have; empty: any.
    pub requestheader_allowed_names: Vec<String>,
    /// Each node's cadvisor, for `metrics.k8s.io` (#89): scheme, port,
    /// a CA for https, a bearer token file.
    pub cadvisor_scheme: String,
    pub cadvisor_port: u16,
    pub cadvisor_ca: Option<PathBuf>,
    pub cadvisor_token_file: Option<PathBuf>,
    /// How often the datastore is compacted (#139); zero turns it off.
    pub etcd_compaction_interval: std::time::Duration,
    /// Data directory (TLS material, misc runtime state).
    pub data_dir: PathBuf,
    /// Cluster CIDR for service IPs.
    pub service_cidr: String,
    /// `--service-node-port-range` (#132), inclusive, `FROM-TO`.
    pub service_node_port_range: String,
    /// Cluster DNS domain.
    pub cluster_domain: String,
    /// A directory of manifests applied once at startup.
    ///
    /// How a component that ships in the disk image — the pod network first —
    /// becomes API objects before anything can schedule it. See
    /// [`crate::manifests`].
    pub manifest_dir: Option<PathBuf>,
    /// Public key (SPKI PEM) used to *verify* ServiceAccount tokens.
    /// Every public key tokens may be signed with (#223).
    pub service_account_key: Vec<PathBuf>,
    /// Private key (PKCS#1/PKCS#8 PEM) used to *sign* ServiceAccount tokens.
    /// Must be the counterpart of `service_account_key`, and identical on every
    /// replica so tokens validate cluster-wide (#11).
    pub service_account_signing_key: Option<PathBuf>,
    /// Static bearer tokens, kube-apiserver's `--token-auth-file` format
    /// (`token,user,uid[,"groups"]`), followed for changes (#188).
    pub token_auth_file: Option<PathBuf>,
    /// `iss` of the ServiceAccount tokens this apiserver mints; a token
    /// naming another issuer is refused (#182).
    pub service_account_issuer: String,
    /// Audiences a token must be for to authenticate to this apiserver, and
    /// the default `aud` of a TokenRequest. Empty: the issuer (#182).
    pub api_audiences: Vec<String>,
    /// Give a pod-bound 3607 s TokenRequest a year, with `warnafter` at
    /// 3607 s, as upstream does for clients that do not refresh (#182).
    pub service_account_extend_token_expiration: bool,
    /// Allow anonymous authentication (default true for dev). Even when true,
    /// anonymous is only bound to discovery/health unless `dev_anonymous_admin`
    /// is also set (#16).
    pub anonymous_auth: bool,
    /// Bind anonymous requests to cluster-admin (the dev "kubectl without certs"
    /// convenience). Off by default — a secured cluster never grants anonymous
    /// standing access (#16).
    pub dev_anonymous_admin: bool,
    /// Permit serving plain HTTP when no TLS material is configured. Off by
    /// default: the server refuses to start on plain HTTP unless this is set,
    /// so TLS is never dropped silently (#16).
    pub insecure: bool,
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
            manifest_dir: None,
            client_ca: None,
            etcd_servers: Vec::new(),
            etcd_cacert: None,
            etcd_cert: None,
            etcd_key: None,
            cadvisor_scheme: "http".into(),
            cadvisor_port: 9096,
            cadvisor_ca: None,
            cadvisor_token_file: None,
            proxy_client_cert: None,
            proxy_client_key: None,
            requestheader_client_ca: None,
            requestheader_allowed_names: Vec::new(),
            etcd_compaction_interval: std::time::Duration::from_secs(300),
            data_dir: PathBuf::from("/var/lib/kubernetes"),
            service_cidr: "10.96.0.0/12".into(),
            service_node_port_range: crate::node_port::DEFAULT_RANGE.into(),
            cluster_domain: "cluster.local".into(),
            service_account_key: Vec::new(),
            service_account_signing_key: None,
            token_auth_file: None,
            service_account_issuer: crate::auth::DEFAULT_ISSUER.into(),
            api_audiences: Vec::new(),
            service_account_extend_token_expiration: true,
            anonymous_auth: true,
            dev_anonymous_admin: false,
            insecure: false,
            advertise_address: None,
        }
    }
}
