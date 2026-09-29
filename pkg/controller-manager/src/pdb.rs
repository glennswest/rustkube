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
use crate::owned::{self, Controller, Dependency, Deps};

pub struct PdbController {
    api: Arc<ApiClient>,
}

impl PdbController {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self { api }
    }

    pub async fn run(&self) {
        owned::run(&self.api, self).await;
    }

    async fn reconcile_pdb(&self, pdb: &Value, pods: &[Value]) -> anyhow::Result<()> {
        let namespace = pdb["metadata"]["namespace"].as_str().unwrap_or("default");
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
            if updated["status"] == pdb["status"] { return Ok(()); }
            self
                .api
                .update(
                    &format!(
                        "/apis/policy/v1/namespaces/{namespace}/poddisruptionbudgets/{name}/status"
                    ),
                    &updated,
                )
                .await?;
        Ok(())
    }
}

/// Selector match, including `matchExpressions` — see
/// [`apimachinery::selector`].
fn selector_matches(selector: &Value, labels: &Value) -> bool {
    apimachinery::selector::matches(selector, labels)
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

#[async_trait::async_trait]
impl Controller for PdbController {
    fn name(&self) -> &'static str { "pdb" }
    fn primary(&self) -> &'static str { "/apis/policy/v1/poddisruptionbudgets" }
    fn dependencies(&self) -> Vec<Dependency> {
        vec![Dependency { path: "/api/v1/pods", route: Arc::new(owned::pod_membership) }]
    }
    async fn reconcile(&self, pdb: &Value, _children: &[Value], deps: &Deps) -> anyhow::Result<()> {
        self.reconcile_pdb(pdb, &owned::selected_pods(pdb, deps.feed(0))?).await
    }
}
