//! DaemonSet controller.
//!
//! Ensures one pod runs on every Ready node. Pods are named
//! {ds-name}-{5-char-hash-of-node-name} and placed directly
//! (bypasses scheduler by setting spec.nodeName).

use crate::runner::ApiClient;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::time::{self, Duration};
use tracing::{debug, error, info, warn};

pub struct DaemonSetController {
    api: Arc<ApiClient>,
}

impl DaemonSetController {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self { api }
    }

    pub async fn run(&self) {
        info!("DaemonSet controller started");
        let mut interval = time::interval(Duration::from_secs(3));

        loop {
            interval.tick().await;
            if let Err(e) = self.reconcile_all().await {
                error!("DaemonSet reconcile error: {e}");
            }
        }
    }

    async fn reconcile_all(&self) -> anyhow::Result<()> {
        let ns_list: Value = self.api.list("/api/v1/namespaces").await?;
        let namespaces = ns_list["items"].as_array().cloned().unwrap_or_default();

        // Get all ready nodes once (shared across namespaces)
        let node_list: Value = self.api.list("/api/v1/nodes").await?;
        let nodes = node_list["items"].as_array().cloned().unwrap_or_default();
        let ready_nodes: Vec<String> = nodes
            .iter()
            .filter(|n| is_node_ready(n))
            .filter_map(|n| n["metadata"]["name"].as_str().map(|s| s.to_string()))
            .collect();

        for ns in &namespaces {
            let ns_name = ns["metadata"]["name"].as_str().unwrap_or("default");
            if let Err(e) = self.reconcile_namespace(ns_name, &ready_nodes).await {
                debug!("DaemonSet reconcile in {ns_name}: {e}");
            }
        }
        Ok(())
    }

    async fn reconcile_namespace(
        &self,
        namespace: &str,
        ready_nodes: &[String],
    ) -> anyhow::Result<()> {
        let ds_list: Value = self
            .api
            .list(&format!(
                "/apis/apps/v1/namespaces/{namespace}/daemonsets"
            ))
            .await?;
        let daemonsets = ds_list["items"].as_array().cloned().unwrap_or_default();

        let pod_list: Value = self
            .api
            .list(&format!("/api/v1/namespaces/{namespace}/pods"))
            .await?;
        let pods = pod_list["items"].as_array().cloned().unwrap_or_default();

        for ds in &daemonsets {
            if let Err(e) = self
                .reconcile_daemonset(namespace, ds, &pods, ready_nodes)
                .await
            {
                let name = ds["metadata"]["name"].as_str().unwrap_or("?");
                warn!("Failed to reconcile daemonset {namespace}/{name}: {e}");
            }
        }
        Ok(())
    }

    async fn reconcile_daemonset(
        &self,
        namespace: &str,
        ds: &Value,
        all_pods: &[Value],
        ready_nodes: &[String],
    ) -> anyhow::Result<()> {
        let ds_name = ds["metadata"]["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("daemonset missing name"))?;
        let ds_uid = ds["metadata"]["uid"].as_str().unwrap_or("");

        // Find pods owned by this DaemonSet
        let owned_pods: Vec<&Value> = all_pods
            .iter()
            .filter(|pod| {
                pod["metadata"]["ownerReferences"]
                    .as_array()
                    .map(|refs| refs.iter().any(|r| r["uid"].as_str() == Some(ds_uid)))
                    .unwrap_or(false)
            })
            .filter(|pod| {
                let phase = pod["status"]["phase"].as_str().unwrap_or("Pending");
                phase != "Succeeded" && phase != "Failed"
            })
            .collect();

        // Build set of nodes that already have a pod
        let nodes_with_pods: HashSet<String> = owned_pods
            .iter()
            .filter_map(|pod| {
                pod["spec"]["nodeName"]
                    .as_str()
                    .map(|s| s.to_string())
            })
            .collect();

        // Create pods on nodes that need one
        for node_name in ready_nodes {
            if !nodes_with_pods.contains(node_name) {
                let pod = build_daemonset_pod(namespace, ds_name, ds_uid, node_name, ds)?;
                match self
                    .api
                    .create(&format!("/api/v1/namespaces/{namespace}/pods"), &pod)
                    .await
                {
                    Ok(_) => {
                        let pod_name = pod["metadata"]["name"].as_str().unwrap_or("?");
                        info!("Created DaemonSet pod {namespace}/{pod_name} on node {node_name}");
                    }
                    Err(e) => {
                        warn!("Failed to create DaemonSet pod on {node_name}: {e}");
                    }
                }
            }
        }

        // Delete pods on nodes that no longer exist
        let ready_set: HashSet<&str> = ready_nodes.iter().map(|s| s.as_str()).collect();
        for pod in &owned_pods {
            let node_name = pod["spec"]["nodeName"].as_str().unwrap_or("");
            if !node_name.is_empty() && !ready_set.contains(node_name) {
                let pod_name = pod["metadata"]["name"].as_str().unwrap_or("");
                if !pod_name.is_empty() {
                    match self
                        .api
                        .delete(&format!(
                            "/api/v1/namespaces/{namespace}/pods/{pod_name}"
                        ))
                        .await
                    {
                        Ok(_) => {
                            info!("Deleted DaemonSet pod {pod_name} (node {node_name} gone)");
                        }
                        Err(e) => {
                            warn!("Failed to delete DaemonSet pod {pod_name}: {e}");
                        }
                    }
                }
            }
        }

        // Update DaemonSet status
        let desired = ready_nodes.len();
        let current_scheduled = owned_pods.len();
        let ready_count = owned_pods.iter().filter(|p| is_pod_ready(p)).count();

        let mut updated = ds.clone();
        updated["status"] = json!({
            "desiredNumberScheduled": desired,
            "currentNumberScheduled": current_scheduled,
            "numberReady": ready_count,
            "numberAvailable": ready_count,
            "observedGeneration": ds["metadata"]["generation"].as_u64().unwrap_or(1)
        });

        let _ = self
            .api
            .update(
                &format!("/apis/apps/v1/namespaces/{namespace}/daemonsets/{ds_name}"),
                &updated,
            )
            .await;

        Ok(())
    }
}

