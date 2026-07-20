//! DaemonSet controller.
//!
//! Ensures one pod runs on every Ready node. Pods are named
//! {ds-name}-{5-char-hash-of-node-name} and placed directly
//! (bypasses scheduler by setting spec.nodeName).

use crate::backoff::CreateBackoff;
use crate::runner::ApiClient;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;
use tokio::time::{self, Duration};
use tracing::{debug, error, info, warn};

pub struct DaemonSetController {
    api: Arc<ApiClient>,
    /// Per-(DaemonSet uid + node) recreation backoff after failed pods, so a
    /// node whose pod keeps failing is retried with widening spacing instead of
    /// a per-reconcile pod storm on that node.
    backoff: CreateBackoff,
}

impl DaemonSetController {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self {
            api,
            // Match the k8s DaemonSetController's failedPodsBackoff window: 1s
            // doubling to a 15min cap, keyed per (DaemonSet uid, node).
            backoff: CreateBackoff::with_params(Duration::from_secs(1), Duration::from_secs(900)),
        }
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

        // Get ALL nodes once (full objects, shared across namespaces). A DaemonSet
        // runs one pod on every node it's ELIGIBLE for — which includes NotReady
        // nodes (its pods carry the not-ready toleration). Scheduling/GC keys off
        // node existence + eligibility, not readiness; readiness only affects the
        // numberReady count. (Keying off readiness churned pods whenever a node
        // briefly went NotReady — #44.)
        let node_list: Value = self.api.list("/api/v1/nodes").await?;
        let nodes = node_list["items"].as_array().cloned().unwrap_or_default();

