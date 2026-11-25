//! ReplicaSet controller, and the ReplicationController controller (#125).
//!
//! Indexed ownership events drive per-ReplicaSet Pod reconciliation.
//! Creates pods from the template when under-provisioned, deletes excess pods
//! when over-provisioned.
//!
//! A core/v1 ReplicationController is a ReplicaSet with an equality-only
//! selector, as upstream's own controller treats it: the same reconcile runs
//! over `/api/v1/replicationcontrollers` ([`Kind::REPLICATION_CONTROLLER`]),
//! its Pods owned by kind `ReplicationController`, its status written to
//! `replicationcontrollers/status`.
//!
//! Each pass claims its Pods first (#295), as upstream's
//! `ControllerRefManager.ClaimPods`: a Pod it controls whose labels no longer
//! match the selector is released (its ownerReference removed), and an
//! orphan Pod (no controller) that matches, and is not being deleted, is
//! adopted. Only what it then controls is counted.

use crate::backoff::CreateBackoff;
use crate::runner::ApiClient;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, info, warn};

/// Which of the two workload kinds a controller runs over.
#[derive(Clone, Copy, Debug)]
pub struct Kind {
    name: &'static str,
    /// The API path the kind is served under, e.g. `/apis/apps/v1`.
    base: &'static str,
    plural: &'static str,
    api_version: &'static str,
    kind: &'static str,
    component: &'static str,
}

impl Kind {
    pub const REPLICA_SET: Kind = Kind {
        name: "replicaset", base: "/apis/apps/v1", plural: "replicasets", api_version: "apps/v1",
        kind: "ReplicaSet", component: "replicaset-controller",
    };
    pub const REPLICATION_CONTROLLER: Kind = Kind {
        name: "replicationcontroller", base: "/api/v1", plural: "replicationcontrollers", api_version: "v1",
        kind: "ReplicationController", component: "replication-controller",
    };
}

pub struct ReplicaSetController {
    api: Arc<ApiClient>,
    kind: Kind,
    primary: String,
    /// Per-ReplicaSet (keyed by uid) recreation backoff after failed pods.
    backoff: CreateBackoff,
    recorder: crate::events::EventRecorder,
}

