//! DNS server setup and main loop.
//!
//! Binds a UDP/TCP DNS server using hickory-dns,
//! serves cluster DNS from our authority, and
//! periodically syncs records from the API server.

use crate::authority::ClusterAuthority;
use crate::records::RecordStore;
use crate::syncer::sync_dns_records;
use hickory_proto::rr::{LowerName, Name};
use hickory_server::authority::Catalog;
use hickory_server::ServerFuture;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, UdpSocket};
use tokio::time;
use tracing::{error, info};

/// Cluster DNS configuration.
#[derive(Debug, Clone)]
pub struct DnsConfig {
    pub listen_addr: String,
    pub listen_port: u16,
    pub api_server_url: String,
    pub cluster_domain: String,
    pub ttl: u32,
    pub sync_interval: Duration,
    pub upstream_dns: Vec<String>,
}

impl Default for DnsConfig {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0".into(),
            listen_port: 53,
            api_server_url: "http://localhost:6443".into(),
            cluster_domain: "cluster.local".into(),
            ttl: 30,
            sync_interval: Duration::from_secs(5),
            upstream_dns: vec!["8.8.8.8".into(), "8.8.4.4".into()],
        }
    }
}

/// The cluster DNS server.
pub struct ClusterDns {
    config: DnsConfig,
    store: Arc<RecordStore>,
}

impl ClusterDns {
    pub fn new(config: DnsConfig) -> Self {
        let store = Arc::new(RecordStore::new(&config.cluster_domain, config.ttl));
        Self { config, store }
    }

    /// Run the DNS server. Blocks forever.
    pub async fn run(&self) -> anyhow::Result<()> {
        let bind_addr = format!("{}:{}", self.config.listen_addr, self.config.listen_port);

        info!(
            "Cluster DNS starting on {bind_addr}, domain={}",
            self.config.cluster_domain
        );

        // Create the authority and register it in a Catalog
        let authority = ClusterAuthority::new(&self.config.cluster_domain, self.store.clone());

        let origin: LowerName = Name::from_utf8(&self.config.cluster_domain)
            .unwrap_or_else(|_| Name::from_utf8("cluster.local").unwrap())
            .into();

        let mut catalog = Catalog::new();
        catalog.upsert(origin, vec![Arc::new(authority)]);

        // Spawn the syncer task
        let store = self.store.clone();
        let api_url = self.config.api_server_url.clone();
        let sync_interval = self.config.sync_interval;
        tokio::spawn(async move {
            let client = reqwest::Client::new();
            let mut interval = time::interval(sync_interval);
            loop {
                interval.tick().await;
                if let Err(e) = sync_dns_records(&api_url, &store, &client).await {
                    error!("DNS sync failed: {e}");
                }
            }
        });

        // Build the hickory server with the Catalog
        let mut server = ServerFuture::new(catalog);

        // Bind UDP
        let udp_socket = UdpSocket::bind(&bind_addr).await?;
        info!("DNS listening on UDP {bind_addr}");
        server.register_socket(udp_socket);

        // Bind TCP
        let tcp_listener = TcpListener::bind(&bind_addr).await?;
        info!("DNS listening on TCP {bind_addr}");
        server.register_listener(tcp_listener, Duration::from_secs(30));

        // Run the server
        server.block_until_done().await?;

        Ok(())
    }

    /// Get the record store (for testing/debugging).
    pub fn store(&self) -> &Arc<RecordStore> {
        &self.store
    }
}