fn build_daemonset_pod(
    namespace: &str,
    ds_name: &str,
    ds_uid: &str,
    node_name: &str,
    ds: &Value,
) -> anyhow::Result<Value> {
    let template = &ds["spec"]["template"];
    let node_hash = &hash_node_name(node_name);
    let pod_name = format!("{ds_name}-{node_hash}");

    let mut labels = template["metadata"]["labels"].clone();
    if labels.is_null() {
        labels = json!({});
    }

    let mut spec = template["spec"].clone();
    if let Some(s) = spec.as_object_mut() {
        // Bypass scheduler — place directly on node
        s.insert("nodeName".into(), Value::String(node_name.to_string()));
        // Add not-ready toleration so DaemonSet pods run on NotReady nodes too
        let toleration = json!({
            "key": "node.kubernetes.io/not-ready",
            "operator": "Exists",
            "effect": "NoExecute"
        });
        let tolerations = s
            .entry("tolerations")
            .or_insert_with(|| json!([]));
        if let Some(arr) = tolerations.as_array_mut() {
            arr.push(toleration);
        }
    }

    let pod = json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": pod_name,
            "namespace": namespace,
            "labels": labels,
            "ownerReferences": [{
                "apiVersion": "apps/v1",
                "kind": "DaemonSet",
                "name": ds_name,
                "uid": ds_uid,
                "controller": true,
                "blockOwnerDeletion": true
            }]
        },
        "spec": spec,
        "status": {
            "phase": "Pending"
        }
    });

    Ok(pod)
}

fn hash_node_name(name: &str) -> String {
    // Simple 5-char hash of node name for pod naming
    let mut hash: u64 = 5381;
    for b in name.bytes() {
        hash = hash.wrapping_mul(33).wrapping_add(b as u64);
    }
    format!("{:05x}", hash & 0xFFFFF)
}

fn is_node_ready(node: &Value) -> bool {
    node["status"]["conditions"]
        .as_array()
        .map(|conds| {
            conds.iter().any(|c| {
                c["type"].as_str() == Some("Ready") && c["status"].as_str() == Some("True")
            })
        })
        .unwrap_or(false)
}

fn is_pod_ready(pod: &Value) -> bool {
    pod["status"]["conditions"]
        .as_array()
        .map(|conds| {
            conds.iter().any(|c| {
                c["type"].as_str() == Some("Ready") && c["status"].as_str() == Some("True")
            })
        })
        .unwrap_or(false)
}
