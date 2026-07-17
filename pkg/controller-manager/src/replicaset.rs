//! ReplicaSet controller.
//!
//! Watches ReplicaSets and manages Pods to maintain the desired replica count.
//! Creates pods from the template when under-provisioned, deletes excess pods
//! when over-provisioned.

use crate::backoff::CreateBackoff;
use crate::runner::ApiClient;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Instant;
use tokio::time::{self, Duration};
use tracing::{debug, error, info, warn};

pub struct ReplicaSetController {
    api: Arc<ApiClient>,
    /// Per-ReplicaSet (keyed by uid) recreation backoff after failed pods.
    backoff: CreateBackoff,
}

impl ReplicaSetController {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self {
            api,
            backoff: CreateBackoff::new(),
        }
    }

    pub async fn run(&self) {
        info!("ReplicaSet controller started");
        let mut interval = time::interval(Duration::from_secs(2));

        loop {
            interval.tick().await;
            if let Err(e) = self.reconcile_all().await {
                error!("ReplicaSet reconcile error: {e}");
            }
        }
    }

    async fn reconcile_all(&self) -> anyhow::Result<()> {
        let ns_list: Value = self.api.list("/api/v1/namespaces").await?;
        let namespaces = ns_list["items"].as_array().cloned().unwrap_or_default();

        for ns in &namespaces {
            let ns_name = ns["metadata"]["name"].as_str().unwrap_or("default");
            if let Err(e) = self.reconcile_namespace(ns_name).await {
                debug!("ReplicaSet reconcile in {ns_name}: {e}");
            }
        }
        Ok(())
    }

    async fn reconcile_namespace(&self, namespace: &str) -> anyhow::Result<()> {
        let rs_list: Value = self
            .api
            .list(&format!(
                "/apis/apps/v1/namespaces/{namespace}/replicasets"
            ))
            .await?;
        let replicasets = rs_list["items"].as_array().cloned().unwrap_or_default();

        let pod_list: Value = self
            .api
            .list(&format!("/api/v1/namespaces/{namespace}/pods"))
            .await?;
        let pods = pod_list["items"].as_array().cloned().unwrap_or_default();

        for rs in &replicasets {
            if let Err(e) = self.reconcile_replicaset(namespace, rs, &pods).await {
                let name = rs["metadata"]["name"].as_str().unwrap_or("?");
                warn!("Failed to reconcile replicaset {namespace}/{name}: {e}");
            }
        }
        Ok(())
    }

    async fn reconcile_replicaset(
        &self,
        namespace: &str,
        rs: &Value,
        all_pods: &[Value],
    ) -> anyhow::Result<()> {
        let rs_name = rs["metadata"]["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("replicaset missing name"))?;
        let rs_uid = rs["metadata"]["uid"].as_str().unwrap_or("");
        let desired = rs["spec"]["replicas"].as_u64().unwrap_or(1) as usize;

        // All pods owned by this ReplicaSet (by controller ownerReference).
        let owned: Vec<&Value> = all_pods
            .iter()
            .filter(|pod| {
                pod["metadata"]["ownerReferences"]
                    .as_array()
                    .map(|refs| refs.iter().any(|r| r["uid"].as_str() == Some(rs_uid)))
                    .unwrap_or(false)
            })
            .collect();

        // Partition into active (counts toward `replicas`) and terminal. A pod
        // already being deleted (deletionTimestamp set) is going away, so it is
        // neither active nor a GC candidate.
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

        // Garbage-collect terminal pods so they never accumulate (the root cause
        // of the pod storm in #27 — a Failed pod was excluded from the count but
        // never deleted, so every reconcile minted another replacement). Keep the
        // single newest terminal pod for post-mortem (`kubectl logs`), delete the
        // rest. Count a Failed pod as a failure to drive recreation backoff.
        terminal.sort_by(|a, b| {
            let ta = a["metadata"]["creationTimestamp"].as_str().unwrap_or("");
            let tb = b["metadata"]["creationTimestamp"].as_str().unwrap_or("");
            tb.cmp(ta)
        });
        let observed_failure = terminal
            .iter()
            .any(|p| p["status"]["phase"].as_str() == Some("Failed"));
        for pod in terminal.iter().skip(1) {
            let pod_name = pod["metadata"]["name"].as_str().unwrap_or("");
            if !pod_name.is_empty() {
                match self
                    .api
                    .delete(&format!("/api/v1/namespaces/{namespace}/pods/{pod_name}"))
                    .await
                {
                    Ok(_) => info!("GC terminal pod {namespace}/{pod_name} (ReplicaSet {rs_name})"),
                    Err(e) => debug!("Failed to GC terminal pod {pod_name}: {e}"),
                }
            }
        }

        let current = active.len();

        // Update recreation backoff from observed failures, and decide whether a
        // create is allowed right now.
        let now = Instant::now();
        if observed_failure {
            self.backoff.record_failure(rs_uid, now);
        } else if current >= desired {
            // Stable at desired with no terminal churn — clear any backoff.
            self.backoff.clear(rs_uid);
        }
        let create_allowed = self.backoff.allowed(rs_uid, now);

        if current < desired {
            if !create_allowed {
                debug!(
                    "ReplicaSet {namespace}/{rs_name}: backing off pod creation \
                     ({current}/{desired}) after repeated failures"
                );
            } else {
                // Scale up — create missing pods
                let to_create = desired - current;
                for _i in 0..to_create {
                    let pod = build_pod_from_template(namespace, rs_name, rs_uid, rs)?;
                    match self
                        .api
                        .create(&format!("/api/v1/namespaces/{namespace}/pods"), &pod)
                        .await
                    {
                        Ok(_) => {
                            let pod_name = pod["metadata"]["name"].as_str().unwrap_or("?");
                            info!("Created pod {namespace}/{pod_name} for ReplicaSet {rs_name}");
                        }
                        Err(e) => {
                            warn!("Failed to create pod for {rs_name}: {e}");
                        }
                    }
                }
            }
        } else if current > desired {
            // Scale down — delete excess pods (newest first)
            let to_delete = current - desired;
            let mut deletable: Vec<&Value> = active.clone();
            // Sort by creation timestamp descending (delete newest first)
            deletable.sort_by(|a, b| {
                let ta = a["metadata"]["creationTimestamp"].as_str().unwrap_or("");
                let tb = b["metadata"]["creationTimestamp"].as_str().unwrap_or("");
                tb.cmp(ta)
            });

            for pod in deletable.iter().take(to_delete) {
                let pod_name = pod["metadata"]["name"].as_str().unwrap_or("");
                if !pod_name.is_empty() {
                    match self
                        .api
                        .delete(&format!("/api/v1/namespaces/{namespace}/pods/{pod_name}"))
                        .await
                    {
                        Ok(_) => {
                            info!("Deleted pod {namespace}/{pod_name} (scale down {rs_name})");
                        }
                        Err(e) => {
                            warn!("Failed to delete pod {pod_name}: {e}");
                        }
                    }
                }
            }
        }

        // Update ReplicaSet status
        let ready_count = active.iter().filter(|pod| is_pod_ready(pod)).count();

        let mut updated_rs = rs.clone();
        updated_rs["status"] = json!({
            "replicas": current,
            "readyReplicas": ready_count,
            "availableReplicas": ready_count,
            "observedGeneration": rs["metadata"]["generation"].as_u64().unwrap_or(1)
        });

        let _ = self
            .api
            .update(
                &format!("/apis/apps/v1/namespaces/{namespace}/replicasets/{rs_name}"),
                &updated_rs,
            )
            .await;

        Ok(())
    }
}

