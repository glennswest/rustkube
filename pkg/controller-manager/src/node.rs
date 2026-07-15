//! Node lifecycle controller.
//!
//! Monitors node heartbeats via Lease objects in kube-node-lease namespace.
//! Marks nodes as NotReady when lease expires, and taints them for eviction.

use crate::runner::ApiClient;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::time::{self, Duration};
use tracing::{error, info};

const NODE_MONITOR_GRACE_PERIOD: Duration = Duration::from_secs(40);

pub struct NodeLifecycleController {
    api: Arc<ApiClient>,
}

impl NodeLifecycleController {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self { api }
    }

    pub async fn run(&self) {
        info!("Node lifecycle controller started");
        let mut interval = time::interval(Duration::from_secs(5));

        loop {
            interval.tick().await;
            if let Err(e) = self.reconcile_all().await {
                error!("Node lifecycle reconcile error: {e}");
            }
        }
    }

    async fn reconcile_all(&self) -> anyhow::Result<()> {
        let node_list: Value = self.api.list("/api/v1/nodes").await?;
        let nodes = node_list["items"].as_array().cloned().unwrap_or_default();

        let lease_list: Value = self
            .api
            .list("/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases")
            .await?;
        let leases = lease_list["items"].as_array().cloned().unwrap_or_default();

        let now = chrono::Utc::now();

        for node in &nodes {
            let node_name = node["metadata"]["name"].as_str().unwrap_or("");
            if node_name.is_empty() {
                continue;
            }

            // Find the corresponding lease
            let lease = leases
                .iter()
                .find(|l| l["metadata"]["name"].as_str() == Some(node_name));

            let is_healthy = match lease {
                Some(l) => {
                    // Check renewTime
                    if let Some(renew_time) = l["spec"]["renewTime"].as_str() {
                        if let Ok(t) = chrono::DateTime::parse_from_rfc3339(renew_time) {
                            let elapsed = now.signed_duration_since(t);
                            elapsed.num_seconds() < NODE_MONITOR_GRACE_PERIOD.as_secs() as i64
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                }
                None => {
                    // No lease found — check if node was just created
                    // Give new nodes time to register their first lease
                    if let Some(created) = node["metadata"]["creationTimestamp"].as_str() {
                        if let Ok(t) = chrono::DateTime::parse_from_rfc3339(created) {
                            let elapsed = now.signed_duration_since(t);
                            elapsed.num_seconds() < NODE_MONITOR_GRACE_PERIOD.as_secs() as i64
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                }
            };

            // Check current Ready condition
            let conditions = node["status"]["conditions"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let current_ready = conditions.iter().any(|c| {
                c["type"].as_str() == Some("Ready") && c["status"].as_str() == Some("True")
            });

            if !is_healthy && current_ready {
                // Node went unhealthy — mark NotReady
                info!("Node {node_name} is not responding, marking NotReady");
                let mut updated = node.clone();
                update_condition(
                    &mut updated,
                    "Ready",
                    "False",
                    "NodeStatusUnknown",
                    "Kubelet stopped posting node status",
                );
                // Add NoSchedule taint
                add_taint(
                    &mut updated,
                    "node.kubernetes.io/not-ready",
                    "NoSchedule",
                );

                let _ = self
                    .api
                    .update(
                        &format!("/api/v1/nodes/{node_name}"),
                        &updated,
                    )
                    .await;
            }
        }

        Ok(())
    }
}

fn update_condition(node: &mut Value, cond_type: &str, status: &str, reason: &str, message: &str) {
    let conditions = node["status"]["conditions"]
        .as_array_mut();

    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let new_condition = json!({
        "type": cond_type,
        "status": status,
        "reason": reason,
        "message": message,
        "lastHeartbeatTime": now,
        "lastTransitionTime": now
    });

    match conditions {
        Some(conds) => {
            if let Some(existing) = conds.iter_mut().find(|c| c["type"].as_str() == Some(cond_type)) {
                *existing = new_condition;
            } else {
                conds.push(new_condition);
            }
        }
        None => {
            node["status"]["conditions"] = json!([new_condition]);
        }
    }
}

fn add_taint(node: &mut Value, key: &str, effect: &str) {
    let taint = json!({
        "key": key,
        "effect": effect,
        "timeAdded": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
    });

    match node["spec"]["taints"].as_array_mut() {
        Some(taints) => {
            if !taints.iter().any(|t| t["key"].as_str() == Some(key) && t["effect"].as_str() == Some(effect)) {
                taints.push(taint);
            }
        }
        None => {
            node["spec"]["taints"] = json!([taint]);
        }
    }
}
