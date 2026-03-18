//! DNS record store.
//!
//! Maintains A, SRV, and PTR records for cluster services.
//! Thread-safe via DashMap for concurrent read/write.

use dashmap::DashMap;
use std::collections::HashSet;
use std::net::Ipv4Addr;
use std::sync::Arc;

/// DNS record types we store.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DnsRecord {
    /// A record: name → IPv4 address
    A {
        name: String,
        ip: Ipv4Addr,
        ttl: u32,
    },
    /// SRV record: name → target:port
    Srv {
        name: String,
        target: String,
        port: u16,
        priority: u16,
        weight: u16,
        ttl: u32,
    },
    /// PTR record: reverse IP → hostname
    Ptr {
        name: String,
        target: String,
        ttl: u32,
    },
}

/// Thread-safe DNS record store.
#[derive(Debug, Clone)]
pub struct RecordStore {
    /// A records: fqdn → set of IPv4 addresses
    a_records: Arc<DashMap<String, HashSet<Ipv4Addr>>>,
    /// SRV records: fqdn → set of (target, port, priority, weight)
    srv_records: Arc<DashMap<String, Vec<(String, u16, u16, u16)>>>,
    /// PTR records: reversed IP → hostname
    ptr_records: Arc<DashMap<String, String>>,
    /// Default TTL
    pub ttl: u32,
    /// Cluster domain suffix
    pub cluster_domain: String,
}

impl RecordStore {
    pub fn new(cluster_domain: &str, ttl: u32) -> Self {
        Self {
            a_records: Arc::new(DashMap::new()),
            srv_records: Arc::new(DashMap::new()),
            ptr_records: Arc::new(DashMap::new()),
            ttl,
            cluster_domain: cluster_domain.to_string(),
        }
    }

    /// Set A records for a service. Replaces existing records.
    pub fn set_a_records(&self, fqdn: &str, ips: HashSet<Ipv4Addr>) {
        let fqdn = fqdn.to_lowercase();

        // Remove old PTR records for this name
        let old_ips = self.a_records.get(&fqdn).map(|v| v.clone());
        if let Some(old) = old_ips {
            for ip in &old {
                let ptr_name = ip_to_ptr(ip);
                self.ptr_records.remove(&ptr_name);
            }
        }

        // Set new A records and PTR records
        for ip in &ips {
            let ptr_name = ip_to_ptr(ip);
            self.ptr_records.insert(ptr_name, format!("{fqdn}."));
        }

        self.a_records.insert(fqdn, ips);
    }

    /// Remove A records for a service.
    pub fn remove_a_records(&self, fqdn: &str) {
        let fqdn = fqdn.to_lowercase();
        if let Some((_, ips)) = self.a_records.remove(&fqdn) {
            for ip in &ips {
                let ptr_name = ip_to_ptr(ip);
                self.ptr_records.remove(&ptr_name);
            }
        }
    }

    /// Set SRV records for a service port.
    pub fn set_srv_records(&self, fqdn: &str, records: Vec<(String, u16, u16, u16)>) {
        self.srv_records.insert(fqdn.to_lowercase(), records);
    }

    /// Look up A records for a name.
    pub fn lookup_a(&self, name: &str) -> Vec<Ipv4Addr> {
        let name = name.to_lowercase();
        self.a_records
            .get(&name)
            .map(|ips| ips.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Look up SRV records for a name.
    pub fn lookup_srv(&self, name: &str) -> Vec<(String, u16, u16, u16)> {
        let name = name.to_lowercase();
        self.srv_records
            .get(&name)
            .map(|v| v.clone())
            .unwrap_or_default()
    }

    /// Look up PTR records for a reversed IP.
    pub fn lookup_ptr(&self, name: &str) -> Option<String> {
        let name = name.to_lowercase();
        self.ptr_records.get(&name).map(|v| v.clone())
    }

    /// Clear all records.
    pub fn clear(&self) {
        self.a_records.clear();
        self.srv_records.clear();
        self.ptr_records.clear();
    }

    /// Record count for metrics.
    pub fn record_count(&self) -> (usize, usize, usize) {
        (
            self.a_records.len(),
            self.srv_records.len(),
            self.ptr_records.len(),
        )
    }
}

/// Convert an IPv4 address to a PTR record name.
/// 10.96.0.1 → "1.0.96.10.in-addr.arpa"
fn ip_to_ptr(ip: &Ipv4Addr) -> String {
    let octets = ip.octets();
    format!(
        "{}.{}.{}.{}.in-addr.arpa",
        octets[3], octets[2], octets[1], octets[0]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ip_to_ptr() {
        let ip: Ipv4Addr = "10.96.0.1".parse().unwrap();
        assert_eq!(ip_to_ptr(&ip), "1.0.96.10.in-addr.arpa");
    }

    #[test]
    fn test_a_record_crud() {
        let store = RecordStore::new("cluster.local", 30);
        let mut ips = HashSet::new();
        ips.insert("10.96.0.1".parse().unwrap());

        store.set_a_records("kubernetes.default.svc.cluster.local", ips.clone());
        assert_eq!(
            store.lookup_a("kubernetes.default.svc.cluster.local"),
            vec!["10.96.0.1".parse::<Ipv4Addr>().unwrap()]
        );

        // PTR should be set automatically
        assert_eq!(
            store.lookup_ptr("1.0.96.10.in-addr.arpa"),
            Some("kubernetes.default.svc.cluster.local.".to_string())
        );

        store.remove_a_records("kubernetes.default.svc.cluster.local");
        assert!(store.lookup_a("kubernetes.default.svc.cluster.local").is_empty());
        assert!(store.lookup_ptr("1.0.96.10.in-addr.arpa").is_none());
    }
}
