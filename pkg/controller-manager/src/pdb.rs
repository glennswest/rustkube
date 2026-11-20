//! PodDisruptionBudget status controller (#7).
//!
//! Computes each PDB's `status` (currentHealthy / desiredHealthy /
//! disruptionsAllowed / expectedPods) from its selector-matched pods, so
//! `kubectl get pdb` shows real numbers and the eviction subresource has a
//! budget to spend.
//!
//! The apiserver's eviction reserves a disruption in `status` before it
//! deletes a pod (#360): `disruptionsAllowed` minus one and the pod recorded
//! in `status.disruptedPods`. Here, as upstream's disruption controller, a
//! recorded pod that still exists counts as not healthy until it is deleted
//! or [`DISRUPTION_TIMEOUT`] passes, so the reservation is not handed out
//! again by the next recount; a record whose pod is gone, is terminating or
//! has expired is dropped.

use crate::owned::{self, Controller, Dependency, Deps};
use crate::runner::ApiClient;
use serde_json::{json, Value};
use std::sync::Arc;

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
        tracing::debug!(namespace, name = pdb["metadata"]["name"].as_str().unwrap_or(""),
            kind = pdb["kind"].as_str().unwrap_or(""), candidates = pods.len(), matched = matched.len(),
            revision = pdb["metadata"]["resourceVersion"].as_str().unwrap_or(""), "reconciling PDB membership");
        let expected = matched.len() as i64;
        let now = chrono::Utc::now();
        let (disrupted, next_expiry) = live_disruptions(&pdb["status"]["disruptedPods"], &matched, now);
        let healthy = matched
            .iter()
            .filter(|p| {
                is_ready(p)
                    && p["metadata"]["deletionTimestamp"].is_null()
                    && !disrupted.contains_key(p["metadata"]["name"].as_str().unwrap_or(""))
            })
            .count() as i64;
        if let Some(at) = next_expiry {
            // Recount when the oldest reservation lapses.
            apimachinery::reactor::requeue_at_time(at);
        }

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
        if !disrupted.is_empty() {
            updated["status"]["disruptedPods"] = Value::Object(disrupted);
        }
        if updated["status"] == pdb["status"] {
            return Ok(());
        }
        self.api
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

/// How long an eviction's reservation counts while its pod has not been
/// deleted: upstream's `DeletionTimeout`.
pub const DISRUPTION_TIMEOUT: chrono::Duration = chrono::Duration::seconds(120);

/// The `disruptedPods` records still in force: their pod is among `matched`,
/// not terminating, and recorded less than [`DISRUPTION_TIMEOUT`] ago. With
/// the time the first of them lapses.
fn live_disruptions(
    recorded: &Value,
    matched: &[&Value],
    now: chrono::DateTime<chrono::Utc>,
) -> (serde_json::Map<String, Value>, Option<chrono::DateTime<chrono::Utc>>) {
    let mut live = serde_json::Map::new();
    let mut next: Option<chrono::DateTime<chrono::Utc>> = None;
    for (name, at) in recorded.as_object().into_iter().flatten() {
        let Some(when) = at.as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()) else { continue };
        let expires = when.with_timezone(&chrono::Utc) + DISRUPTION_TIMEOUT;
        let present = matched.iter().any(|p| {
            p["metadata"]["name"].as_str() == Some(name.as_str()) && p["metadata"]["deletionTimestamp"].is_null()
        });
        if present && expires > now {
            live.insert(name.clone(), at.clone());
            next = Some(next.map_or(expires, |n| n.min(expires)));
        }
    }
    (live, next)
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
        .map(|cs| {
            cs.iter()
                .any(|c| c["type"] == "Ready" && c["status"] == "True")
        })
        .unwrap_or(false)
}

#[async_trait::async_trait]
impl Controller for PdbController {
    fn name(&self) -> &'static str {
        "pdb"
    }
    fn primary(&self) -> &'static str {
        "/apis/policy/v1/poddisruptionbudgets"
    }
    fn dependencies(&self) -> Vec<Dependency> {
        vec![Dependency {
            path: "/api/v1/pods".into(),
            route: Arc::new(owned::pod_membership),
        }]
    }
    async fn reconcile(&self, pdb: &Value, _children: &[Value], deps: &Deps) -> anyhow::Result<()> {
        self.reconcile_pdb(pdb, &owned::selected_pods(pdb, deps.feed(0))?)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reserved_disruption_counts_until_its_pod_goes_or_it_expires() {
        // #360: the apiserver's eviction recorded web-1 and web-2.
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-20T12:00:00Z").unwrap().with_timezone(&chrono::Utc);
        let pod = |n: &str, deleting: bool| {
            let mut p = json!({"metadata": {"name": n}, "status": {"conditions": [{"type": "Ready", "status": "True"}]}});
            if deleting {
                p["metadata"]["deletionTimestamp"] = json!("2026-11-20T11:59:59Z");
            }
            p
        };
        let pods = [pod("web-1", false), pod("web-2", true), pod("web-3", false), pod("web-4", false)];
        let matched: Vec<&Value> = pods.iter().collect();
        let recorded = json!({
            "web-1": "2026-11-20T11:59:30Z",   // 30 s ago: in force
            "web-2": "2026-11-20T11:59:30Z",   // its pod is terminating: dropped
            "web-3": "2026-11-20T11:57:00Z",   // 3 min ago: expired
            "gone":  "2026-11-20T11:59:50Z",   // no such pod: dropped
            "bad":   "yesterday"
        });
        let (live, next) = live_disruptions(&recorded, &matched, now);
        assert_eq!(live.keys().collect::<Vec<_>>(), ["web-1"]);
        assert_eq!(next.unwrap().to_rfc3339(), "2026-11-20T12:01:30+00:00");
        assert_eq!(live_disruptions(&Value::Null, &matched, now), (serde_json::Map::new(), None));
    }
}
