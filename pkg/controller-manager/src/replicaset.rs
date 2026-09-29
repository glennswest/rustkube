//! ReplicaSet controller.
//!
//! Indexed ownership events drive per-ReplicaSet Pod reconciliation.
//! Creates pods from the template when under-provisioned, deletes excess pods
//! when over-provisioned.

use crate::backoff::CreateBackoff;
use crate::runner::ApiClient;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, info, warn};

pub struct ReplicaSetController {
    api: Arc<ApiClient>,
    /// Per-ReplicaSet (keyed by uid) recreation backoff after failed pods.
    backoff: CreateBackoff,
    recorder: crate::events::EventRecorder,
}

impl ReplicaSetController {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self {
            recorder: crate::events::EventRecorder::new(api.clone(), "replicaset-controller"),
            api,
            backoff: CreateBackoff::new(),
        }
    }

    pub async fn run(&self) {
        info!("ReplicaSet indexed object workers started");
        crate::owned::run(&self.api, self).await;
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

        // Being deleted: what it owns is the garbage collector's now. Making
        // replacements for the children a foreground delete removes races
        // that delete (upstream skips a deleting owner the same way; found by
        // the test container's gc-foreground check, #96).
        if !rs["metadata"]["deletionTimestamp"].is_null() {
            return Ok(());
        }
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
                    .delete_observed(
                        &format!("/api/v1/namespaces/{namespace}/pods/{pod_name}"),
                        pod,
                    )
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
                            let pod_name =
                                pod["metadata"]["name"].as_str().unwrap_or("?").to_string();
                            info!("Created pod {namespace}/{pod_name} for ReplicaSet {rs_name}");
                            self.recorder
                                .event(
                                    rs,
                                    "Normal",
                                    "SuccessfulCreate",
                                    &format!("Created pod: {pod_name}"),
                                )
                                .await;
                        }
                        Err(e) => {
                            warn!("Failed to create pod for {rs_name}: {e}");
                            self.recorder
                                .event(
                                    rs,
                                    "Warning",
                                    "FailedCreate",
                                    &format!("Error creating pod: {e}"),
                                )
                                .await;
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
                        .delete_observed(
                            &format!("/api/v1/namespaces/{namespace}/pods/{pod_name}"),
                            pod,
                        )
                        .await
                    {
                        Ok(_) => {
                            info!("Deleted pod {namespace}/{pod_name} (scale down {rs_name})");
                            self.recorder
                                .event(
                                    rs,
                                    "Normal",
                                    "SuccessfulDelete",
                                    &format!("Deleted pod: {pod_name}"),
                                )
                                .await;
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
            // status only: a whole-object PUT would carry a spec read
            // before the last reconcile and revert it.
            .update_status(
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

#[async_trait::async_trait]
impl crate::owned::Controller for ReplicaSetController {
    fn name(&self) -> &'static str {
        "replicaset"
    }
    fn primary(&self) -> &'static str {
        "/apis/apps/v1/replicasets"
    }
    fn children(&self) -> &'static str {
        "/api/v1/pods"
    }
    async fn reconcile(&self, object: &Value, children: &[Value]) -> anyhow::Result<()> {
        let namespace = object["metadata"]["namespace"]
            .as_str()
            .unwrap_or("default");
        self.reconcile_replicaset(namespace, object, children).await
    }
}
