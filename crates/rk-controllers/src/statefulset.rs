//! StatefulSet controller.
//!
//! Manages ordered creation and deletion of pods for StatefulSets.
//! Pods are named {sts-name}-{ordinal} and created sequentially
//! (each pod must be Running+Ready before the next is created).

use crate::runner::ApiClient;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::time::{self, Duration};
use tracing::{debug, error, info, warn};

pub struct StatefulSetController {
    api: Arc<ApiClient>,
}

impl StatefulSetController {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self { api }
    }

    pub async fn run(&self) {
        info!("StatefulSet controller started");
        let mut interval = time::interval(Duration::from_secs(2));

        loop {
            interval.tick().await;
            if let Err(e) = self.reconcile_all().await {
                error!("StatefulSet reconcile error: {e}");
            }
        }
    }

    async fn reconcile_all(&self) -> anyhow::Result<()> {
        let ns_list: Value = self.api.list("/api/v1/namespaces").await?;
        let namespaces = ns_list["items"].as_array().cloned().unwrap_or_default();

        for ns in &namespaces {
            let ns_name = ns["metadata"]["name"].as_str().unwrap_or("default");
            if let Err(e) = self.reconcile_namespace(ns_name).await {
                debug!("StatefulSet reconcile in {ns_name}: {e}");
            }
        }
        Ok(())
    }

    async fn reconcile_namespace(&self, namespace: &str) -> anyhow::Result<()> {
        let sts_list: Value = self
            .api
            .list(&format!(
                "/apis/apps/v1/namespaces/{namespace}/statefulsets"
            ))
            .await?;
        let statefulsets = sts_list["items"].as_array().cloned().unwrap_or_default();

        let pod_list: Value = self
            .api
            .list(&format!("/api/v1/namespaces/{namespace}/pods"))
            .await?;
        let pods = pod_list["items"].as_array().cloned().unwrap_or_default();

        for sts in &statefulsets {
            if let Err(e) = self.reconcile_statefulset(namespace, sts, &pods).await {
                let name = sts["metadata"]["name"].as_str().unwrap_or("?");
                warn!("Failed to reconcile statefulset {namespace}/{name}: {e}");
            }
        }
        Ok(())
    }

    async fn reconcile_statefulset(
        &self,
        namespace: &str,
        sts: &Value,
        all_pods: &[Value],
    ) -> anyhow::Result<()> {
        let sts_name = sts["metadata"]["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("statefulset missing name"))?;
        let sts_uid = sts["metadata"]["uid"].as_str().unwrap_or("");
        let desired = sts["spec"]["replicas"].as_u64().unwrap_or(1) as usize;
        let service_name = sts["spec"]["serviceName"].as_str().unwrap_or("");

        // Find pods owned by this StatefulSet
        let mut owned_pods: Vec<&Value> = all_pods
            .iter()
            .filter(|pod| {
                pod["metadata"]["ownerReferences"]
                    .as_array()
                    .map(|refs| refs.iter().any(|r| r["uid"].as_str() == Some(sts_uid)))
                    .unwrap_or(false)
            })
            .filter(|pod| {
                let phase = pod["status"]["phase"].as_str().unwrap_or("Pending");
                phase != "Succeeded" && phase != "Failed"
            })
            .collect();

        // Sort by ordinal index
        owned_pods.sort_by_key(|pod| {
            let name = pod["metadata"]["name"].as_str().unwrap_or("");
            extract_ordinal(name).unwrap_or(0)
        });

        let current = owned_pods.len();

        if current < desired {
            // Ordered creation: only create next if previous is Ready
            let next_ordinal = current;
            if next_ordinal > 0 {
                // Check if previous pod is Running+Ready
                if let Some(prev_pod) = owned_pods.last() {
                    if !is_pod_ready(prev_pod) {
                        debug!(
                            "StatefulSet {sts_name}: waiting for pod ordinal {} to be ready",
                            next_ordinal - 1
                        );
                        // Still update status with current count
                        self.update_status(namespace, sts_name, sts, &owned_pods)
                            .await;
                        return Ok(());
                    }
                }
            }

            let pod = build_statefulset_pod(
                namespace,
                sts_name,
                sts_uid,
                next_ordinal,
                service_name,
                sts,
            )?;
            match self
                .api
                .create(&format!("/api/v1/namespaces/{namespace}/pods"), &pod)
                .await
            {
                Ok(_) => {
                    info!(
                        "Created pod {namespace}/{sts_name}-{next_ordinal} for StatefulSet {sts_name}"
                    );
                }
                Err(e) => {
                    warn!("Failed to create pod for {sts_name}: {e}");
                }
            }
        } else if current > desired {
            // Reverse-ordered deletion: delete highest ordinal first
            if let Some(pod) = owned_pods.last() {
                let pod_name = pod["metadata"]["name"].as_str().unwrap_or("");
                let phase = pod["status"]["phase"].as_str().unwrap_or("");
                if phase != "Terminating" && !pod_name.is_empty() {
                    match self
                        .api
                        .delete(&format!(
                            "/api/v1/namespaces/{namespace}/pods/{pod_name}"
                        ))
                        .await
                    {
                        Ok(_) => {
                            info!(
                                "Deleted pod {namespace}/{pod_name} (scale down {sts_name})"
                            );
                        }
                        Err(e) => {
                            warn!("Failed to delete pod {pod_name}: {e}");
                        }
                    }
                }
            }
        }

        self.update_status(namespace, sts_name, sts, &owned_pods)
            .await;
        Ok(())
    }

    async fn update_status(
        &self,
        namespace: &str,
        sts_name: &str,
        sts: &Value,
        owned_pods: &[&Value],
    ) {
        let ready_count = owned_pods.iter().filter(|p| is_pod_ready(p)).count();
        let current = owned_pods.len();

        let mut updated = sts.clone();
        updated["status"] = json!({
            "replicas": current,
            "readyReplicas": ready_count,
            "currentReplicas": current,
            "updatedReplicas": current,
            "observedGeneration": sts["metadata"]["generation"].as_u64().unwrap_or(1)
        });

        let _ = self
            .api
            .update(
                &format!("/apis/apps/v1/namespaces/{namespace}/statefulsets/{sts_name}"),
                &updated,
            )
            .await;
    }
}

