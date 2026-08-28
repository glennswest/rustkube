//! Node lifecycle controller.
//!
//! Monitors node heartbeats via Lease objects in kube-node-lease namespace.
//! Marks nodes as NotReady when lease expires, and taints them for eviction.

use crate::runner::ApiClient;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::time::{self, Duration};
use tracing::{debug, error, info};

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
                // Both effects, as upstream does. NoSchedule keeps new work
                // off; NoExecute is what moves work that is already there, and
                // without it a node can be unreachable for an hour with its
                // pods still nominally running on it.
                let stamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
                apimachinery::taint::add_taint(
                    &mut updated,
                    "node.kubernetes.io/not-ready",
                    "NoSchedule",
                    &stamp,
                );
                apimachinery::taint::add_taint(
                    &mut updated,
                    "node.kubernetes.io/not-ready",
                    "NoExecute",
                    &stamp,
                );

                let _ = self
                    .api
                    .update(
                        &format!("/api/v1/nodes/{node_name}"),
                        &updated,
                    )
                    .await;
            }

            // **And take them off again.** A taint added when a node goes bad
            // and never removed when it recovers leaves a healthy node
            // permanently unschedulable — and the reason is in `spec` while
            // everyone reading a node looks at `status`, so it presents as
            // "the scheduler is ignoring my node".
            if is_healthy {
                let mut updated = node.clone();
                let mut changed = false;
                for effect in ["NoSchedule", "NoExecute"] {
                    changed |= apimachinery::taint::remove_taint(
                        &mut updated,
                        "node.kubernetes.io/not-ready",
                        effect,
                    );
                }
                if changed {
                    info!("Node {node_name} is responding again, clearing not-ready taints");
                    let _ = self
                        .api
                        .update(&format!("/api/v1/nodes/{node_name}"), &updated)
                        .await;
                }
            }

            // NoExecute means "get off", so something has to do the moving.
            if let Err(e) = self.evict_for_taints(node, node_name).await {
                debug!("taint eviction on {node_name}: {e}");
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


impl NodeLifecycleController {
    /// Evict pods that do not tolerate this node's `NoExecute` taints.
    ///
    /// Without this a `NoExecute` taint is a `NoSchedule` taint with a
    /// different spelling: it keeps new pods away and leaves the existing ones
    /// exactly where they were, which is the opposite of what it means.
    ///
    /// `tolerationSeconds` is respected rather than rounded away — a pod that
    /// tolerates `not-ready:NoExecute` for 300s is saying "give the node five
    /// minutes to come back before moving me", and evicting at once turns a
    /// blip into an outage. Pods that are not yet due are simply left; the
    /// next pass reconsiders them, which is what makes the deadline work
    /// without a timer.
    async fn evict_for_taints(&self, node: &Value, node_name: &str) -> anyhow::Result<()> {
        let taints: Vec<Value> = node["spec"]["taints"]
            .as_array()
            .map(|a| a.iter().filter(|t| t["effect"].as_str() == Some("NoExecute")).cloned().collect())
            .unwrap_or_default();
        if taints.is_empty() {
            return Ok(());
        }

        let now = chrono::Utc::now();
        let age = |t: &Value| -> i64 {
            t["timeAdded"]
                .as_str()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|added| now.signed_duration_since(added).num_seconds())
                // No timeAdded means we cannot tell how long it has been
                // there. Treating that as "just now" is the safe reading: it
                // delays an eviction rather than bringing one forward.
                .unwrap_or(0)
        };

        let pods: Value = self.api.list("/api/v1/pods").await?;
        for pod in pods["items"].as_array().cloned().unwrap_or_default() {
            if pod["spec"]["nodeName"].as_str() != Some(node_name) {
                continue;
            }
            // A pod already going away is not evicted again.
            if !pod["metadata"]["deletionTimestamp"].is_null() {
                continue;
            }
            let phase = pod["status"]["phase"].as_str().unwrap_or("Pending");
            if phase == "Succeeded" || phase == "Failed" {
                continue;
            }
            if let apimachinery::taint::Verdict::Evict(key) =
                apimachinery::taint::verdict_for(&pod, &taints, age)
            {
                let ns = pod["metadata"]["namespace"].as_str().unwrap_or("default");
                let name = pod["metadata"]["name"].as_str().unwrap_or("");
                if name.is_empty() {
                    continue;
                }
                info!(
                    "Evicting {ns}/{name} from {node_name}: does not tolerate {key}:NoExecute"
                );
                let _ = self
                    .api
                    .delete(&format!("/api/v1/namespaces/{ns}/pods/{name}"))
                    .await;
            }
        }
        Ok(())
    }
}