impl ReplicaSetController {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self::of(api, Kind::REPLICA_SET)
    }

    pub fn of(api: Arc<ApiClient>, kind: Kind) -> Self {
        Self {
            recorder: crate::events::EventRecorder::new(api.clone(), kind.component),
            api,
            kind,
            primary: format!("{}/{}", kind.base, kind.plural),
            backoff: CreateBackoff::new(),
        }
    }

    pub async fn run(&self) {
        info!("{} indexed object workers started", self.kind.kind);
        crate::owned::run(&self.api, self).await;
    }

    async fn reconcile_replicaset(
        &self,
        namespace: &str,
        rs: &Value,
        children: &[Value],
        candidates: &[Value],
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

        // The Pods it controls, after adopting and releasing (#295).
        let owned_pods = self.claim_pods(namespace, rs, children, candidates).await?;
        let owned: Vec<&Value> = owned_pods.iter().collect();

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
        let failed: Vec<&str> = terminal
            .iter()
            .filter(|p| p["status"]["phase"].as_str() == Some("Failed"))
            .filter_map(|p| p["metadata"]["uid"].as_str())
            .collect();
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
        // The newest Failed pod is kept, so it is seen on every pass; it
        // counts once.
        let new_failure = self.backoff.observe_failures(rs_uid, &failed, now);
        if !new_failure && current >= desired {
            // Stable at desired with no terminal churn — clear any backoff.
            self.backoff.clear(rs_uid);
        }
        let create_allowed = self.backoff.allowed(rs_uid, now);
        // What this pass tried to change and the first refusal (#125):
        // upstream's manageReplicas error, for the ReplicaFailure condition.
        let mut attempted = false;
        let mut failure: Option<(&'static str, String)> = None;

        if current < desired {
            if !create_allowed {
                debug!(
                    "ReplicaSet {namespace}/{rs_name}: backing off pod creation \
                     ({current}/{desired}) after repeated failures"
                );
            } else {
                // Scale up — create missing pods
                let to_create = desired - current;
                attempted = true;
                for _i in 0..to_create {
                    let pod = build_pod_from_template(namespace, rs_name, rs_uid, rs, &self.kind)?;
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
                            failure.get_or_insert(("FailedCreate", e.to_string()));
                            self.recorder
                                .event(
                                    rs,
                                    "Warning",
                                    "FailedCreate",
                                    &create_failure(&e),
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

            attempted = true;
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
                            failure.get_or_insert(("FailedDelete", e.to_string()));
                        }
                    }
                }
            }
        }

        // Update ReplicaSet status
        let ready_count = active.iter().filter(|pod| is_pod_ready(pod)).count();
        // Available: Ready for `minReadySeconds` (#379). A Ready pod not yet
        // there wakes this reconcile when it will be.
        let min_ready = crate::availability::min_ready_seconds(rs);
        let now = chrono::Utc::now();
        let available_count = active
            .iter()
            .filter(|pod| crate::availability::is_pod_available(pod, min_ready, now))
            .count();
        crate::availability::requeue_until_available(active.iter().copied(), min_ready);
        // Pods whose labels carry every label of the template (upstream's
        // fullyLabeledReplicas).
        let template_labels = rs["spec"]["template"]["metadata"]["labels"].as_object();
        let fully_labeled = active
            .iter()
            .filter(|pod| {
                template_labels.is_none_or(|want| {
                    want.iter().all(|(k, v)| pod["metadata"]["labels"].get(k) == Some(v))
                })
            })
            .count();

        let mut updated_rs = rs.clone();
        updated_rs["status"] = json!({
            "replicas": current,
            "fullyLabeledReplicas": fully_labeled,
            "readyReplicas": ready_count,
            "availableReplicas": available_count,
            "observedGeneration": rs["metadata"]["generation"].as_u64().unwrap_or(1)
        });
        let settled = attempted || current == desired;
        let conditions = failure_conditions(&rs["status"]["conditions"], failure.as_ref(), settled);
        if conditions.as_array().is_some_and(|c| !c.is_empty()) {
            updated_rs["status"]["conditions"] = conditions;
        }

        let _ = self
            .api
            // status only: a whole-object PUT would carry a spec read
            // before the last reconcile and revert it.
            .update_status(
                &format!("{}/namespaces/{namespace}/{}/{rs_name}", self.kind.base, self.kind.plural),
                &updated_rs,
            )
            .await;

        Ok(())
    }
}

impl ReplicaSetController {
    /// Release, then adopt, as upstream's `ClaimPods` (#295); the Pods it
    /// controls afterwards. A write refused for any reason but the Pod being
    /// gone fails the pass, which is retried: counting a Pod it failed to
    /// adopt as missing would make a replacement for it.
    async fn claim_pods(
        &self,
        namespace: &str,
        rs: &Value,
        children: &[Value],
        candidates: &[Value],
    ) -> anyhow::Result<Vec<Value>> {
        let rs_uid = rs["metadata"]["uid"].as_str().unwrap_or("");
        let rs_name = rs["metadata"]["name"].as_str().unwrap_or("");
        let selector = workload_selector(&self.kind, rs);
        let plan = claim(rs_uid, selector.as_ref(), children, candidates);
        let mut owned: Vec<Value> = plan.keep.into_iter().cloned().collect();
        let pod_path = |pod: &Value| {
            format!("/api/v1/namespaces/{namespace}/pods/{}", pod["metadata"]["name"].as_str().unwrap_or(""))
        };
        for pod in plan.release {
            let refs: Vec<Value> = owner_refs(pod).into_iter().filter(|r| r["uid"].as_str() != Some(rs_uid)).collect();
            let patch = json!({"metadata": {"resourceVersion": pod["metadata"]["resourceVersion"], "ownerReferences": refs}});
            match self.api.patch_merge(&pod_path(pod), &patch).await {
                Ok(_) => info!("Released pod {} from {} {namespace}/{rs_name}: its labels no longer match", pod_path(pod), self.kind.kind),
                Err(e) if gone(&e) => {}
                Err(e) => return Err(e.context(format!("releasing {}", pod_path(pod)))),
            }
        }
        if plan.adopt.is_empty() {
            return Ok(owned);
        }
        // Upstream checks the owner itself, uncached, before adopting: a
        // ReplicaSet deleted or recreated since the feed saw it must not
        // take Pods.
        let path = format!("{}/namespaces/{namespace}/{}/{rs_name}", self.kind.base, self.kind.plural);
        let fresh: Value = self.api.get(&path).await?.json().await?;
        if fresh["metadata"]["uid"].as_str() != Some(rs_uid) || !fresh["metadata"]["deletionTimestamp"].is_null() {
            debug!("{} {namespace}/{rs_name} changed before adopting; not adopting", self.kind.kind);
            return Ok(owned);
        }
        let ours = owner_ref(&self.kind, rs_name, rs_uid);
        for pod in plan.adopt {
            let mut refs = owner_refs(pod);
            refs.push(ours.clone());
            let patch = json!({"metadata": {"resourceVersion": pod["metadata"]["resourceVersion"], "ownerReferences": refs}});
            match self.api.patch_merge(&pod_path(pod), &patch).await {
                Ok(adopted) => {
                    info!("Adopted pod {} into {} {namespace}/{rs_name}", pod_path(pod), self.kind.kind);
                    owned.push(adopted);
                }
                Err(e) if gone(&e) => {}
                Err(e) => return Err(e.context(format!("adopting {}", pod_path(pod)))),
            }
        }
        Ok(owned)
    }
}