        for ns in &namespaces {
            let ns_name = ns["metadata"]["name"].as_str().unwrap_or("default");
            if let Err(e) = self.reconcile_namespace(ns_name, &nodes).await {
                debug!("DaemonSet reconcile in {ns_name}: {e}");
            }
        }
        Ok(())
    }

    async fn reconcile_namespace(
        &self,
        namespace: &str,
        nodes: &[Value],
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
                .reconcile_daemonset(namespace, ds, &pods, nodes)
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
        nodes: &[Value],
    ) -> anyhow::Result<()> {
        let ds_name = ds["metadata"]["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("daemonset missing name"))?;
        let ds_uid = ds["metadata"]["uid"].as_str().unwrap_or("");

        // Nodes this DaemonSet is eligible for (matches nodeSelector) — regardless
        // of readiness; its pods tolerate NotReady. Scheduling and GC key off this
        // set, so a node briefly going NotReady no longer churns its pod (#44).
        let eligible: Vec<String> = nodes
            .iter()
            .filter(|n| node_matches_ds(n, ds))
            .filter_map(|n| n["metadata"]["name"].as_str().map(|s| s.to_string()))
            .collect();
        let eligible_set: HashSet<&str> = eligible.iter().map(|s| s.as_str()).collect();

        // All pods owned by this DaemonSet.
        let owned: Vec<&Value> = all_pods
            .iter()
            .filter(|pod| {
                pod["metadata"]["ownerReferences"]
                    .as_array()
                    .map(|refs| refs.iter().any(|r| r["uid"].as_str() == Some(ds_uid)))
                    .unwrap_or(false)
            })
            .collect();

        // Partition into active (counts as "node has a pod") and terminal. A pod
        // already being deleted is going away — neither active nor a GC target.
        let mut active: Vec<&Value> = Vec::new();
        let mut terminal: Vec<&Value> = Vec::new();
        for pod in &owned {
            let phase = pod["status"]["phase"].as_str().unwrap_or("Pending");
            let deleting = !pod["metadata"]["deletionTimestamp"].is_null();
            if phase == "Succeeded" || phase == "Failed" {
                if !deleting {
                    terminal.push(pod);
                }
            } else if !deleting {
                active.push(pod);
            }
        }

        // Delete + recreate, matching the k8s DaemonSetController: a Failed pod
        // is DELETED (not retained) so the node stops looking occupied, then the
        // create pass below mints a fresh replacement — gated by failedPodsBackoff
        // so a crash-looping pod isn't recreated in a tight loop (the #27 storm
        // class). A Succeeded pod (unusual for a DaemonSet) is just cleared out.
        // Deleting pods are already excluded from `terminal`, so each Failed pod
        // arms this node's backoff exactly once.
        let now = Instant::now();
        for pod in &terminal {
            let node = pod["spec"]["nodeName"].as_str().unwrap_or("").to_string();
            if pod["status"]["phase"].as_str() == Some("Failed") {
                self.backoff.record_failure(&format!("{ds_uid}/{node}"), now);
            }
            let pod_name = pod["metadata"]["name"].as_str().unwrap_or("");
            if !pod_name.is_empty() {
                match self
                    .api
                    .delete(&format!("/api/v1/namespaces/{namespace}/pods/{pod_name}"))
                    .await
                {
                    Ok(_) => info!(
                        "Deleted terminal DaemonSet pod {namespace}/{pod_name} (node {node}); \
                         will recreate"
                    ),
                    Err(e) => debug!("Failed to delete terminal DaemonSet pod {pod_name}: {e}"),
                }
            }
        }

        // Nodes that already have an ACTIVE pod.
        let nodes_with_pods: HashSet<String> = active
            .iter()
            .filter_map(|pod| pod["spec"]["nodeName"].as_str().map(|s| s.to_string()))
            .collect();

        // Create pods on eligible nodes that need one (subject to per-node
        // backoff). Eligible includes NotReady nodes — DS pods run there too.
        for node_name in &eligible {
            if nodes_with_pods.contains(node_name) {
                continue;
            }
            if !self.backoff.allowed(&format!("{ds_uid}/{node_name}"), now) {
                debug!(
                    "DaemonSet {namespace}/{ds_name}: backing off pod creation on \
                     node {node_name} after repeated failures"
                );
                continue;
            }
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

        // Delete active pods only on nodes the DS is no longer eligible for — a
        // deleted node, or one that no longer matches the nodeSelector. A merely
        // NotReady node still keeps its pod (that's the whole point of a
        // DaemonSet), so it is NOT collected here.
        for pod in &active {
            let node_name = pod["spec"]["nodeName"].as_str().unwrap_or("");
            if !node_name.is_empty() && !eligible_set.contains(node_name) {
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

        // DaemonSet status, counted from the pods the DS actually owns (#44):
        //   desired          = eligible nodes
        //   currentScheduled = owned active pods placed on an eligible node
        //   ready/available  = those that are Ready
        //   misscheduled     = owned active pods on a node the DS isn't eligible for
        let desired = eligible.len();
        let current_scheduled = active
            .iter()
            .filter(|p| pod_on_eligible(p, &eligible_set))
            .count();
        let misscheduled = active.len() - current_scheduled;
        let ready_count = active
            .iter()
            .filter(|p| pod_on_eligible(p, &eligible_set) && is_pod_ready(p))
            .count();

        // A node running a Ready pod has recovered — clear its backoff so future
        // failures start from the base delay again.
        for pod in active.iter().filter(|p| is_pod_ready(p)) {
            if let Some(node) = pod["spec"]["nodeName"].as_str() {
                self.backoff.clear(&format!("{ds_uid}/{node}"));
            }
        }

        let mut updated = ds.clone();
        updated["status"] = json!({
            "desiredNumberScheduled": desired,
            "currentNumberScheduled": current_scheduled,
            "updatedNumberScheduled": current_scheduled,
            "numberReady": ready_count,
            "numberAvailable": ready_count,
            "numberUnavailable": desired.saturating_sub(ready_count),
            "numberMisscheduled": misscheduled,
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
    // Random suffix (k8s generateName style, e.g. `cilium-fd98f`) rather than a
    // deterministic per-node name: a lingering/terminating pod must never
    // collide (409) with its replacement — that collision was the self-healing
    // deadlock in #38. Node placement is tracked by spec.nodeName, not the name.
    let suffix = &uuid::Uuid::new_v4().to_string()[..5];
    let pod_name = format!("{ds_name}-{suffix}");

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

/// Whether an owned pod sits on a node the DaemonSet is still eligible for.
fn pod_on_eligible(pod: &Value, eligible: &HashSet<&str>) -> bool {
    pod["spec"]["nodeName"]
        .as_str()
        .map(|n| eligible.contains(n))
        .unwrap_or(false)
}

/// Whether `node` matches the DaemonSet's pod-template `nodeSelector` (every
/// key=value present in the node's labels). No selector matches every node.
fn node_matches_ds(node: &Value, ds: &Value) -> bool {
    let Some(sel) = ds["spec"]["template"]["spec"]["nodeSelector"].as_object() else {
        return true;
    };
    if sel.is_empty() {
        return true;
    }
    let labels = &node["metadata"]["labels"];
    sel.iter().all(|(k, v)| labels.get(k) == Some(v))
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
