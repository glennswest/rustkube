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

    /// How often to compact the datastore's history (a Go duration; 0 turns
    /// it off). One apiserver compacts per interval, to the revision of one
    /// interval earlier, so a LIST continue token lasts one to two intervals.
    #[arg(long = "etcd-compaction-interval", default_value = "5m0s", value_parser = apiserver::compactor::parse_interval)]
    etcd_compaction_interval: std::time::Duration,

    /// Scheme of each node's cadvisor, read for metrics.k8s.io (http, https)
    #[arg(long = "cadvisor-scheme", default_value = "http")]
    cadvisor_scheme: String,

    /// Port of each node's cadvisor (stormcos: 9096)
    #[arg(long = "cadvisor-port", default_value_t = 9096)]
    cadvisor_port: u16,

    /// CA bundle (PEM) to verify cadvisor over https
    #[arg(long = "cadvisor-ca-file")]
    cadvisor_ca: Option<PathBuf>,

    /// Bearer token file sent to cadvisor (its --bearer-token-file)
    #[arg(long = "cadvisor-token-file")]
    cadvisor_token_file: Option<PathBuf>,

    /// Client certificate (PEM) presented to aggregated API servers, as
    /// upstream's front proxy; renewed in place without a restart
    #[arg(long = "proxy-client-cert-file")]
    proxy_client_cert: Option<PathBuf>,

    /// Private key (PEM) for --proxy-client-cert-file
    #[arg(long = "proxy-client-key-file")]
    proxy_client_key: Option<PathBuf>,

    /// CA bundle (PEM) that signed --proxy-client-cert-file, published to
    /// aggregated API servers in kube-system/extension-apiserver-authentication
    #[arg(long = "requestheader-client-ca-file")]
    requestheader_client_ca: Option<PathBuf>,

    /// Common names allowed for the front-proxy certificate (comma-separated;
    /// empty: any signed by --requestheader-client-ca-file)
    #[arg(long = "requestheader-allowed-names", value_delimiter = ',')]
    requestheader_allowed_names: Vec<String>,

    /// Serve HTTPS with a self-signed cert (dev/bootstrap), kept under
    /// --data-dir
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

    /// CRL (PEM or DER) whose revoked client certificates are refused;
    /// repeatable or comma-separated, one per signer. Re-read every 30 s; a
    /// bad or missing file keeps the last good one. Needs --client-ca-file
    #[arg(long = "client-crl-file", value_delimiter = ',')]
    client_crl: Vec<PathBuf>,

    /// Allow anonymous requests. Set false to require authentication (401 for
    /// unauthenticated requests). Even when true, anonymous is bound only to
    /// discovery/health unless --dev-anonymous-admin is also set.
    #[arg(long = "anonymous-auth", default_value_t = true, action = clap::ArgAction::Set)]
    anonymous_auth: bool,

    /// DEV ONLY: bind anonymous requests to cluster-admin so kubectl works
    /// without credentials. Off by default — a secured cluster must never grant
    /// anonymous standing access.
    #[arg(long = "dev-anonymous-admin", default_value_t = false, action = clap::ArgAction::Set)]
    dev_anonymous_admin: bool,

    /// Permit serving plain HTTP when no TLS cert/key (and no --tls) is given.
    /// Off by default: the server refuses to start on plain HTTP rather than
    /// silently dropping TLS. Required for plaintext dev/bring-up.
    #[arg(long = "insecure", default_value_t = false, action = clap::ArgAction::Set)]
    insecure: bool,

    /// Public key file(s) (PEM; a file may hold several keys) tokens are
    /// VERIFIED against — repeat it or separate with commas. A token signed
    /// by any of them verifies, so the signing key can be rotated with an
    /// overlap. Pair with --service-account-signing-key-file; identical on
    /// every replica.
    #[arg(long = "service-account-key-file", value_delimiter = ',')]
    service_account_key: Vec<PathBuf>,

    /// Directory of manifests to apply once at startup (YAML or JSON, applied
    /// in filename order). Objects are created if absent; an object annotated
    /// `addonmanager.kubernetes.io/mode: Reconcile` is also overwritten when
    /// it already exists.
    #[arg(long = "manifest-dir", env = "MANIFEST_DIR")]
    manifest_dir: Option<PathBuf>,

    /// Private key (PEM) used to SIGN ServiceAccount tokens (upstream name).
    /// Without it the apiserver falls back to an ephemeral per-process key and
    /// tokens will not validate on other replicas or survive a restart.
    #[arg(long = "service-account-signing-key-file")]
    service_account_signing_key: Option<PathBuf>,

    /// Static bearer tokens, one `token,user,uid[,"group1,group2"]` line each
    /// (upstream's --token-auth-file format). Re-read when it changes; a
    /// missing file means no static tokens, and removing it revokes them.
    #[arg(long = "token-auth-file")]
    token_auth_file: Option<PathBuf>,

    /// `iss` of minted ServiceAccount tokens; a token naming another issuer
    /// is refused. Also the default --api-audiences.
    #[arg(long = "service-account-issuer", default_value = "https://kubernetes.default.svc")]
    service_account_issuer: String,

    /// Audiences a token must be for to authenticate here, comma-separated;
    /// also a TokenRequest's default audiences. Defaults to the issuer.
    #[arg(long = "api-audiences", value_delimiter = ',')]
    api_audiences: Vec<String>,

    /// Give a pod-bound TokenRequest for 3607 s (a projected token volume) a
    /// year, with `warnafter` at 3607 s, as upstream does.
    #[arg(long = "service-account-extend-token-expiration", default_value_t = true, action = clap::ArgAction::Set)]
    service_account_extend_token_expiration: bool,

    /// Address advertised to in-cluster clients; registered as an endpoint of
    /// the default/kubernetes Service. Defaults to --bind-addr when concrete.
    #[arg(long = "advertise-address")]
    advertise_address: Option<String>,

    /// Where the --tls self-signed certificate is kept (apiserver.crt,
    /// apiserver.key): written on first start, reused after a restart (#88)
    #[arg(long, default_value = "/var/lib/kubernetes")]
    data_dir: PathBuf,

    /// Service CIDR
    #[arg(long, default_value = "10.96.0.0/12")]
    service_cidr: String,

    /// Port range NodePort Services are allocated from, inclusive (#132)
    #[arg(long, default_value = "30000-32767")]
    service_node_port_range: String,

    /// Cluster DNS domain: the --tls certificate's
    /// kubernetes.default.svc.<domain> SAN (#88)
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
        etcd_compaction_interval: cli.etcd_compaction_interval,
        cadvisor_scheme: cli.cadvisor_scheme,
        cadvisor_port: cli.cadvisor_port,
        cadvisor_ca: cli.cadvisor_ca,
        cadvisor_token_file: cli.cadvisor_token_file,
        proxy_client_cert: cli.proxy_client_cert,
        proxy_client_key: cli.proxy_client_key,
        requestheader_client_ca: cli.requestheader_client_ca,
        requestheader_allowed_names: cli.requestheader_allowed_names,
        tls_cert: cli.tls_cert,
        tls_key: cli.tls_key,
        tls_auto: cli.tls,
        client_ca: cli.client_ca,
        client_crl: cli.client_crl,
        anonymous_auth: cli.anonymous_auth,
        dev_anonymous_admin: cli.dev_anonymous_admin,
        insecure: cli.insecure,
        manifest_dir: cli.manifest_dir,
        service_account_key: cli.service_account_key,
        service_account_signing_key: cli.service_account_signing_key,
        token_auth_file: cli.token_auth_file,
        service_account_issuer: cli.service_account_issuer,
        api_audiences: cli.api_audiences,
        service_account_extend_token_expiration: cli.service_account_extend_token_expiration,
        advertise_address: cli.advertise_address,
        data_dir: cli.data_dir,
        service_cidr: cli.service_cidr,
        service_node_port_range: cli.service_node_port_range,
        cluster_domain: cli.cluster_domain,
        ..Default::default()
    };

    apiserver::run(config).await
}
