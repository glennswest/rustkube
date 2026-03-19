//! DNS upstream forwarder.
//!
//! Forwards queries that don't match the cluster domain to upstream
//! DNS servers. Supports multiple upstreams with round-robin fallback.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::net::UdpSocket;
use tracing::{debug, warn};

/// Upstream DNS forwarder with round-robin selection.
pub struct Forwarder {
    upstreams: Vec<SocketAddr>,
    next: AtomicUsize,
    timeout: std::time::Duration,
}

impl Forwarder {
    /// Create a new forwarder with the given upstream DNS servers.
    pub fn new(upstreams: Vec<SocketAddr>, timeout_secs: u64) -> Self {
        Self {
            upstreams,
            next: AtomicUsize::new(0),
            timeout: std::time::Duration::from_secs(timeout_secs),
        }
    }

    /// Forward a DNS query to upstream and return the response bytes.
    pub async fn forward(&self, query_bytes: &[u8]) -> Option<Vec<u8>> {
        if self.upstreams.is_empty() {
            return None;
        }

        let upstream = self.pick_upstream();
        debug!("Forwarding DNS query to upstream {upstream}");

        match self.send_query(upstream, query_bytes).await {
            Ok(response) => Some(response),
            Err(e) => {
                warn!("Upstream DNS {upstream} failed: {e}");
                // Try next upstream
                let fallback = self.pick_upstream();
                if fallback != upstream {
                    match self.send_query(fallback, query_bytes).await {
                        Ok(response) => Some(response),
                        Err(e) => {
                            warn!("Upstream DNS {fallback} also failed: {e}");
                            None
                        }
                    }
                } else {
                    None
                }
            }
        }
    }

    /// Check if a DNS name should be forwarded (not in cluster domain).
    pub fn should_forward(name: &str, cluster_domain: &str) -> bool {
        !name.ends_with(cluster_domain) && !name.trim_end_matches('.').ends_with(cluster_domain.trim_end_matches('.'))
    }

    fn pick_upstream(&self) -> SocketAddr {
        let idx = self.next.fetch_add(1, Ordering::Relaxed) % self.upstreams.len();
        self.upstreams[idx]
    }

    async fn send_query(
        &self,
        upstream: SocketAddr,
        query_bytes: &[u8],
    ) -> anyhow::Result<Vec<u8>> {
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        socket.send_to(query_bytes, upstream).await?;

        let mut buf = vec![0u8; 4096];
        let result = tokio::time::timeout(self.timeout, socket.recv_from(&mut buf)).await;

        match result {
            Ok(Ok((len, _))) => {
                buf.truncate(len);
                Ok(buf)
            }
            Ok(Err(e)) => Err(e.into()),
            Err(_) => Err(anyhow::anyhow!("DNS upstream timeout")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_forward() {
        assert!(!Forwarder::should_forward(
            "my-svc.default.svc.cluster.local.",
            "cluster.local"
        ));
        assert!(!Forwarder::should_forward(
            "pod.default.svc.cluster.local",
            "cluster.local"
        ));
        assert!(Forwarder::should_forward("google.com.", "cluster.local"));
        assert!(Forwarder::should_forward("example.org", "cluster.local"));
    }
}
