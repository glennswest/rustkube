//! Service/Endpoints syncer for DNS records.
//!
//! Watches the API server for Service and Endpoints changes,
//! converting them into DNS A, SRV, and PTR records.

use crate::records::RecordStore;
use serde_json::Value;
use std::collections::HashSet;
use std::net::Ipv4Addr;
use tracing::debug;

/// Sync DNS records from the API server.
pub async fn sync_dns_records(
    api_url: &str,
    store: &RecordStore,
    client: &reqwest::Client,
) -> anyhow::Result<()> {
    // Fetch all services
    let svc_resp: Value = client
        .get(format!("{api_url}/api/v1/services"))
        .send()
        .await?
        .json()
        .await?;

    let services = svc_resp["items"].as_array().cloned().unwrap_or_default();

    // Fetch all endpoints
    let ep_resp: Value = client
        .get(format!("{api_url}/api/v1/endpoints"))
        .send()
        .await?
        .json()
        .await?;

    let endpoints = ep_resp["items"].as_array().cloned().unwrap_or_default();

    // Build a lookup from namespace/name → endpoints
    let mut ep_map: std::collections::HashMap<String, &Value> = std::collections::HashMap::new();
    for ep in &endpoints {
        let name = ep["metadata"]["name"].as_str().unwrap_or("");
        let ns = ep["metadata"]["namespace"].as_str().unwrap_or("default");
        ep_map.insert(format!("{ns}/{name}"), ep);
    }

    let domain = &store.cluster_domain;
    let mut seen_fqdns = HashSet::new();

    for svc in &services {
        let name = svc["metadata"]["name"].as_str().unwrap_or("");
        let namespace = svc["metadata"]["namespace"].as_str().unwrap_or("default");
        let cluster_ip = svc["spec"]["clusterIP"].as_str().unwrap_or("");
        let _svc_type = svc["spec"]["type"].as_str().unwrap_or("ClusterIP");

        // FQDN: svc-name.namespace.svc.cluster.local
        let fqdn = format!("{name}.{namespace}.svc.{domain}");
        seen_fqdns.insert(fqdn.clone());

        let is_headless = cluster_ip.is_empty() || cluster_ip == "None";

        if is_headless {
            // Headless service — A records point to individual pod IPs
            let ep_key = format!("{namespace}/{name}");
            if let Some(ep) = ep_map.get(&ep_key) {
                let mut pod_ips = HashSet::new();

                let subsets = ep["subsets"].as_array().cloned().unwrap_or_default();
                for subset in &subsets {
                    let addresses = subset["addresses"].as_array().cloned().unwrap_or_default();
                    for addr in &addresses {
                        if let Some(ip_str) = addr["ip"].as_str() {
                            if let Ok(ip) = ip_str.parse::<Ipv4Addr>() {
                                pod_ips.insert(ip);

                                // Also create pod-specific DNS:
                                // <ip-dashed>.namespace.pod.cluster.local
                                let ip_dashed = ip_str.replace('.', "-");
                                let pod_fqdn =
                                    format!("{ip_dashed}.{namespace}.pod.{domain}");
                                let mut single = HashSet::new();
                                single.insert(ip);
                                store.set_a_records(&pod_fqdn, single);
                                seen_fqdns.insert(pod_fqdn);

                                // Hostname-based: hostname.svc.namespace.svc.cluster.local
                                if let Some(hostname) = addr["hostname"].as_str() {
                                    let host_fqdn =
                                        format!("{hostname}.{name}.{namespace}.svc.{domain}");
                                    let mut single = HashSet::new();
                                    single.insert(ip);
                                    store.set_a_records(&host_fqdn, single);
                                    seen_fqdns.insert(host_fqdn);
                                }
                            }
                        }
                    }
                }

                store.set_a_records(&fqdn, pod_ips);
            }
        } else {
            // ClusterIP service — A record points to cluster IP
            if let Ok(ip) = cluster_ip.parse::<Ipv4Addr>() {
                let mut ips = HashSet::new();
                ips.insert(ip);
                store.set_a_records(&fqdn, ips);
            }
        }

        // SRV records for named ports
        let ports = svc["spec"]["ports"].as_array().cloned().unwrap_or_default();
        for port_spec in &ports {
            let port_name = port_spec["name"].as_str().unwrap_or("");
            let port_num = port_spec["port"].as_u64().unwrap_or(0) as u16;
            let protocol = port_spec["protocol"]
                .as_str()
                .unwrap_or("TCP")
                .to_lowercase();

            if !port_name.is_empty() {
                // SRV: _port-name._protocol.svc-name.namespace.svc.cluster.local
                let srv_fqdn =
                    format!("_{port_name}._{protocol}.{name}.{namespace}.svc.{domain}");

                if is_headless {
                    // SRV points to individual pod hostnames
                    let ep_key = format!("{namespace}/{name}");
                    if let Some(ep) = ep_map.get(&ep_key) {
                        let mut srv_records = Vec::new();
                        let subsets =
                            ep["subsets"].as_array().cloned().unwrap_or_default();

                        for subset in &subsets {
                            let addresses =
                                subset["addresses"].as_array().cloned().unwrap_or_default();
                            for addr in &addresses {
                                if let Some(hostname) = addr["hostname"].as_str() {
                                    let target = format!(
                                        "{hostname}.{name}.{namespace}.svc.{domain}."
                                    );
                                    srv_records.push((target, port_num, 0, 100));
                                } else if let Some(ip_str) = addr["ip"].as_str() {
                                    let ip_dashed = ip_str.replace('.', "-");
                                    let target = format!(
                                        "{ip_dashed}.{namespace}.pod.{domain}."
                                    );
                                    srv_records.push((target, port_num, 0, 100));
                                }
                            }
                        }

                        store.set_srv_records(&srv_fqdn, srv_records);
                    }
                } else {
                    // SRV points to the service FQDN
                    store.set_srv_records(
                        &srv_fqdn,
                        vec![(format!("{fqdn}."), port_num, 0, 100)],
                    );
                }
            }
        }
    }

    debug!(
        "DNS synced: {} A records, {} SRV records, {} PTR records",
        store.record_count().0,
        store.record_count().1,
        store.record_count().2
    );

    Ok(())
}