fn extract_ordinal(pod_name: &str) -> Option<usize> {
    pod_name.rsplit('-').next()?.parse().ok()
}

fn build_statefulset_pod(
    namespace: &str,
    sts_name: &str,
    sts_uid: &str,
    ordinal: usize,
    service_name: &str,
    sts: &Value,
) -> anyhow::Result<Value> {
    let template = &sts["spec"]["template"];
    let pod_name = format!("{sts_name}-{ordinal}");

    let mut labels = template["metadata"]["labels"].clone();
    if labels.is_null() {
        labels = json!({});
    }
    if let Some(map) = labels.as_object_mut() {
        map.insert(
            "statefulset.kubernetes.io/pod-name".into(),
            Value::String(pod_name.clone()),
        );
    }

    let mut pod = json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": pod_name,
            "namespace": namespace,
            "labels": labels,
            "ownerReferences": [{
                "apiVersion": "apps/v1",
                "kind": "StatefulSet",
                "name": sts_name,
                "uid": sts_uid,
                "controller": true,
                "blockOwnerDeletion": true
            }]
        },
        "spec": template["spec"],
        "status": {
            "phase": "Pending"
        }
    });

    // Set hostname and subdomain for stable network identity
    if let Some(spec) = pod["spec"].as_object_mut() {
        spec.insert("hostname".into(), Value::String(pod_name.clone()));
        if !service_name.is_empty() {
            spec.insert("subdomain".into(), Value::String(service_name.to_string()));
        }
    }

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
