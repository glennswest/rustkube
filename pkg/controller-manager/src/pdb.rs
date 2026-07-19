//! PodDisruptionBudget status controller (#7).
//!
//! Computes each PDB's `status` (currentHealthy / desiredHealthy /
//! disruptionsAllowed / expectedPods) from its selector-matched pods, so
//! `kubectl get pdb` shows real numbers and the eviction path has status to
//! reflect. The eviction *decision* is enforced live in the apiserver; this
//! keeps the reported status current.

use crate::runner::ApiClient;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::time::{self, Duration};
use tracing::{debug, error, info};

pub struct PdbController {
    api: Arc<ApiClient>,
}

impl PdbController {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self { api }
    }

    pub async fn run(&self) {
        info!("PodDisruptionBudget controller started");
        let mut interval = time::interval(Duration::from_secs(5));
        loop {
            interval.tick().await;
            if let Err(e) = self.reconcile_all().await {
                error!("PDB reconcile error: {e}");
            }
        }
    }

    async fn reconcile_all(&self) -> anyhow::Result<()> {
        let ns_list: Value = self.api.list("/api/v1/namespaces").await?;
        for ns in ns_list["items"].as_array().cloned().unwrap_or_default() {
            let name = ns["metadata"]["name"].as_str().unwrap_or("default");
            if let Err(e) = self.reconcile_namespace(name).await {
                debug!("PDB reconcile in {name}: {e}");
            }
        }
        Ok(())
    }

    async fn reconcile_namespace(&self, namespace: &str) -> anyhow::Result<()> {
        let pdbs: Value = self
            .api
            .list(&format!(
                "/apis/policy/v1/namespaces/{namespace}/poddisruptionbudgets"
            ))
            .await?;
        let pdbs = pdbs["items"].as_array().cloned().unwrap_or_default();
        if pdbs.is_empty() {
            return Ok(());
        }
        let pods: Value = self
            .api
            .list(&format!("/api/v1/namespaces/{namespace}/pods"))
            .await?;
        let pods = pods["items"].as_array().cloned().unwrap_or_default();

        for pdb in &pdbs {
            let matched: Vec<&Value> = pods
                .iter()
                .filter(|p| selector_matches(&pdb["spec"]["selector"], &p["metadata"]["labels"]))
                .collect();
            let expected = matched.len() as i64;
            let healthy = matched.iter().filter(|p| is_ready(p)).count() as i64;

            let spec = &pdb["spec"];
            let desired = if let Some(min) = intstr_to_count(&spec["minAvailable"], expected) {
                min
            } else if let Some(max_u) = intstr_to_count(&spec["maxUnavailable"], expected) {
                expected - max_u
            } else {
                0
            };
            let allowed = (healthy - desired).max(0);

            let name = pdb["metadata"]["name"].as_str().unwrap_or("");
            let mut updated = pdb.clone();
            updated["status"] = json!({
                "currentHealthy": healthy,
                "desiredHealthy": desired,
                "disruptionsAllowed": allowed,
                "expectedPods": expected,
                "observedGeneration": pdb["metadata"]["generation"].as_u64().unwrap_or(1),
            });
            let _ = self
                .api
                .update(
                    &format!(
                        "/apis/policy/v1/namespaces/{namespace}/poddisruptionbudgets/{name}/status"
                    ),
                    &updated,
                )
                .await;
        }
        Ok(())
    }
}

/// matchLabels-only selector match; an empty selector matches everything.
fn selector_matches(selector: &Value, labels: &Value) -> bool {
    let ml = match selector.get("matchLabels").and_then(Value::as_object) {
        Some(m) => m,
        None => return selector.is_object(),
    };
    let pod_labels = labels.as_object();
    ml.iter().all(|(k, v)| {
        pod_labels
            .and_then(|pl| pl.get(k))
            .map(|pv| pv == v)
            .unwrap_or(false)
    })
}

fn intstr_to_count(v: &Value, total: i64) -> Option<i64> {
    if v.is_null() {
        return None;
    }
    if let Some(n) = v.as_i64() {
        return Some(n);
    }
    if let Some(s) = v.as_str() {
        if let Some(pct) = s.strip_suffix('%') {
            if let Ok(p) = pct.parse::<f64>() {
                return Some(((p / 100.0) * total as f64).ceil() as i64);
            }
        }
        if let Ok(n) = s.parse::<i64>() {
            return Some(n);
        }
    }
    None
}

fn is_ready(pod: &Value) -> bool {
    pod["status"]["conditions"]
        .as_array()
        .map(|cs| cs.iter().any(|c| c["type"] == "Ready" && c["status"] == "True"))
        .unwrap_or(false)
}
