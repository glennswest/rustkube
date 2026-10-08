//! The ephemeral-volume controller (#94), as upstream's: a Pod's generic
//! ephemeral volume (`volumes[].ephemeral.volumeClaimTemplate`) gets its
//! PersistentVolumeClaim, `<pod>-<volume>` in the Pod's namespace, made from
//! the template — its metadata's labels and annotations, its spec — and
//! owned by the Pod (`controller: true`, `blockOwnerDeletion: true`), so the
//! garbage collector deletes it with the Pod.
//!
//! A claim of that name that the Pod does not own is left alone, never
//! adopted: the Pod gets a Warning Event saying so, as upstream refuses it,
//! and the kubelet keeps waiting. A Pod being deleted gets no new claims.
//! The scheduler and the attach/detach controller already look claims up by
//! the same `<pod>-<volume>` name.

use crate::owned::{self, Controller, Deps};
use crate::runner::ApiClient;
use serde_json::{json, Value};
use std::sync::Arc;

pub struct EphemeralVolumeController {
    api: Arc<ApiClient>,
    recorder: crate::events::EventRecorder,
}

impl EphemeralVolumeController {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self { recorder: crate::events::EventRecorder::new(api.clone(), "ephemeral-volume-controller"), api }
    }

    pub async fn run(&self) {
        owned::run(&self.api, self).await;
    }
}

/// `(volume name, claim)` for each generic ephemeral volume of `pod`: the
/// claim it should have, made from the volume's template.
pub fn wanted(pod: &Value) -> Vec<(String, Value)> {
    let ns = pod["metadata"]["namespace"].as_str().unwrap_or("default");
    let pod_name = pod["metadata"]["name"].as_str().unwrap_or("");
    pod["spec"]["volumes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|v| v["ephemeral"].is_object())
        .filter_map(|v| {
            let vol = v["name"].as_str()?.to_string();
            let tpl = &v["ephemeral"]["volumeClaimTemplate"];
            let mut metadata = json!({
                "name": format!("{pod_name}-{vol}"),
                "namespace": ns,
                "ownerReferences": [{"apiVersion": "v1", "kind": "Pod", "name": pod_name, "uid": pod["metadata"]["uid"],
                                     "controller": true, "blockOwnerDeletion": true}],
            });
            for k in ["labels", "annotations"] {
                if tpl["metadata"][k].is_object() {
                    metadata[k] = tpl["metadata"][k].clone();
                }
            }
            Some((vol, json!({"apiVersion": "v1", "kind": "PersistentVolumeClaim", "metadata": metadata,
                              "spec": tpl["spec"].clone()})))
        })
        .collect()
}

/// Is `claim` controlled by the Pod with this uid?
pub fn owned_by(claim: &Value, pod_uid: &str) -> bool {
    claim["metadata"]["ownerReferences"]
        .as_array()
        .is_some_and(|refs| refs.iter().any(|r| r["uid"].as_str() == Some(pod_uid) && r["controller"] == true))
}

#[async_trait::async_trait]
impl Controller for EphemeralVolumeController {
    fn name(&self) -> &'static str {
        "ephemeral-volume"
    }
    fn primary(&self) -> &'static str {
        "/api/v1/pods"
    }
    fn children(&self) -> Option<&'static str> {
        Some("/api/v1/persistentvolumeclaims")
    }
    async fn reconcile(&self, pod: &Value, children: &[Value], _deps: &Deps) -> anyhow::Result<()> {
        if !pod["metadata"]["deletionTimestamp"].is_null() {
            return Ok(());
        }
        let wanted = wanted(pod);
        if wanted.is_empty() {
            return Ok(());
        }
        let ns = pod["metadata"]["namespace"].as_str().unwrap_or("default");
        let uid = pod["metadata"]["uid"].as_str().unwrap_or("");
        for (vol, claim) in wanted {
            let name = claim["metadata"]["name"].as_str().unwrap_or("").to_string();
            if children.iter().any(|c| c["metadata"]["name"].as_str() == Some(&name)) {
                continue;
            }
            let path = format!("/api/v1/namespaces/{ns}/persistentvolumeclaims");
            let existing = self.api.get(&format!("{path}/{name}")).await?;
            if existing.status().is_success() {
                let current: Value = existing.json().await?;
                if !owned_by(&current, uid) {
                    let pod_name = pod["metadata"]["name"].as_str().unwrap_or("");
                    self.recorder
                        .event(pod, "Warning", "FailedCreate", &format!(
                            "PVC {ns}/{name} was not created for pod {ns}/{pod_name} (pod is not owner); ephemeral volume {vol} cannot use it"))
                        .await;
                }
                continue;
            }
            anyhow::ensure!(existing.status().as_u16() == 404, "reading {path}/{name}: {}", existing.status());
            self.api.create(&path, &claim).await?;
            tracing::info!("ephemeral volume {vol}: created claim {ns}/{name}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_claim_per_ephemeral_volume_from_its_template() {
        let pod = json!({"metadata": {"name": "web", "namespace": "ns", "uid": "u1"}, "spec": {"volumes": [
            {"name": "data", "ephemeral": {"volumeClaimTemplate": {
                "metadata": {"labels": {"type": "scratch"}, "annotations": {"a": "b"}},
                "spec": {"accessModes": ["ReadWriteOnce"], "storageClassName": "fast", "resources": {"requests": {"storage": "1Gi"}}}}}},
            {"name": "config", "configMap": {"name": "cm"}}]}});
        let w = wanted(&pod);
        assert_eq!(w.len(), 1);
        let (vol, claim) = &w[0];
        assert_eq!(vol, "data");
        assert_eq!(claim["metadata"]["name"], "web-data");
        assert_eq!(claim["metadata"]["namespace"], "ns");
        assert_eq!(claim["metadata"]["labels"], json!({"type": "scratch"}));
        assert_eq!(claim["metadata"]["annotations"], json!({"a": "b"}));
        assert_eq!(claim["spec"]["storageClassName"], "fast");
        let owner = &claim["metadata"]["ownerReferences"][0];
        assert_eq!((owner["kind"].as_str(), owner["uid"].as_str(), owner["controller"].as_bool()), (Some("Pod"), Some("u1"), Some(true)));
        assert!(owned_by(claim, "u1"));
        assert!(!owned_by(&json!({"metadata": {}}), "u1"));
        assert!(!owned_by(&json!({"metadata": {"ownerReferences": [{"uid": "u1", "controller": false}]}}), "u1"));
    }
}