/// A refused create's event message, upstream's: `Error creating: ` and the
/// apiserver's own reason (#340), e.g. `pods "x" is forbidden: unable to
/// validate against any security context constraint: …`.
fn create_failure(e: &anyhow::Error) -> String {
    format!("Error creating: {e}")
}

/// A write to a Pod that no longer exists.
fn gone(e: &anyhow::Error) -> bool {
    apimachinery::api_status::status_of(e) == Some(reqwest::StatusCode::NOT_FOUND)
}

fn owner_refs(pod: &Value) -> Vec<Value> {
    pod["metadata"]["ownerReferences"].as_array().cloned().unwrap_or_default()
}

/// The uid of the Pod's controller, if it has one.
fn controller_uid(pod: &Value) -> Option<&str> {
    pod["metadata"]["ownerReferences"]
        .as_array()?
        .iter()
        .find(|r| r["controller"].as_bool() == Some(true))
        .and_then(|r| r["uid"].as_str())
}

/// The controller reference a ReplicaSet's Pods carry.
fn owner_ref(kind: &Kind, rs_name: &str, rs_uid: &str) -> Value {
    json!({
        "apiVersion": kind.api_version,
        "kind": kind.kind,
        "name": rs_name,
        "uid": rs_uid,
        "controller": true,
        "blockOwnerDeletion": true
    })
}

/// The label selector its Pods match (#295): a ReplicaSet's
/// `spec.selector`; an RC's label map, or its template's labels when it names
/// none. Read by this controller's kind, not the object's `kind` field. None
/// for an empty selector, which would claim every Pod in the namespace.
fn workload_selector(kind: &Kind, rs: &Value) -> Option<Value> {
    let mut typed = rs.clone();
    typed["kind"] = json!(kind.kind);
    let selector = apimachinery::informer::pod_selector(&typed)?;
    let empty = |field: &str| match &selector[field] {
        Value::Object(m) => m.is_empty(),
        Value::Array(a) => a.is_empty(),
        _ => true,
    };
    (!(empty("matchLabels") && empty("matchExpressions"))).then_some(selector)
}

/// What one pass does with the Pods (#295), as upstream's `ClaimObject`.
#[derive(Debug, Default)]
struct Claim<'a> {
    /// Controlled by it and still matching.
    keep: Vec<&'a Value>,
    /// Controlled by it, labels no longer matching: its reference goes.
    release: Vec<&'a Value>,
    /// No controller, matching, not being deleted: it becomes the controller.
    adopt: Vec<&'a Value>,
}

