//! Horizontal Pod Autoscaler (HPA) controller — **inert until there is a
//! metrics source** (#89).
//!
//! It used to read no metrics at all: "utilization" was the fraction of the
//! target's Pods that were Ready, and the desired count only rose, so every
//! HPA drove its target to `maxReplicas` and kept it there. Owner's decision
//! on #89: until a real HPA reads CPU/memory, this one changes no replica
//! counts and says why in its status, as upstream's does when the resource
//! metrics API answers nothing — `AbleToScale=True` (the target was read),
//! `ScalingActive=False` (`FailedGetResourceMetric`), `desiredReplicas` equal
//! to the current count. A target that cannot be read is `AbleToScale=False`
//! (`FailedGetScale`).

use crate::owned::{self, Controller, Dependency, Deps};
use crate::runner::ApiClient;
use apimachinery::informer::Index;
use serde_json::{json, Value};
use std::sync::Arc;

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
        let target_ref = &hpa["spec"]["scaleTargetRef"];
        let feed = match target_ref["kind"].as_str().unwrap_or("") {
            "Deployment" => Some(0),
            "ReplicaSet" => Some(1),
            "StatefulSet" => Some(2),
            _ => None,
        };
        let target = match (feed, target_ref["name"].as_str()) {
            (Some(i), Some(name)) => deps
                .feed(i)
                .select(&Index::Name(namespace.into(), name.into()))?
                .into_iter()
                .next(),
            _ => None,
        };
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let mut status = inert_status(hpa, target.as_ref(), &now);
        owned::preserve_transition_times(&hpa["status"], &mut status);
        if hpa["status"] == status {
            return Ok(());
        }
        let mut updated = hpa.clone();
        updated["status"] = status;
        self.api
            .update(
                &format!("/apis/autoscaling/v2/namespaces/{namespace}/horizontalpodautoscalers/{hpa_name}/status"),
                &updated,
            )
            .await?;
        Ok(())
    }
}

/// The resource the HPA's first metric names (upstream's default: cpu).
fn first_resource(hpa: &Value) -> String {
    hpa["spec"]["metrics"]
        .as_array()
        .and_then(|m| m.first())
        .map(|m| match m["type"].as_str() {
            Some("Resource") => m["resource"]["name"].as_str().unwrap_or("cpu").to_string(),
            Some("ContainerResource") => m["containerResource"]["name"].as_str().unwrap_or("cpu").to_string(),
            Some(other) => other.to_ascii_lowercase(),
            None => "cpu".to_string(),
        })
        .unwrap_or_else(|| "cpu".to_string())
}

/// The status an inert HPA reports: the target's count as both current and
/// desired, and conditions saying it cannot scale for want of metrics.
/// `lastScaleTime` is kept as it was; nothing scales.
pub(crate) fn inert_status(hpa: &Value, target: Option<&Value>, now: &str) -> Value {
    let condition = |kind: &str, status: &str, reason: &str, message: String| {
        json!({"type": kind, "status": status, "reason": reason, "message": message, "lastTransitionTime": now})
    };
    let (current, conditions) = match target {
        Some(t) => {
            let current = t["spec"]["replicas"].as_u64().unwrap_or(1);
            let resource = first_resource(hpa);
            (current, vec![
                condition("AbleToScale", "True", "SucceededGetScale",
                    "the HPA controller was able to get the target's current scale".into()),
                condition("ScalingActive", "False", "FailedGetResourceMetric", format!(
                    "the HPA was unable to compute the replica count: failed to get {resource} utilization: \
                     no metrics source: metrics.k8s.io is not served (rustkube#89)")),
            ])
        }
        None => {
            let r = &hpa["spec"]["scaleTargetRef"];
            (hpa["status"]["currentReplicas"].as_u64().unwrap_or(0), vec![condition(
                "AbleToScale", "False", "FailedGetScale",
                format!("the HPA controller was unable to get the target's current scale: {} {:?} not found or not supported",
                    r["kind"].as_str().unwrap_or(""), r["name"].as_str().unwrap_or("")),
            )])
        }
    };
    let mut status = json!({
        "currentReplicas": current,
        "desiredReplicas": current,
        "currentMetrics": [],
        "conditions": conditions,
    });
    if !hpa["status"]["lastScaleTime"].is_null() {
        status["lastScaleTime"] = hpa["status"]["lastScaleTime"].clone();
    }
    status
}

