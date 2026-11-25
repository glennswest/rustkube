//! StatefulSet controller.
//!
//! Manages ordered creation and deletion of pods for StatefulSets.
//! Pods are named {sts-name}-{ordinal} and created sequentially
//! (each pod must be Running+Ready before the next is created).

use crate::runner::ApiClient;
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::{debug, info, warn};

pub struct StatefulSetController {
    api: Arc<ApiClient>,
}

impl StatefulSetController {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self { api }
    }

    pub async fn run(&self) {
        info!("StatefulSet indexed object workers started");
        crate::owned::run(&self.api, self).await;
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

        // Being deleted: what it owns is the garbage collector's now. Making
        // replacements for the children a foreground delete removes races
        // that delete (upstream skips a deleting owner the same way; found by
        // the test container's gc-foreground check, #96).
        if !sts["metadata"]["deletionTimestamp"].is_null() {
            return Ok(());
        }
        let desired = sts["spec"]["replicas"].as_u64().unwrap_or(1) as usize;
        let service_name = sts["spec"]["serviceName"].as_str().unwrap_or("");

        // Find pods owned by this StatefulSet
        let mut owned_pods: Vec<&Value> = all_pods
            .iter()
            .filter(|pod| owned_by(pod, sts_uid))
            .filter(|pod| {
                let phase = pod["status"]["phase"].as_str().unwrap_or("Pending");
                phase != "Succeeded" && phase != "Failed"
            })
            .collect();

        // Sort by ordinal index
        owned_pods.sort_by_key(|pod| ordinal_of(sts_name, pod).unwrap_or(usize::MAX));
        let ordinals: std::collections::BTreeMap<usize, &Value> = owned_pods
            .iter()
            .filter_map(|pod| ordinal_of(sts_name, pod).map(|o| (o, *pod)))
            .collect();

        // A finished Pod holds its ordinal's name: deleted first, as
        // upstream's, and made again on a later pass. Counted as missing
        // without that, its create was refused AlreadyExists on every pass.
        let finished = all_pods.iter().find(|pod| {
            owned_by(pod, sts_uid)
                && matches!(pod["status"]["phase"].as_str(), Some("Succeeded" | "Failed"))
                && pod["metadata"]["deletionTimestamp"].is_null()
                && ordinal_of(sts_name, pod).is_some_and(|o| o < desired)
        });
        if let Some(pod) = finished {
            let pod_name = pod["metadata"]["name"].as_str().unwrap_or("");
            match self.api.delete_observed(&format!("/api/v1/namespaces/{namespace}/pods/{pod_name}"), pod).await {
                Ok(_) => info!("Deleted finished pod {namespace}/{pod_name} of StatefulSet {sts_name} to recreate it"),
                Err(e) => warn!("Failed to delete finished pod {pod_name}: {e}"),
            }
            self.update_status(namespace, sts_name, sts, &owned_pods).await;
            return Ok(());
        }

        // OrderedReady waits for each lower Pod to be available: Ready for
        // `minReadySeconds` (#379), and woken when it will be.
        let min_ready = crate::availability::min_ready_seconds(sts);
        let now = chrono::Utc::now();
        crate::availability::requeue_until_available(owned_pods.iter().copied(), min_ready);
        match plan(desired, &ordinals, |p| crate::availability::is_pod_available(p, min_ready, now)) {
            Step::Create(ordinal) => {
                let pod = build_statefulset_pod(namespace, sts_name, sts_uid, ordinal, service_name, sts)?;
                match self.api.create(&format!("/api/v1/namespaces/{namespace}/pods"), &pod).await {
                    Ok(_) => info!("Created pod {namespace}/{sts_name}-{ordinal} for StatefulSet {sts_name}"),
                    Err(e) => warn!("Failed to create pod {sts_name}-{ordinal}: {e}"),
                }
            }
            Step::WaitFor(ordinal) => {
                debug!("StatefulSet {sts_name}: waiting for pod ordinal {ordinal} to be available");
            }
            Step::Delete(ordinal) => {
                let pod = ordinals[&ordinal];
                let pod_name = pod["metadata"]["name"].as_str().unwrap_or("");
                if pod["metadata"]["deletionTimestamp"].is_null() && !pod_name.is_empty() {
                    match self
                        .api
                        .delete_observed(&format!("/api/v1/namespaces/{namespace}/pods/{pod_name}"), pod)
                        .await
                    {
                        Ok(_) => info!("Deleted pod {namespace}/{pod_name} (scale down {sts_name})"),
                        Err(e) => warn!("Failed to delete pod {pod_name}: {e}"),
                    }
                }
            }
            Step::Settled => {}
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
        let min_ready = crate::availability::min_ready_seconds(sts);
        let now = chrono::Utc::now();
        let available_count = owned_pods
            .iter()
            .filter(|p| crate::availability::is_pod_available(p, min_ready, now))
            .count();
        let current = owned_pods.len();

        let mut updated = sts.clone();
        updated["status"] = json!({
            "replicas": current,
            "readyReplicas": ready_count,
            "availableReplicas": available_count,
            "currentReplicas": current,
            "updatedReplicas": current,
            "observedGeneration": sts["metadata"]["generation"].as_u64().unwrap_or(1)
        });

        let _ = self
            .api
            // status only: a whole-object PUT would carry a spec read
            // before the last reconcile and revert it.
            .update_status(
                &format!("/apis/apps/v1/namespaces/{namespace}/statefulsets/{sts_name}"),
                &updated,
            )
            .await;
    }
}