/// `children` name it as an owner; `candidates` match its selector. A Pod
/// another controller controls is left alone either way. Without a usable
/// selector it keeps what it controls and claims nothing.
fn claim<'a>(rs_uid: &str, selector: Option<&Value>, children: &'a [Value], candidates: &'a [Value]) -> Claim<'a> {
    let mut plan = Claim::default();
    let matches = |pod: &Value| selector.is_some_and(|s| apimachinery::selector::matches(s, &pod["metadata"]["labels"]));
    for pod in children {
        if rs_uid.is_empty() || controller_uid(pod) != Some(rs_uid) {
            continue;
        }
        if selector.is_none() || matches(pod) {
            plan.keep.push(pod);
        } else {
            plan.release.push(pod);
        }
    }
    if rs_uid.is_empty() {
        return plan;
    }
    for pod in candidates {
        if controller_uid(pod).is_none() && pod["metadata"]["deletionTimestamp"].is_null() && matches(pod) {
            plan.adopt.push(pod);
        }
    }
    plan
}

/// The status conditions after a pass, as upstream's calculateStatus (#125):
/// a refused create or delete adds `ReplicaFailure` (True, its reason and the
/// API's message) unless one is there already, which keeps its transition
/// time; a pass that `settled` (tried and was not refused, or is at the
/// desired count) removes it; a pass that tried nothing (recreate backoff)
/// leaves it.
fn failure_conditions(existing: &Value, failure: Option<&(&'static str, String)>, settled: bool) -> Value {
    let mut conditions: Vec<Value> = existing.as_array().cloned().unwrap_or_default();
    let has = conditions.iter().any(|c| c["type"] == "ReplicaFailure");
    match failure {
        Some((reason, message)) if !has => conditions.push(json!({
            "type": "ReplicaFailure", "status": "True", "reason": reason, "message": message,
            "lastTransitionTime": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        })),
        None if settled => conditions.retain(|c| c["type"] != "ReplicaFailure"),
        _ => {}
    }
    Value::Array(conditions)
}

/// Build a Pod object from a ReplicaSet's pod template.
fn build_pod_from_template(
    namespace: &str,
    rs_name: &str,
    rs_uid: &str,
    rs: &Value,
    kind: &Kind,
) -> anyhow::Result<Value> {
    let template = &rs["spec"]["template"];
    let suffix = &uuid::Uuid::new_v4().to_string()[..5];
    let pod_name = format!("{rs_name}-{suffix}");

    let mut labels = template["metadata"]["labels"].clone();
    if labels.is_null() {
        labels = json!({});
    }

    let mut pod = json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": pod_name,
            "namespace": namespace,
            "labels": labels,
            "ownerReferences": [owner_ref(kind, rs_name, rs_uid)]
        },
        "spec": template["spec"],
        "status": {
            "phase": "Pending"
        }
    });
    crate::owned::copy_template_metadata(template, &mut pod);

    Ok(pod)
}

use crate::availability::is_pod_ready;