#[async_trait::async_trait]
impl Controller for HpaController {
    fn name(&self) -> &'static str {
        "hpa"
    }
    fn primary(&self) -> &'static str {
        "/apis/autoscaling/v2/horizontalpodautoscalers"
    }
    fn dependencies(&self) -> Vec<Dependency> {
        let route: owned::Route = Arc::new(|delta, primary| {
            delta
                .old
                .iter()
                .chain(delta.new.iter())
                .flat_map(|o| {
                    owned::keys_at(
                        primary,
                        Index::Target(
                            o["metadata"]["namespace"].as_str().unwrap_or("").into(),
                            o["kind"].as_str().unwrap_or("").into(),
                            o["metadata"]["name"].as_str().unwrap_or("").into(),
                        ),
                    )
                })
                .collect()
        });
        vec![
            Dependency {
                path: "/apis/apps/v1/deployments".into(),
                route: route.clone(),
            },
            Dependency {
                path: "/apis/apps/v1/replicasets".into(),
                route: route.clone(),
            },
            Dependency {
                path: "/apis/apps/v1/statefulsets".into(),
                route,
            },
        ]
    }
    async fn reconcile(&self, hpa: &Value, _: &[Value], deps: &Deps) -> anyhow::Result<()> {
        self.reconcile_hpa(
            hpa["metadata"]["namespace"].as_str().unwrap_or("default"),
            hpa,
            deps,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hpa(metrics: Value) -> Value {
        json!({"metadata": {"name": "h", "namespace": "ns"},
               "spec": {"scaleTargetRef": {"apiVersion": "apps/v1", "kind": "Deployment", "name": "web"},
                        "minReplicas": 1, "maxReplicas": 10, "metrics": metrics}})
    }

    #[test]
    fn an_hpa_changes_nothing_and_says_it_has_no_metrics() {
        // #89: whatever the Pods' state, the count stays the target's own.
        let deploy = json!({"spec": {"replicas": 3}});
        let s = inert_status(&hpa(json!([{"type": "Resource", "resource": {"name": "memory",
            "target": {"type": "Utilization", "averageUtilization": 50}}}])), Some(&deploy), "t");
        assert_eq!((s["currentReplicas"].as_u64(), s["desiredReplicas"].as_u64()), (Some(3), Some(3)));
        let c = s["conditions"].as_array().unwrap();
        assert_eq!((c[0]["type"].as_str(), c[0]["status"].as_str(), c[0]["reason"].as_str()),
                   (Some("AbleToScale"), Some("True"), Some("SucceededGetScale")));
        assert_eq!((c[1]["type"].as_str(), c[1]["status"].as_str(), c[1]["reason"].as_str()),
                   (Some("ScalingActive"), Some("False"), Some("FailedGetResourceMetric")));
        assert!(c[1]["message"].as_str().unwrap().contains("failed to get memory utilization"));
        assert!(s.get("lastScaleTime").is_none());
        // No metrics listed: upstream's default, cpu.
        let s = inert_status(&hpa(Value::Null), Some(&deploy), "t");
        assert!(s["conditions"][1]["message"].as_str().unwrap().contains("failed to get cpu utilization"));
    }

    #[test]
    fn a_missing_target_is_unable_to_scale_and_keeps_the_last_scale_time() {
        let mut h = hpa(Value::Null);
        h["status"] = json!({"currentReplicas": 4, "lastScaleTime": "2026-10-01T00:00:00Z"});
        let s = inert_status(&h, None, "t");
        assert_eq!((s["currentReplicas"].as_u64(), s["desiredReplicas"].as_u64()), (Some(4), Some(4)));
        assert_eq!(s["conditions"][0]["reason"], "FailedGetScale");
        assert_eq!(s["conditions"][0]["status"], "False");
        assert_eq!(s["lastScaleTime"], "2026-10-01T00:00:00Z");
    }
}