fn owned_by(pod: &Value, uid: &str) -> bool {
    pod["metadata"]["ownerReferences"]
        .as_array()
        .is_some_and(|refs| refs.iter().any(|r| r["uid"].as_str() == Some(uid)))
}

/// A Pod's ordinal: `N` in `<sts>-N`. Any other name has none.
fn ordinal_of(sts_name: &str, pod: &Value) -> Option<usize> {
    let rest = pod["metadata"]["name"].as_str()?.strip_prefix(sts_name)?.strip_prefix('-')?;
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) || (rest.len() > 1 && rest.starts_with('0')) {
        return None;
    }
    rest.parse().ok()
}

/// One pass's step, as upstream's `OrderedReady` (review finding
/// c1b3be631b68, #363).
#[derive(Debug, PartialEq)]
enum Step {
    /// The lowest ordinal below `replicas` with no Pod.
    Create(usize),
    /// A lower ordinal is not Ready yet: nothing is created past it.
    WaitFor(usize),
    /// None missing: the highest ordinal at or above `replicas` goes.
    Delete(usize),
    Settled,
}

/// The next step for `replicas` Pods given the live ones by ordinal. The
/// next ordinal is the lowest one missing, not the count of Pods: with 0
/// and 2 present and 3 wanted, it is 1. The count re-created 2 on every
/// pass (AlreadyExists) and 1 never came.
fn plan(
    replicas: usize,
    ordinals: &std::collections::BTreeMap<usize, &Value>,
    available: impl Fn(&Value) -> bool,
) -> Step {
    if let Some(missing) = (0..replicas).find(|o| !ordinals.contains_key(o)) {
        return match ordinals.range(..missing).find(|(_, p)| !available(p)) {
            Some((&o, _)) => Step::WaitFor(o),
            None => Step::Create(missing),
        };
    }
    match ordinals.range(replicas..).next_back() {
        Some((&o, _)) => Step::Delete(o),
        None => Step::Settled,
    }
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
    crate::owned::copy_template_metadata(template, &mut pod);

    // Set hostname and subdomain for stable network identity
    if let Some(spec) = pod["spec"].as_object_mut() {
        spec.insert("hostname".into(), Value::String(pod_name.clone()));
        if !service_name.is_empty() {
            spec.insert("subdomain".into(), Value::String(service_name.to_string()));
        }
    }

    Ok(pod)
}

use crate::availability::is_pod_ready;

#[async_trait::async_trait]
impl crate::owned::Controller for StatefulSetController {
    fn name(&self) -> &'static str {
        "statefulset"
    }
    fn primary(&self) -> &'static str {
        "/apis/apps/v1/statefulsets"
    }
    fn children(&self) -> Option<&'static str> {
        Some("/api/v1/pods")
    }
    async fn reconcile(
        &self,
        object: &Value,
        children: &[Value],
        _deps: &crate::owned::Deps,
    ) -> anyhow::Result<()> {
        let namespace = object["metadata"]["namespace"]
            .as_str()
            .unwrap_or("default");
        self.reconcile_statefulset(namespace, object, children)
            .await
    }
}

#[cfg(test)]
mod template_metadata_tests {
    use super::*;