#[async_trait::async_trait]
impl crate::owned::Controller for ReplicaSetController {
    fn name(&self) -> &'static str {
        self.kind.name
    }
    fn primary(&self) -> &str {
        &self.primary
    }
    fn children(&self) -> Option<&'static str> {
        Some("/api/v1/pods")
    }
    /// Pods by selector, for adoption (#295): an orphan's change wakes the
    /// ReplicaSets that select it.
    fn dependencies(&self) -> Vec<crate::owned::Dependency> {
        vec![crate::owned::Dependency {
            path: "/api/v1/pods".into(),
            route: Arc::new(crate::owned::pod_membership),
        }]
    }
    async fn reconcile(
        &self,
        object: &Value,
        children: &[Value],
        deps: &crate::owned::Deps,
    ) -> anyhow::Result<()> {
        let namespace = object["metadata"]["namespace"]
            .as_str()
            .unwrap_or("default");
        let candidates = match workload_selector(&self.kind, object) {
            Some(selector) => crate::owned::pods_matching(&selector, namespace, deps.feed(0))?,
            None => Vec::new(),
        };
        self.reconcile_replicaset(namespace, object, children, &candidates).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Classify pods exactly as reconcile does, to lock in the #27 fix: a Failed
    // pod must NOT count as active (else the RS is "under" desired and storms),
    // and it must be visible as terminal (so it gets GC'd).
    fn classify(owned: &[Value]) -> (Vec<&Value>, Vec<&Value>) {
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

    fn pod(name: &str, labels: Value, controller: Option<&str>) -> Value {
        let refs: Vec<Value> = controller
            .map(|uid| json!({"kind": "ReplicaSet", "name": "x", "uid": uid, "controller": true}))
            .into_iter()
            .collect();
        json!({"metadata": {"name": name, "uid": name, "labels": labels, "ownerReferences": refs}})
    }
    fn names(v: &[&Value]) -> Vec<String> {
        v.iter().map(|p| p["metadata"]["name"].as_str().unwrap().to_string()).collect()
    }

    #[test]
    fn claim_adopts_orphans_releases_strays_and_leaves_others_alone() {
        // #295, upstream's ClaimPods.
        let sel = json!({"matchLabels": {"app": "a"}});
        let children = vec![
            pod("mine", json!({"app": "a"}), Some("rs")),
            pod("relabelled", json!({"app": "b"}), Some("rs")),
            // A non-controller reference to it: not its Pod.
            json!({"metadata": {"name": "referenced", "labels": {"app": "a"},
                "ownerReferences": [{"uid": "rs", "controller": false}]}}),
        ];
        let mut deleting = pod("deleting-orphan", json!({"app": "a"}), None);
        deleting["metadata"]["deletionTimestamp"] = json!("2026-11-20T00:00:00Z");
        let candidates = vec![
            pod("mine", json!({"app": "a"}), Some("rs")),
            pod("orphan", json!({"app": "a"}), None),
            pod("theirs", json!({"app": "a"}), Some("other")),
            deleting,
        ];
        let plan = claim("rs", Some(&sel), &children, &candidates);
        assert_eq!(names(&plan.keep), ["mine"]);
        assert_eq!(names(&plan.release), ["relabelled"]);
        assert_eq!(names(&plan.adopt), ["orphan"]);
        // No usable selector: keep what it controls, claim nothing.
        let plan = claim("rs", None, &children, &candidates);
        assert_eq!((names(&plan.keep), plan.release.len(), plan.adopt.len()), (vec!["mine".to_string(), "relabelled".to_string()], 0, 0));
    }

    #[test]
    fn a_refused_create_says_why() {
        // #340: the event and the ReplicaFailure condition carry the
        // apiserver's Status message, not "403 Forbidden for url".
        let msg = "pods \"web-1\" is forbidden: unable to validate against any security context constraint: [runAsUser: Invalid value: 0]";
        let body = json!({"kind": "Status", "reason": "Forbidden", "code": 403, "message": msg}).to_string();
        let e = anyhow::Error::from(apimachinery::api_status::ApiStatusError::from_body(
            reqwest::StatusCode::FORBIDDEN, "https://a:6443/api/v1/namespaces/n/pods", body.as_bytes()));
        assert_eq!(create_failure(&e), format!("Error creating: {msg}"));
        let conditions = failure_conditions(&json!([]), Some(&("FailedCreate", e.to_string())), true);
        assert_eq!(conditions[0]["message"], msg);
        assert_eq!(conditions[0]["reason"], "FailedCreate");
    }

    #[test]
    fn workload_selectors_by_kind() {
        let rs = json!({"spec": {"selector": {"matchLabels": {"app": "a"}}}});
        assert_eq!(workload_selector(&Kind::REPLICA_SET, &rs), Some(json!({"matchLabels": {"app": "a"}})));
        // An empty selector would claim the whole namespace.
        assert_eq!(workload_selector(&Kind::REPLICA_SET, &json!({"spec": {"selector": {}}})), None);
        assert_eq!(workload_selector(&Kind::REPLICA_SET, &json!({"spec": {"selector": {"matchLabels": {}}}})), None);
        let rc = json!({"spec": {"selector": {"app": "a"}}});
        assert_eq!(workload_selector(&Kind::REPLICATION_CONTROLLER, &rc), Some(json!({"matchLabels": {"app": "a"}})));
        let rc = json!({"spec": {"template": {"metadata": {"labels": {"app": "t"}}}}});
        assert_eq!(workload_selector(&Kind::REPLICATION_CONTROLLER, &rc), Some(json!({"matchLabels": {"app": "t"}})));
    }

    #[test]
    fn a_replication_controller_s_pods_name_it_as_owner() {
        let rc = json!({"spec": {"template": {"metadata": {"labels": {"app": "a"}},
                                               "spec": {"containers": [{"name": "c", "image": "i"}]}}}});
        let pod = build_pod_from_template("ns", "web", "u1", &rc, &Kind::REPLICATION_CONTROLLER).unwrap();
        let owner = &pod["metadata"]["ownerReferences"][0];
        assert_eq!((owner["apiVersion"].as_str(), owner["kind"].as_str()), (Some("v1"), Some("ReplicationController")));
        assert_eq!(owner["uid"], "u1");
        assert!(pod["metadata"]["name"].as_str().unwrap().starts_with("web-"));
        assert_eq!(pod["metadata"]["labels"]["app"], "a");
        let rs = build_pod_from_template("ns", "web", "u1", &rc, &Kind::REPLICA_SET).unwrap();
        assert_eq!(rs["metadata"]["ownerReferences"][0]["kind"], "ReplicaSet");
    }

    #[test]
    fn a_pod_carries_its_template_s_annotations_and_finalizers() {
        // #256: stormoptions marks its operator's template with
        // options.storm.io/golden-digest and waits for a pod that carries it.
        let rs = json!({"spec": {"template": {"metadata": {
            "labels": {"app": "a"},
            "annotations": {"options.storm.io/golden-digest": "17b6d634",
                            "kubectl.kubernetes.io/restartedAt": "2026-11-19T00:00:00Z"},
            "finalizers": ["example.com/keep"]},
            "spec": {"containers": [{"name": "c", "image": "i"}]}}}});
        let pod = build_pod_from_template("ns", "web", "u1", &rs, &Kind::REPLICA_SET).unwrap();
        assert_eq!(pod["metadata"]["annotations"]["options.storm.io/golden-digest"], "17b6d634");
        assert_eq!(pod["metadata"]["annotations"]["kubectl.kubernetes.io/restartedAt"], "2026-11-19T00:00:00Z");
        assert_eq!(pod["metadata"]["finalizers"], json!(["example.com/keep"]));
        assert_eq!(pod["metadata"]["labels"]["app"], "a");
        // A template with none adds neither field.
        let bare = json!({"spec": {"template": {"metadata": {"labels": {"app": "a"}}, "spec": {}}}});
        let pod = build_pod_from_template("ns", "web", "u1", &bare, &Kind::REPLICA_SET).unwrap();
        assert!(pod["metadata"].get("annotations").is_none());
        assert!(pod["metadata"].get("finalizers").is_none());
    }

    #[test]
    fn a_refused_create_is_a_replica_failure_until_a_clean_pass() {
        let refused = ("FailedCreate", "pods \"x\" is forbidden: exceeded quota: q".to_string());
        let set = failure_conditions(&Value::Null, Some(&refused), true);
        assert_eq!(set[0]["type"], "ReplicaFailure");
        assert_eq!(set[0]["status"], "True");
        assert_eq!(set[0]["reason"], "FailedCreate");
        assert!(set[0]["message"].as_str().unwrap().contains("exceeded quota"));
        // Refused again: the condition is kept, transition time and all.
        let mut aged = set.clone();
        aged[0]["lastTransitionTime"] = json!("2026-01-01T00:00:00Z");
        assert_eq!(failure_conditions(&aged, Some(&refused), true), aged);
        // Backing off, nothing tried: left as it is.
        assert_eq!(failure_conditions(&aged, None, false), aged);
        // A clean pass (or at the desired count): gone; other conditions stay.
        let mut mixed = aged.as_array().unwrap().clone();
        mixed.push(json!({"type": "Other", "status": "True"}));
        assert_eq!(failure_conditions(&json!(mixed), None, true), json!([{"type": "Other", "status": "True"}]));
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
