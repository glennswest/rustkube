//! Deployment controller.
//!
//! Watches Deployments and manages ReplicaSets to implement rolling updates.
//! For each Deployment, ensures exactly one active ReplicaSet exists with
//! the correct pod template spec and replica count.

use crate::runner::ApiClient;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::time::{self, Duration};
use tracing::{debug, error, info, warn};

pub struct DeploymentController {
    api: Arc<ApiClient>,
}

impl DeploymentController {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self { api }
    }

    pub async fn run(&self) {
        info!("Deployment controller started");
        let mut interval = time::interval(Duration::from_secs(2));

        loop {
            interval.tick().await;
            if let Err(e) = self.reconcile_all().await {
                error!("Deployment reconcile error: {e}");
            }
        }
    }

    async fn reconcile_all(&self) -> anyhow::Result<()> {
        // List all namespaces
        let ns_list: Value = self.api.list("/api/v1/namespaces").await?;
        let namespaces = ns_list["items"].as_array().cloned().unwrap_or_default();

        for ns in &namespaces {
            let ns_name = ns["metadata"]["name"].as_str().unwrap_or("default");
            if let Err(e) = self.reconcile_namespace(ns_name).await {
                debug!("Deployment reconcile in {ns_name}: {e}");
            }
        }
        Ok(())
    }

    async fn reconcile_namespace(&self, namespace: &str) -> anyhow::Result<()> {
        let deploy_list: Value = self
            .api
            .list(&format!(
                "/apis/apps/v1/namespaces/{namespace}/deployments"
            ))
            .await?;
        let deployments = deploy_list["items"].as_array().cloned().unwrap_or_default();

        let rs_list: Value = self
            .api
            .list(&format!(
                "/apis/apps/v1/namespaces/{namespace}/replicasets"
            ))
            .await?;
        let replicasets = rs_list["items"].as_array().cloned().unwrap_or_default();

        for deploy in &deployments {
            if let Err(e) = self.reconcile_deployment(namespace, deploy, &replicasets).await {
                let name = deploy["metadata"]["name"].as_str().unwrap_or("?");
                warn!("Failed to reconcile deployment {namespace}/{name}: {e}");
            }
        }
        Ok(())
    }

    async fn reconcile_deployment(
        &self,
        namespace: &str,
        deploy: &Value,
        existing_rs: &[Value],
    ) -> anyhow::Result<()> {
        let deploy_name = deploy["metadata"]["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("deployment missing name"))?;
        let deploy_uid = deploy["metadata"]["uid"].as_str().unwrap_or("");
        let desired_replicas = deploy["spec"]["replicas"].as_u64().unwrap_or(1);
        let selector = &deploy["spec"]["selector"];
        let pod_template = &deploy["spec"]["template"];

        // Find ReplicaSets owned by this Deployment
        let owned_rs: Vec<&Value> = existing_rs
            .iter()
            .filter(|rs| {
                rs["metadata"]["ownerReferences"]
                    .as_array()
                    .map(|refs| {
                        refs.iter()
                            .any(|r| r["uid"].as_str() == Some(deploy_uid))
                    })
                    .unwrap_or(false)
            })
            .collect();

        // Hash the pod template to identify the current revision
        let template_hash = compute_template_hash(pod_template);
        let rs_name = format!("{deploy_name}-{template_hash}");

        // Find the ReplicaSet matching the current template
        let current_rs = owned_rs
            .iter()
            .find(|rs| rs["metadata"]["name"].as_str() == Some(&rs_name));

        if let Some(rs) = current_rs {
            // ReplicaSet exists — ensure replica count matches
            let current_replicas = rs["spec"]["replicas"].as_u64().unwrap_or(0);
            if current_replicas != desired_replicas {
                let mut updated_rs = (*rs).clone();
                updated_rs["spec"]["replicas"] = json!(desired_replicas);
                self.api
                    .update(
                        &format!(
                            "/apis/apps/v1/namespaces/{namespace}/replicasets/{rs_name}"
                        ),
                        &updated_rs,
                    )
                    .await?;
                info!(
                    "Scaled ReplicaSet {namespace}/{rs_name}: {current_replicas} -> {desired_replicas}"
                );
            }

            // Scale down old ReplicaSets
            for old_rs in &owned_rs {
                let old_name = old_rs["metadata"]["name"].as_str().unwrap_or("");
                if old_name != rs_name {
                    let old_replicas = old_rs["spec"]["replicas"].as_u64().unwrap_or(0);
                    if old_replicas > 0 {
                        let mut scaled = (*old_rs).clone();
                        scaled["spec"]["replicas"] = json!(0);
                        self.api
                            .update(
                                &format!(
                                    "/apis/apps/v1/namespaces/{namespace}/replicasets/{old_name}"
                                ),
                                &scaled,
                            )
                            .await?;
                        info!("Scaled down old ReplicaSet {namespace}/{old_name} to 0");
                    }
                }
            }
        } else {
            // Create the ReplicaSet
            let match_labels = selector["matchLabels"].clone();
            let mut rs_labels = match_labels.clone();
            if let Some(obj) = rs_labels.as_object_mut() {
                obj.insert("pod-template-hash".into(), json!(template_hash));
            }

            let rs = json!({
                "apiVersion": "apps/v1",
                "kind": "ReplicaSet",
                "metadata": {
                    "name": rs_name,
                    "namespace": namespace,
                    "labels": rs_labels,
                    "ownerReferences": [{
                        "apiVersion": "apps/v1",
                        "kind": "Deployment",
                        "name": deploy_name,
                        "uid": deploy_uid,
                        "controller": true,
                        "blockOwnerDeletion": true
                    }]
                },
                "spec": {
                    "replicas": desired_replicas,
                    "selector": {
                        "matchLabels": rs_labels
                    },
                    "template": pod_template
                }
            });

            self.api
                .create(
                    &format!(
                        "/apis/apps/v1/namespaces/{namespace}/replicasets"
                    ),
                    &rs,
                )
                .await?;
            info!("Created ReplicaSet {namespace}/{rs_name} with {desired_replicas} replicas");

            // Scale down old ReplicaSets
            for old_rs in &owned_rs {
                let old_name = old_rs["metadata"]["name"].as_str().unwrap_or("");
                let old_replicas = old_rs["spec"]["replicas"].as_u64().unwrap_or(0);
                if old_replicas > 0 {
                    let mut scaled = (*old_rs).clone();
                    scaled["spec"]["replicas"] = json!(0);
                    self.api
                        .update(
                            &format!(
                                "/apis/apps/v1/namespaces/{namespace}/replicasets/{old_name}"
                            ),
                            &scaled,
                        )
                        .await?;
                    info!("Scaled down old ReplicaSet {namespace}/{old_name} to 0");
                }
            }
        }

        // Update Deployment status
        let pod_list: Value = self
            .api
            .list(&format!(
                "/api/v1/namespaces/{namespace}/pods"
            ))
            .await?;
        let pods = pod_list["items"].as_array().cloned().unwrap_or_default();

        // Count pods owned by our ReplicaSets
        let rs_uids: Vec<&str> = owned_rs
            .iter()
            .filter_map(|rs| rs["metadata"]["uid"].as_str())
            .collect();
        let owned_pods: Vec<&Value> = pods
            .iter()
            .filter(|pod| {
                pod["metadata"]["ownerReferences"]
                    .as_array()
                    .map(|refs| {
                        refs.iter()
                            .any(|r| rs_uids.contains(&r["uid"].as_str().unwrap_or("")))
                    })
                    .unwrap_or(false)
            })
            .collect();

        let ready_pods = owned_pods
            .iter()
            .filter(|pod| is_pod_ready(pod))
            .count();

        let mut updated_deploy = deploy.clone();
        updated_deploy["status"] = json!({
            "replicas": owned_pods.len(),
            "readyReplicas": ready_pods,
            "updatedReplicas": desired_replicas,
            "availableReplicas": ready_pods,
            "observedGeneration": deploy["metadata"]["generation"].as_u64().unwrap_or(1),
            "conditions": [{
                "type": "Available",
                "status": if ready_pods > 0 { "True" } else { "False" },
                "reason": if ready_pods > 0 { "MinimumReplicasAvailable" } else { "MinimumReplicasUnavailable" },
                "lastTransitionTime": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
            }, {
                "type": "Progressing",
                "status": "True",
                "reason": "NewReplicaSetAvailable",
                "lastTransitionTime": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
            }]
        });

        let _ = self
            .api
            .update(
                &format!("/apis/apps/v1/namespaces/{namespace}/deployments/{deploy_name}"),
                &updated_deploy,
            )
            .await;

        Ok(())
    }
}

/// Compute a short hash of the pod template for the ReplicaSet name suffix.
fn compute_template_hash(template: &Value) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    let s = serde_json::to_string(template).unwrap_or_default();
    s.hash(&mut hasher);
    let h = hasher.finish();
    format!("{:010x}", h & 0xFF_FFFF_FFFF) // 10-char hex like K8s
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
