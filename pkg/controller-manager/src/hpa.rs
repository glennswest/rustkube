//! Horizontal Pod Autoscaler (HPA) controller.
//!
//! **A placeholder** (#89). Indexed HorizontalPodAutoscaler workers
//! scales their targets (Deployments, ReplicaSets, StatefulSets), but reads no
//! metrics: "utilization" is the fraction of the target's pods that are Ready,
//! whatever resource the HPA names, and the desired count starts at the
//! current one and only rises, so it never scales down.

use crate::runner::ApiClient;
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::{info, warn};
use crate::owned::{self, Controller, Dependency, Deps};
use apimachinery::informer::Index;

pub struct HpaController {
    api: Arc<ApiClient>,
}

impl HpaController {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self { api }
    }

    pub async fn run(&self) {
        owned::run(&self.api, self).await;
    }

    async fn reconcile_hpa(&self, namespace: &str, hpa: &Value, deps: &Deps) -> anyhow::Result<()> {
        let hpa_name = hpa["metadata"]["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("HPA missing name"))?;
        let min_replicas = hpa["spec"]["minReplicas"].as_u64().unwrap_or(1) as usize;
        let max_replicas = hpa["spec"]["maxReplicas"].as_u64().unwrap_or(10) as usize;

        // Get scale target ref
        let target_ref = &hpa["spec"]["scaleTargetRef"];
        let target_kind = target_ref["kind"].as_str().unwrap_or("Deployment");
        let target_name = target_ref["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("HPA missing scaleTargetRef.name"))?;
        let target_api = match target_kind {
            "Deployment" | "ReplicaSet" | "StatefulSet" => "apis/apps/v1",
            _ => "apis/apps/v1",
        };
        let target_resource = match target_kind {
            "Deployment" => "deployments",
            "ReplicaSet" => "replicasets",
            "StatefulSet" => "statefulsets",
            other => {
                warn!("HPA {hpa_name}: unsupported target kind {other}");
                return Ok(());
            }
        };

        let i = match target_kind { "Deployment" => 0, "ReplicaSet" => 1, "StatefulSet" => 2, _ => return Ok(()) };
        let targets = deps.feed(i).select(&Index::Name(namespace.into(),target_name.into()))?;
        let Some(target_obj) = targets.first() else { return Ok(()); };
        let current_replicas = target_obj["spec"]["replicas"].as_u64().unwrap_or(1) as usize;
        let pods = deps.feed(3).select(&Index::Owner(target_obj["metadata"]["uid"].as_str().unwrap_or("").into()))?;

        // Count ready pods owned by target (simplified — real HPA uses metrics API)
        let target_uid = target_obj["metadata"]["uid"].as_str().unwrap_or("");
        let owned_pods: Vec<&Value> = pods
            .iter()
            .filter(|pod| {
                pod["metadata"]["ownerReferences"]
                    .as_array()
                    .map(|refs| refs.iter().any(|r| r["uid"].as_str() == Some(target_uid)))
                    .unwrap_or(false)
            })
            .filter(|pod| {
                let phase = pod["status"]["phase"].as_str().unwrap_or("");
                phase == "Running"
            })
            .collect();

        // Compute desired replicas from metrics
        let desired = self.compute_desired_replicas(hpa, &owned_pods, current_replicas);
        let desired = desired.clamp(min_replicas, max_replicas);

        if desired != current_replicas {
            info!(
                "HPA {namespace}/{hpa_name}: scaling {target_kind}/{target_name} from {current_replicas} to {desired}"
            );
            let mut updated = target_obj.clone();
            updated["spec"]["replicas"] = json!(desired);
            let _ = self
                .api
                .update(
                    &format!(
                        "/{target_api}/namespaces/{namespace}/{target_resource}/{target_name}"
                    ),
                    &updated,
                )
                .await;
        }

        // Update HPA status
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let mut updated_hpa = hpa.clone();
        updated_hpa["status"] = json!({
            "currentReplicas": current_replicas,
            "desiredReplicas": desired,
            "lastScaleTime": if desired != current_replicas { json!(now) } else { hpa["status"]["lastScaleTime"].clone() },
            "currentMetrics": [],
            "conditions": [{
                "type": "ScalingActive",
                "status": "True",
                "lastTransitionTime": now
            }]
        });
        owned::preserve_transition_times(&hpa["status"], &mut updated_hpa["status"]);
        if hpa["status"] == updated_hpa["status"] { return Ok(()); }
        let _ = self
            .api
            .update(
                &format!(
                    "/apis/autoscaling/v2/namespaces/{namespace}/horizontalpodautoscalers/{hpa_name}/status"
                ),
                &updated_hpa,
            )
            .await;

        Ok(())
    }

    fn compute_desired_replicas(
        &self,
        hpa: &Value,
        pods: &[&Value],
        current_replicas: usize,
    ) -> usize {
        let metrics = hpa["spec"]["metrics"].as_array();
        let metrics = match metrics {
            Some(m) => m,
            None => return current_replicas,
        };

        let mut max_desired = current_replicas;

        for metric in metrics {
            let metric_type = metric["type"].as_str().unwrap_or("");
            match metric_type {
                "Resource" => {
                    let resource_name = metric["resource"]["name"].as_str().unwrap_or("cpu");
                    let target_avg = metric["resource"]["target"]["averageUtilization"]
                        .as_u64()
                        .unwrap_or(80) as f64;

                    let current_util = self.get_average_utilization(pods, resource_name);
                    if current_util > 0.0 && target_avg > 0.0 {
                        let ratio = current_util / target_avg;
                        let desired = (current_replicas as f64 * ratio).ceil() as usize;
                        max_desired = max_desired.max(desired);
                    }
                }
                "Pods" => {
                    let target_avg = metric["pods"]["target"]["averageValue"]
                        .as_str()
                        .and_then(|v| v.parse::<f64>().ok())
                        .unwrap_or(100.0);
                    // Simplified: use running pod count as metric
                    let running = pods.len() as f64;
                    if running > 0.0 && target_avg > 0.0 {
                        let desired =
                            (current_replicas as f64 * (running / target_avg)).ceil() as usize;
                        max_desired = max_desired.max(desired);
                    }
                }
                _ => {}
            }
        }

        // Limit scale velocity: max 2x up, scale down by 1 at a time
        let scaled = if max_desired > current_replicas {
            std::cmp::min(max_desired, current_replicas * 2)
        } else if max_desired < current_replicas {
            current_replicas - 1
        } else {
            current_replicas
        };

        std::cmp::max(scaled, 1)
    }

    fn get_average_utilization(&self, pods: &[&Value], _resource: &str) -> f64 {
        if pods.is_empty() {
            return 0.0;
        }

        // Simplified metric: count pods in various states as a proxy for utilization
        // Real HPA would query metrics-server for actual CPU/memory usage
        let total_pods = pods.len() as f64;
        let ready_pods = pods
            .iter()
            .filter(|p| {
                p["status"]["conditions"]
                    .as_array()
                    .map(|c| {
                        c.iter().any(|cond| {
                            cond["type"].as_str() == Some("Ready")
                                && cond["status"].as_str() == Some("True")
                        })
                    })
                    .unwrap_or(false)
            })
            .count() as f64;

        // Estimate utilization as percentage of pods that are ready and presumably loaded
        if total_pods > 0.0 {
            (ready_pods / total_pods) * 100.0
        } else {
            0.0
        }
    }
}

#[async_trait::async_trait]
impl Controller for HpaController {
    fn name(&self) -> &'static str { "hpa" }
    fn primary(&self) -> &'static str { "/apis/autoscaling/v2/horizontalpodautoscalers" }
    fn dependencies(&self) -> Vec<Dependency> {
        let route: owned::Route = Arc::new(|delta, primary| delta.old.iter().chain(delta.new.iter()).flat_map(|o|
            owned::keys_at(primary,Index::Target(o["metadata"]["namespace"].as_str().unwrap_or("").into(),o["kind"].as_str().unwrap_or("").into(),o["metadata"]["name"].as_str().unwrap_or("").into()))).collect());
        vec![
            Dependency { path: "/apis/apps/v1/deployments".into(), route: route.clone() },
            Dependency { path: "/apis/apps/v1/replicasets".into(), route: route.clone() },
            Dependency { path: "/apis/apps/v1/statefulsets".into(), route },
            Dependency { path: "/api/v1/pods".into(), route: Arc::new(|delta,primary| {
                let mut keys = Vec::new();
                for pod in delta.old.iter().chain(delta.new.iter()) {
                    for owner in pod["metadata"]["ownerReferences"].as_array().into_iter().flatten() {
                        keys.extend(owned::keys_at(primary,Index::Target(pod["metadata"]["namespace"].as_str().unwrap_or("").into(),owner["kind"].as_str().unwrap_or("").into(),owner["name"].as_str().unwrap_or("").into())));
                    }
                }
                keys
            }) },
        ]
    }
    async fn reconcile(&self, hpa: &Value, _: &[Value], deps: &Deps) -> anyhow::Result<()> {
        self.reconcile_hpa(hpa["metadata"]["namespace"].as_str().unwrap_or("default"),hpa,deps).await
    }
}