/// Build a Pod object from a ReplicaSet's pod template.
fn build_pod_from_template(
    namespace: &str,
    rs_name: &str,
    rs_uid: &str,
    rs: &Value,
) -> anyhow::Result<Value> {
    let template = &rs["spec"]["template"];
    let suffix = &uuid::Uuid::new_v4().to_string()[..5];
    let pod_name = format!("{rs_name}-{suffix}");

    let mut labels = template["metadata"]["labels"].clone();
    if labels.is_null() {
        labels = json!({});
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
                "kind": "ReplicaSet",
                "name": rs_name,
                "uid": rs_uid,
                "controller": true,
                "blockOwnerDeletion": true
            }]
        },
        "spec": template["spec"],
        "status": {
            "phase": "Pending"
        }
    });

    Ok(pod)
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

#[cfg(test)]
mod tests {
    use super::*;

    // Classify pods exactly as reconcile does, to lock in the #27 fix: a Failed
    // pod must NOT count as active (else the RS is "under" desired and storms),
    // and it must be visible as terminal (so it gets GC'd).
    fn classify<'a>(owned: &'a [Value]) -> (Vec<&'a Value>, Vec<&'a Value>) {
        let mut active = Vec::new();
        let mut terminal = Vec::new();
        for pod in owned {
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
        (active, terminal)
    }

    #[test]
    fn failed_pod_is_terminal_not_active() {
        let owned = vec![
            json!({"metadata":{"name":"a"},"status":{"phase":"Running"}}),
            json!({"metadata":{"name":"b"},"status":{"phase":"Failed"}}),
            json!({"metadata":{"name":"c"},"status":{"phase":"Succeeded"}}),
            json!({"metadata":{"name":"d"},"status":{"phase":"Pending"}}),
        ];
        let (active, terminal) = classify(&owned);
        // Running + Pending are active; Failed + Succeeded are terminal (GC).
        assert_eq!(active.len(), 2);
        assert_eq!(terminal.len(), 2);
    }

    #[test]
    fn deleting_pod_is_neither_active_nor_gc_candidate() {
        let owned = vec![
            json!({"metadata":{"name":"x","deletionTimestamp":"2026-07-17T00:00:00Z"},
                   "status":{"phase":"Running"}}),
            json!({"metadata":{"name":"y","deletionTimestamp":"2026-07-17T00:00:00Z"},
                   "status":{"phase":"Failed"}}),
        ];
        let (active, terminal) = classify(&owned);
        assert!(active.is_empty(), "deleting pod must not count as active");
        assert!(terminal.is_empty(), "deleting pod must not be re-deleted");
    }
}