    #[test]
    fn a_pod_carries_its_template_s_annotations() {
        // #256: upstream's GetPodFromTemplate copies annotations and finalizers.
        let obj = json!({"spec": {"template": {"metadata": {"labels": {"app": "a"},
            "annotations": {"kubectl.kubernetes.io/default-container": "c"},
            "finalizers": ["example.com/keep"]},
            "spec": {"containers": [{"name": "c", "image": "i"}]}}}});
        let pod = build_statefulset_pod("ns", "s", "u", 0, "svc", &obj).unwrap();
        assert_eq!(pod["metadata"]["annotations"]["kubectl.kubernetes.io/default-container"], "c");
        assert_eq!(pod["metadata"]["finalizers"], json!(["example.com/keep"]));
        assert_eq!(pod["metadata"]["labels"]["app"], "a");
    }
}

#[cfg(test)]
mod ordinal_tests {
    use super::*;
    use std::collections::BTreeMap;

    fn pod(name: &str, ready: bool) -> Value {
        json!({"metadata": {"name": name},
               "status": {"phase": "Running", "conditions": [{"type": "Ready", "status": if ready { "True" } else { "False" }}]}})
    }

    #[test]
    fn the_lowest_missing_ordinal_is_made_next() {
        // Review finding c1b3be631b68: 0 and 2 present, 3 wanted.
        let (p0, p2) = (pod("web-0", true), pod("web-2", true));
        let ords: BTreeMap<usize, &Value> = [(0, &p0), (2, &p2)].into_iter().collect();
        assert_eq!(plan(3, &ords, is_pod_ready), Step::Create(1));
        // OrderedReady: not past a lower Pod that is not Ready.
        let p0_down = pod("web-0", false);
        let ords: BTreeMap<usize, &Value> = [(0, &p0_down), (2, &p2)].into_iter().collect();
        assert_eq!(plan(3, &ords, is_pod_ready), Step::WaitFor(0));
        // From nothing: 0.
        assert_eq!(plan(2, &BTreeMap::new(), is_pod_ready), Step::Create(0));
        // All present, one beyond replicas: the highest goes.
        let (p1, p3) = (pod("web-1", true), pod("web-3", true));
        let ords: BTreeMap<usize, &Value> = [(0, &p0), (1, &p1), (2, &p2), (3, &p3)].into_iter().collect();
        assert_eq!(plan(2, &ords, is_pod_ready), Step::Delete(3));
        assert_eq!(plan(4, &ords, is_pod_ready), Step::Settled);
        // A gap is filled before anything is scaled down.
        let ords: BTreeMap<usize, &Value> = [(0, &p0), (3, &p3)].into_iter().collect();
        assert_eq!(plan(2, &ords, is_pod_ready), Step::Create(1));
    }

    /// OrderedReady with `minReadySeconds` (#379): a lower Pod that is Ready
    /// but not yet available holds the next one back.
    #[test]
    fn the_next_ordinal_waits_for_the_last_to_be_available() {
        let mut p0 = pod("web-0", true);
        p0["status"]["conditions"][0]["lastTransitionTime"] = json!("2026-11-25T10:00:00Z");
        let ords: BTreeMap<usize, &Value> = [(0, &p0)].into_iter().collect();
        let at = |t: &str| chrono::DateTime::parse_from_rfc3339(t).unwrap().with_timezone(&chrono::Utc);
        let early = at("2026-11-25T10:00:03Z");
        assert_eq!(plan(2, &ords, |p| crate::availability::is_pod_available(p, 5, early)), Step::WaitFor(0));
        let late = at("2026-11-25T10:00:06Z");
        assert_eq!(plan(2, &ords, |p| crate::availability::is_pod_available(p, 5, late)), Step::Create(1));
    }

    #[test]
    fn an_ordinal_is_the_sets_own_suffix() {
        let o = |n: &str| ordinal_of("web", &json!({"metadata": {"name": n}}));
        assert_eq!(o("web-0"), Some(0));
        assert_eq!(o("web-12"), Some(12));
        assert_eq!(o("webby-1"), None);
        assert_eq!(o("web-x"), None);
        assert_eq!(o("web-01"), None);
        assert_eq!(o("web-"), None);
        assert_eq!(ordinal_of("my-db", &json!({"metadata": {"name": "my-db-2"}})), Some(2));
    }
}
