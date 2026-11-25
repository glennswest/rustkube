//! DaemonSet controller.
//!
//! Ensures one pod runs on every node the DaemonSet is eligible for (its
//! template's nodeSelector), Ready or not (#44). Pods get a random 5-character
//! suffix, generateName-style (#38), and are placed directly (bypasses the
//! scheduler by setting spec.nodeName).
//!
//! Each pod is labelled with the hash of the template it was made from
//! (`controller-revision-hash`) and the DaemonSet's generation
//! (`pod-template-generation`). Under `updateStrategy.type: RollingUpdate`
//! (the default) a pod whose hash is not the current template's is replaced,
//! as upstream's `rollingUpdate` (#369): without that, a DaemonSet the applier
//! updated kept its old pods until they happened to die (stormcos#811).

use crate::backoff::CreateBackoff;
use crate::runner::ApiClient;
use apimachinery::informer::{Delta, Key};
use apimachinery::informers::Feed;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use tracing::{debug, info, warn};

/// The label naming the template revision a pod was made from.
pub const REVISION_HASH: &str = "controller-revision-hash";
/// The label naming the DaemonSet generation a pod was made from.
pub const TEMPLATE_GENERATION: &str = "pod-template-generation";

pub struct DaemonSetController {
    api: Arc<ApiClient>,
    /// The events upstream's DaemonSet controller emits.
    ///
    /// **A controller that declines to act and records nothing leaves you
    /// inferring from status arithmetic.** `oc describe ds` showed
    /// `Desired 1 / Current 0` and an empty Events section, which is
    /// indistinguishable between "backing off after failures", "the create was
    /// rejected" and "the controller is not running" — three very different
    /// problems that took two boots to tell apart.
    recorder: crate::events::EventRecorder,
    /// Per-(DaemonSet uid + node) recreation backoff after failed pods, so a
    /// node whose pod keeps failing is retried with widening spacing instead of
    /// a per-reconcile pod storm on that node.
    backoff: CreateBackoff,
}

impl DaemonSetController {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self {
            recorder: crate::events::EventRecorder::new(api.clone(), "daemonset-controller"),
            api,
            // Match the k8s DaemonSetController's failedPodsBackoff window: 1s
            // doubling to a 15min cap, keyed per (DaemonSet uid, node).
            backoff: CreateBackoff::with_params(Duration::from_secs(1), Duration::from_secs(900)),
        }
    }

    pub async fn run(&self) {
        info!("DaemonSet indexed object workers started");
        crate::owned::run(&self.api, self).await;
    }

    async fn reconcile_daemonset(
        &self,
        namespace: &str,
        ds: &Value,
        all_pods: &[Value],
        nodes: &[Value],
    ) -> anyhow::Result<()> {
        let ds_name = ds["metadata"]["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("daemonset missing name"))?;
        let ds_uid = ds["metadata"]["uid"].as_str().unwrap_or("");

        // Being deleted: what it owns is the garbage collector's now. Making
        // replacements for the children a foreground delete removes races
        // that delete (upstream skips a deleting owner the same way; found by
        // the test container's gc-foreground check, #96).
        if !ds["metadata"]["deletionTimestamp"].is_null() {
            return Ok(());
        }

        // Nodes this DaemonSet is eligible for (matches nodeSelector) — regardless
        // of readiness; its pods tolerate NotReady. Scheduling and GC key off this
        // set, so a node briefly going NotReady no longer churns its pod (#44).
        let eligible: Vec<String> = nodes
            .iter()
            .filter(|n| node_matches_ds(n, ds))
            .filter_map(|n| n["metadata"]["name"].as_str().map(|s| s.to_string()))
            .collect();
        let eligible_set: HashSet<&str> = eligible.iter().map(|s| s.as_str()).collect();

        // All pods owned by this DaemonSet.
        let owned: Vec<&Value> = all_pods
            .iter()
            .filter(|pod| {
                pod["metadata"]["ownerReferences"]
                    .as_array()
                    .map(|refs| refs.iter().any(|r| r["uid"].as_str() == Some(ds_uid)))
                    .unwrap_or(false)
            })
            .collect();

        // Partition into active (counts as "node has a pod") and terminal. A pod
        // already being deleted is going away — neither active nor a GC target.
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

        // Delete + recreate, matching the k8s DaemonSetController: a Failed pod
        // is DELETED (not retained) so the node stops looking occupied, then the
        // create pass below mints a fresh replacement — gated by failedPodsBackoff
        // so a crash-looping pod isn't recreated in a tight loop (the #27 storm
        // class). A Succeeded pod (unusual for a DaemonSet) is just cleared out.
        // Deleting pods are already excluded from `terminal`, so each Failed pod
        // arms this node's backoff exactly once.
        let now = Instant::now();
        for pod in &terminal {
            let node = pod["spec"]["nodeName"].as_str().unwrap_or("").to_string();
            if pod["status"]["phase"].as_str() == Some("Failed") {
                if let Some(uid) = pod["metadata"]["uid"].as_str() {
                    self.backoff
                        .observe_failures(&format!("{ds_uid}/{node}"), &[uid], now);
                }
            }
            let pod_name = pod["metadata"]["name"].as_str().unwrap_or("");
            if !pod_name.is_empty() {
                if pod["status"]["phase"].as_str() == Some("Failed") {
                    self.recorder
                        .event(
                            ds,
                            "Warning",
                            "FailedDaemonPod",
                            &format!(
                                "Found failed daemon pod {namespace}/{pod_name} on node \
                                 {node}, will try to kill it"
                            ),
                        )
                        .await;
                }
                match self
                    .api
                    .delete_observed(
                        &format!("/api/v1/namespaces/{namespace}/pods/{pod_name}"),
                        pod,
                    )
                    .await
                {
                    Ok(_) => {
                        info!(
                            "Deleted terminal DaemonSet pod {namespace}/{pod_name} \
                             (node {node}); will recreate"
                        );
                        self.recorder
                            .event(
                                ds,
                                "Normal",
                                "SuccessfulDelete",
                                &format!("Deleted pod: {pod_name}"),
                            )
                            .await;
                    }
                    Err(e) => {
                        debug!("Failed to delete terminal DaemonSet pod {pod_name}: {e}");
                        self.recorder
                            .event(
                                ds,
                                "Warning",
                                "FailedDelete",
                                &format!("Error deleting: {e}"),
                            )
                            .await;
                    }
                }
            }
        }

        // Nodes that already have an ACTIVE pod.
        let nodes_with_pods: HashSet<String> = active
            .iter()
            .filter_map(|pod| pod["spec"]["nodeName"].as_str().map(|s| s.to_string()))
            .collect();

        // Create pods on eligible nodes that need one (subject to per-node
        // backoff). Eligible includes NotReady nodes — DS pods run there too.
        for node_name in &eligible {
            if nodes_with_pods.contains(node_name) {
                continue;
            }
            if !self.backoff.allowed(&format!("{ds_uid}/{node_name}"), now) {
                debug!(
                    "DaemonSet {namespace}/{ds_name}: backing off pod creation on \
                     node {node_name} after repeated failures"
                );
                // Not an upstream event, and it earns its place: without it,
                // "Desired 1 / Current 0" with an empty Events section is
                // indistinguishable from a controller that is not running.
                // Deduplicated by the recorder, so a long backoff does not
                // become a log of its own.
                self.recorder
                    .event(
                        ds,
                        "Normal",
                        "BackingOff",
                        &format!(
                            "Waiting before recreating a pod on node {node_name} after \
                             repeated failures"
                        ),
                    )
                    .await;
                continue;
            }
            self.create_pod(namespace, ds_name, ds_uid, node_name, ds)
                .await?;
        }

        // Delete active pods only on nodes the DS is no longer eligible for — a
        // deleted node, or one that no longer matches the nodeSelector. A merely
        // NotReady node still keeps its pod (that's the whole point of a
        // DaemonSet), so it is NOT collected here.
        for pod in &active {
            let node_name = pod["spec"]["nodeName"].as_str().unwrap_or("");
            if !node_name.is_empty() && !eligible_set.contains(node_name) {
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
                            info!("Deleted DaemonSet pod {pod_name} (node {node_name} gone)");
                        }
                        Err(e) => {
                            warn!("Failed to delete DaemonSet pod {pod_name}: {e}");
                        }
                    }
                }
            }
        }

        // The active pods on each eligible node, an empty list for a node
        // that has none yet.
        let hash = template_hash(ds);
        let mut by_node: BTreeMap<&str, Vec<&Value>> =
            eligible.iter().map(|n| (n.as_str(), Vec::new())).collect();
        for pod in &active {
            if let Some(pods) = pod["spec"]["nodeName"]
                .as_str()
                .and_then(|n| by_node.get_mut(n))
            {
                pods.push(*pod);
            }
        }

        // Available: Ready for `minReadySeconds` (#379), which is what the
        // rollout waits on and `numberAvailable` counts. A Ready pod not yet
        // there wakes this reconcile when it will be.
        let min_ready = crate::availability::min_ready_seconds(ds);
        let now = chrono::Utc::now();
        crate::availability::requeue_until_available(active.iter().copied(), min_ready);

        // Rolling update (#369): replace the pods made from an older template.
        // `OnDelete` leaves them until someone deletes them; the create pass
        // above then makes the replacement from the current template.
        if update_strategy(ds) == "RollingUpdate" {
            let (max_surge, max_unavailable) = rolling_limits(ds, eligible.len());
            let step = plan_rollout(&by_node, &hash, max_surge, max_unavailable, |p| {
                crate::availability::is_pod_available(p, min_ready, now)
            });
            for pod in &step.delete {
                let pod_name = pod["metadata"]["name"].as_str().unwrap_or("");
                let node = pod["spec"]["nodeName"].as_str().unwrap_or("");
                if pod_name.is_empty() {
                    continue;
                }
                match self
                    .api
                    .delete_observed(
                        &format!("/api/v1/namespaces/{namespace}/pods/{pod_name}"),
                        pod,
                    )
                    .await
                {
                    Ok(_) => {
                        info!(
                            "Rolling DaemonSet {namespace}/{ds_name}: deleted old pod \
                             {pod_name} on node {node} (template {hash})"
                        );
                        self.recorder
                            .event(
                                ds,
                                "Normal",
                                "SuccessfulDelete",
                                &format!("Deleted pod: {pod_name}"),
                            )
                            .await;
                    }
                    Err(e) => {
                        warn!("Failed to delete old DaemonSet pod {pod_name}: {e}");
                        self.recorder
                            .event(
                                ds,
                                "Warning",
                                "FailedDelete",
                                &format!("Error deleting: {e}"),
                            )
                            .await;
                    }
                }
            }
            for node in &step.surge {
                self.create_pod(namespace, ds_name, ds_uid, node, ds).await?;
            }
        }

        // DaemonSet status, counted per node from the pods the DS owns (#44),
        // as upstream:
        //   desired          = eligible nodes
        //   currentScheduled = eligible nodes running an active pod
        //   updatedScheduled = those running one of the current template (#369)
        //   ready/available  = those running a Ready pod
        //   misscheduled     = nodes running an active pod the DS isn't eligible for
        let desired = eligible.len();
        let current_scheduled = by_node.values().filter(|p| !p.is_empty()).count();
        let updated_scheduled = by_node
            .values()
            .filter(|p| p.iter().any(|pod| is_current(pod, &hash)))
            .count();
        let misscheduled = active
            .iter()
            .filter(|p| !pod_on_eligible(p, &eligible_set))
            .filter_map(|p| p["spec"]["nodeName"].as_str().map(str::to_string))
            .collect::<HashSet<_>>()
            .len();
        let ready_count = by_node
            .values()
            .filter(|p| p.iter().any(|pod| is_pod_ready(pod)))
            .count();
        let available_count = by_node
            .values()
            .filter(|p| {
                p.iter()
                    .any(|pod| crate::availability::is_pod_available(pod, min_ready, now))
            })
            .count();

        // A node running a Ready pod has recovered — clear its backoff so future
        // failures start from the base delay again.
        for pod in active.iter().filter(|p| is_pod_ready(p)) {
            if let Some(node) = pod["spec"]["nodeName"].as_str() {
                self.backoff.clear(&format!("{ds_uid}/{node}"));
            }
        }

        let status = json!({
            "desiredNumberScheduled": desired,
            "currentNumberScheduled": current_scheduled,
            "updatedNumberScheduled": updated_scheduled,
            "numberReady": ready_count,
            "numberAvailable": available_count,
            "numberUnavailable": desired.saturating_sub(available_count),
            "numberMisscheduled": misscheduled,
            "observedGeneration": ds["metadata"]["generation"].as_u64().unwrap_or(1)
        });
        // An unchanged status is not written: the write would only echo back
        // as a watch event and cost a request per reconcile.
        if ds["status"] != status {
            let mut updated = ds.clone();
            updated["status"] = status;
            // status only: a whole-object PUT would carry a spec read
            // before the last reconcile and revert it.
            self.api
                .update_status(
                    &format!("/apis/apps/v1/namespaces/{namespace}/daemonsets/{ds_name}"),
                    &updated,
                )
                .await?;
        }

        Ok(())
    }

    /// Creates a pod of the current template on `node_name`, recording the
    /// outcome as upstream's events. A refused create is an Event, not an
    /// error: the other nodes still get theirs.
    async fn create_pod(
        &self,
        namespace: &str,
        ds_name: &str,
        ds_uid: &str,
        node_name: &str,
        ds: &Value,
    ) -> anyhow::Result<()> {
        let pod = build_daemonset_pod(namespace, ds_name, ds_uid, node_name, ds)?;
        match self
            .api
            .create(&format!("/api/v1/namespaces/{namespace}/pods"), &pod)
            .await
        {
            Ok(_) => {
                let pod_name = pod["metadata"]["name"].as_str().unwrap_or("?");
                info!("Created DaemonSet pod {namespace}/{pod_name} on node {node_name}");
                self.recorder
                    .event(
                        ds,
                        "Normal",
                        "SuccessfulCreate",
                        &format!("Created pod: {pod_name}"),
                    )
                    .await;
            }
            Err(e) => {
                warn!("Failed to create DaemonSet pod on {node_name}: {e}");
                // The one that was invisible. A create the apiserver
                // rejects was a `warn!` on a node with no reachable log.
                self.recorder
                    .event(
                        ds,
                        "Warning",
                        "FailedCreate",
                        &format!("Error creating: {e}"),
                    )
                    .await;
            }
        }
        Ok(())
    }
}

/// The hash of the DaemonSet's current pod template: the value its pods'
/// `controller-revision-hash` label must carry to count as updated. The
/// Deployment's FNV-1a hash, stable across builds, so an upgrade of rustkube
/// does not roll every DaemonSet by itself.
fn template_hash(ds: &Value) -> String {
    crate::deployment::compute_template_hash(&ds["spec"]["template"])
}

/// Whether a pod was made from the template whose hash is `hash`.
fn is_current(pod: &Value, hash: &str) -> bool {
    pod["metadata"]["labels"][REVISION_HASH].as_str() == Some(hash)
}

/// `spec.updateStrategy.type`, `RollingUpdate` when unset (upstream's default).
fn update_strategy(ds: &Value) -> &str {
    ds["spec"]["updateStrategy"]["type"]
        .as_str()
        .unwrap_or("RollingUpdate")
}

/// `(maxSurge, maxUnavailable)` in nodes, as upstream's
/// `updatedDesiredNodeCounts`: integers or percentages of the eligible nodes,
/// both rounded up; `maxUnavailable` defaults to 1 and `maxSurge` to 0, and
/// both zero means `maxUnavailable` 1, so a rollout always progresses.
fn rolling_limits(ds: &Value, desired: usize) -> (usize, usize) {
    let ru = &ds["spec"]["updateStrategy"]["rollingUpdate"];
    let desired = desired as u64;
    let surge = if ru["maxSurge"].is_null() {
        0
    } else {
        crate::rollout::int_or_percent(&ru["maxSurge"], desired, true)
    };
    let unavailable = if ru["maxUnavailable"].is_null() {
        1
    } else {
        crate::rollout::int_or_percent(&ru["maxUnavailable"], desired, true)
    };
    if surge == 0 && unavailable == 0 {
        return (0, 1);
    }
    (surge as usize, unavailable as usize)
}

/// One step of a rolling update: the old pods to delete now and the nodes to
/// give a new pod beside their old one (surge).
#[derive(Debug, Default)]
struct RolloutStep<'a> {
    delete: Vec<&'a Value>,
    surge: Vec<String>,
}

/// The next step of a DaemonSet rolling update, upstream's `rollingUpdate`.
///
/// `by_node` holds each eligible node's active (not deleting, not finished)
/// pods. A pod is *old* when its `controller-revision-hash` is not `hash` —
/// a pod made before pods carried the label is old too, which is how a
/// keep-data install's pods reach the new template (stormcos#811).
///
/// `available` says whether a pod serves: Ready for the DaemonSet's
/// `minReadySeconds` (#379), as upstream's `IsPodAvailable`; "Ready" below
/// means that.
///
/// Without surge, at most `max_unavailable` nodes are without an available
/// (Ready) pod at once: a node with no pod, or with a new pod not yet Ready,
/// spends the budget, and an old pod that is Ready is deleted only while
/// budget is left. An old pod that is not Ready is deleted at once — it serves
/// nothing, so replacing it costs no availability. The replacement is made by
/// the create pass of the next reconcile, once the deleted pod is going away.
///
/// With surge, a node keeps its old pod while a new one starts beside it, at
/// most `max_surge` nodes at a time (a node whose old pod is not Ready gets
/// its new pod regardless), and the old pod is deleted once the new is Ready.
fn plan_rollout<'a>(
    by_node: &BTreeMap<&str, Vec<&'a Value>>,
    hash: &str,
    max_surge: usize,
    max_unavailable: usize,
    available: impl Fn(&Value) -> bool,
) -> RolloutStep<'a> {
    let mut step = RolloutStep::default();
    if max_surge == 0 {
        let mut unavailable = 0;
        let mut candidates: Vec<Vec<&'a Value>> = Vec::new();
        for pods in by_node.values() {
            let (new, old): (Vec<&Value>, Vec<&Value>) =
                pods.iter().copied().partition(|p| is_current(p, hash));
            let new_ready = new.iter().any(|p| available(p));
            if old.is_empty() {
                if !new_ready {
                    unavailable += 1;
                }
            } else if !new.is_empty() {
                // Both on one node (a surge left over, or a strategy change
                // midway): the old goes once the new serves.
                if new_ready {
                    step.delete.extend(old);
                } else {
                    unavailable += 1;
                }
            } else if !old.iter().any(|p| available(p)) {
                step.delete.extend(old);
            } else {
                candidates.push(old);
            }
        }
        let budget = max_unavailable.saturating_sub(unavailable);
        for old in candidates.into_iter().take(budget) {
            step.delete.extend(old);
        }
    } else {
        let mut surging = 0;
        let mut candidates: Vec<String> = Vec::new();
        for (node, pods) in by_node {
            let (new, old): (Vec<&Value>, Vec<&Value>) =
                pods.iter().copied().partition(|p| is_current(p, hash));
            if old.is_empty() {
                continue;
            }
            if !new.is_empty() {
                if new.iter().any(|p| available(p)) {
                    step.delete.extend(old);
                } else {
                    surging += 1;
                }
            } else if !old.iter().any(|p| available(p)) {
                step.surge.push(node.to_string());
            } else {
                candidates.push(node.to_string());
            }
        }
        let budget = max_surge.saturating_sub(surging);
        step.surge.extend(candidates.into_iter().take(budget));
    }
    step
}

/// Node changes that can alter eligibility: a node appearing or going away,
/// or its labels changing. Heartbeats and conditions wake nothing.
fn route_node(delta: &Delta, primary: &Feed) -> Vec<Key> {
    affected_daemonsets(delta, &primary.list().unwrap_or_default())
}

fn affected_daemonsets(delta: &Delta, daemonsets: &[Value]) -> Vec<Key> {
    if let (Some(old), Some(new)) = (&delta.old, &delta.new) {
        if old["metadata"]["labels"] == new["metadata"]["labels"] {
            return Vec::new();
        }
    }
    daemonsets
        .iter()
        .filter(|ds| {
            delta
                .old
                .iter()
                .chain(delta.new.iter())
                .any(|node| node_matches_ds(node, ds))
        })
        .filter_map(|ds| Key::of(ds).ok())
        .collect()
}

#[async_trait::async_trait]
impl crate::owned::Controller for DaemonSetController {
    fn name(&self) -> &'static str {
        "daemonset"
    }
    fn primary(&self) -> &'static str {
        "/apis/apps/v1/daemonsets"
    }
    fn children(&self) -> Option<&'static str> {
        Some("/api/v1/pods")
    }
    fn dependencies(&self) -> Vec<crate::owned::Dependency> {
        vec![crate::owned::Dependency {
            path: "/api/v1/nodes".into(),
            route: Arc::new(route_node),
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
        // A node list that is not synchronized fails the reconcile: read as
        // empty it would make every pod "misscheduled" and delete it.
        let nodes = deps.feed(0).list()?;
        self.reconcile_daemonset(namespace, object, children, &nodes)
            .await
    }
}

fn build_daemonset_pod(
    namespace: &str,
    ds_name: &str,
    ds_uid: &str,
    node_name: &str,
    ds: &Value,
) -> anyhow::Result<Value> {
    let template = &ds["spec"]["template"];
    // Random suffix (k8s generateName style, e.g. `cilium-fd98f`) rather than a
    // deterministic per-node name: a lingering/terminating pod must never
    // collide (409) with its replacement — that collision was the self-healing
    // deadlock in #38. Node placement is tracked by spec.nodeName, not the name.
    let suffix = &uuid::Uuid::new_v4().to_string()[..5];
    let pod_name = format!("{ds_name}-{suffix}");

    let mut labels = template["metadata"]["labels"].clone();
    if labels.is_null() {
        labels = json!({});
    }
    // What the rolling update compares (#369): the template this pod was
    // made from, and the DaemonSet generation, as upstream labels them.
    if let Some(l) = labels.as_object_mut() {
        l.insert(REVISION_HASH.into(), json!(template_hash(ds)));
        let generation = ds["metadata"]["generation"].as_u64().unwrap_or(1);
        l.insert(TEMPLATE_GENERATION.into(), json!(generation.to_string()));
    }

    let mut spec = template["spec"].clone();
    if let Some(s) = spec.as_object_mut() {
        // Bypass scheduler — place directly on node
        s.insert("nodeName".into(), Value::String(node_name.to_string()));
        // The tolerations upstream's DaemonSet controller adds to every pod
        // it creates.
        //
        // **A DaemonSet has to run on a node that is not working.** That is
        // the point of one: the pod network, the CSI driver and the log
        // shipper are what *make* a node usable, so a scheduling rule that
        // keeps pods off a broken node keeps off exactly the pods that would
        // fix it. Only `not-ready` was here before, so a node under memory or
        // disk pressure, or one cordoned by an operator, would have its
        // DaemonSet pods evicted and never replaced.
        //
        // `network-unavailable` matters most immediately: it is the taint a
        // node carries when it has no pod network, and Cilium is the thing
        // that removes it. Without the toleration the pod network cannot be
        // deployed to a node that has no pod network.
        let defaults = [
            ("node.kubernetes.io/not-ready", "NoExecute"),
            ("node.kubernetes.io/unreachable", "NoExecute"),
            ("node.kubernetes.io/disk-pressure", "NoSchedule"),
            ("node.kubernetes.io/memory-pressure", "NoSchedule"),
            ("node.kubernetes.io/pid-pressure", "NoSchedule"),
            ("node.kubernetes.io/unschedulable", "NoSchedule"),
            ("node.kubernetes.io/network-unavailable", "NoSchedule"),
        ];
        let tolerations = s.entry("tolerations").or_insert_with(|| json!([]));
        if let Some(arr) = tolerations.as_array_mut() {
            for (key, effect) in defaults {
                // Never duplicate one the author already wrote: a DaemonSet
                // that tolerates everything (`operator: Exists` with no key,
                // which is what Cilium ships) already covers these, and
                // appending seven more would change the pod spec on every
                // reconcile.
                let covered = arr.iter().any(|t| {
                    let tk = t["key"].as_str().unwrap_or("");
                    let te = t["effect"].as_str().unwrap_or("");
                    let wildcard_key = tk.is_empty() && t["operator"].as_str() == Some("Exists");
                    (wildcard_key || tk == key) && (te.is_empty() || te == effect)
                });
                if !covered {
                    arr.push(json!({ "key": key, "operator": "Exists", "effect": effect }));
                }
            }
        }
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
                "kind": "DaemonSet",
                "name": ds_name,
                "uid": ds_uid,
                "controller": true,
                "blockOwnerDeletion": true
            }]
        },
        "spec": spec,
        "status": {
            "phase": "Pending"
        }
    });
    crate::owned::copy_template_metadata(template, &mut pod);

    Ok(pod)
}

/// Whether an owned pod sits on a node the DaemonSet is still eligible for.
fn pod_on_eligible(pod: &Value, eligible: &HashSet<&str>) -> bool {
    pod["spec"]["nodeName"]
        .as_str()
        .map(|n| eligible.contains(n))
        .unwrap_or(false)
}

/// Whether `node` matches the DaemonSet's pod-template `nodeSelector` (every
/// key=value present in the node's labels). No selector matches every node.
fn node_matches_ds(node: &Value, ds: &Value) -> bool {
    let Some(sel) = ds["spec"]["template"]["spec"]["nodeSelector"].as_object() else {
        return true;
    };
    if sel.is_empty() {
        return true;
    }
    let labels = &node["metadata"]["labels"];
    sel.iter().all(|(k, v)| labels.get(k) == Some(v))
}

use crate::availability::is_pod_ready;

#[cfg(test)]
mod tests {
    use super::*;

    fn ds(tolerations: Value) -> Value {
        let mut spec = json!({"containers": [{"name": "c", "image": "x"}]});
        if !tolerations.is_null() {
            spec["tolerations"] = tolerations;
        }
        json!({
            "metadata": {"name": "d", "namespace": "kube-system", "uid": "u"},
            "spec": {"template": {"metadata": {"labels": {"a": "b"}}, "spec": spec}}
        })
    }

    /// A DaemonSet must run on a node that is not working — that is the point
    /// of one. The pod network, the CSI driver and the log shipper are what
    /// *make* a node usable, so keeping their pods off a broken node keeps off
    /// exactly the pods that would fix it. Only `not-ready` was tolerated
    /// before.
    #[test]
    fn a_daemonset_pod_carries_its_template_s_annotations() {
        let ds = json!({"metadata": {"name": "d", "namespace": "kube-system", "uid": "u"},
            "spec": {"template": {"metadata": {"labels": {"a": "b"},
                "annotations": {"prometheus.io/scrape": "true"}},
                "spec": {"containers": [{"name": "c", "image": "i"}]}}}});
        let pod = build_daemonset_pod("kube-system", "d", "u", "node1", &ds).unwrap();
        assert_eq!(pod["metadata"]["annotations"]["prometheus.io/scrape"], "true");
    }

    #[test]
    fn daemonset_pods_tolerate_every_node_condition() {
        let pod = build_daemonset_pod("kube-system", "d", "u", "node1", &ds(Value::Null)).unwrap();
        let tol = pod["spec"]["tolerations"].as_array().expect("tolerations");
        let keys: Vec<&str> = tol.iter().filter_map(|t| t["key"].as_str()).collect();
        for want in [
            "node.kubernetes.io/not-ready",
            "node.kubernetes.io/unreachable",
            "node.kubernetes.io/disk-pressure",
            "node.kubernetes.io/memory-pressure",
            "node.kubernetes.io/pid-pressure",
            "node.kubernetes.io/unschedulable",
            // The one Cilium needs: it is the taint a node carries when it has
            // no pod network, and Cilium is what removes it.
            "node.kubernetes.io/network-unavailable",
        ] {
            assert!(keys.contains(&want), "{want} not tolerated: {keys:?}");
        }
    }

    /// A DaemonSet that already tolerates everything — which is what Cilium
    /// ships — gets nothing appended, or the pod spec would differ on every
    /// reconcile.
    #[test]
    fn a_tolerate_everything_daemonset_gets_nothing_appended() {
        let pod = build_daemonset_pod(
            "kube-system",
            "d",
            "u",
            "node1",
            &ds(json!([{"operator": "Exists"}])),
        )
        .unwrap();
        let tol = pod["spec"]["tolerations"].as_array().expect("tolerations");
        assert_eq!(
            tol.len(),
            1,
            "appended to a tolerate-everything spec: {tol:?}"
        );
    }

    fn node(labels: Value, heartbeat: &str) -> Value {
        json!({"metadata": {"name": "n1", "uid": "n1", "labels": labels},
               "status": {"conditions": [{"type": "Ready", "status": "True",
                                          "lastHeartbeatTime": heartbeat}]}})
    }
    fn delta(old: Option<Value>, new: Option<Value>) -> Delta {
        Delta {
            old,
            new,
            affected: Default::default(),
        }
    }

    /// A heartbeat changes the node every few seconds and changes nothing a
    /// DaemonSet decides on; waking every DaemonSet for it would be a poll
    /// loop by another name.
    #[test]
    fn a_node_heartbeat_wakes_no_daemonset() {
        let dss = [ds(Value::Null)];
        let d = delta(Some(node(json!({}), "t1")), Some(node(json!({}), "t2")));
        assert!(affected_daemonsets(&d, &dss).is_empty());
    }

    /// A node appearing, going away, or relabelled wakes exactly the
    /// DaemonSets whose nodeSelector matches it before or after.
    #[test]
    fn node_membership_changes_wake_matching_daemonsets() {
        let mut gpu = ds(Value::Null);
        gpu["metadata"]["name"] = json!("gpu");
        gpu["metadata"]["uid"] = json!("gpu");
        gpu["spec"]["template"]["spec"]["nodeSelector"] = json!({"gpu": "yes"});
        let dss = [ds(Value::Null), gpu];
        let names = |keys: Vec<Key>| {
            let mut n: Vec<_> = keys.into_iter().map(|k| k.name).collect();
            n.sort();
            n
        };
        let plain = node(json!({}), "t");
        let labelled = node(json!({"gpu": "yes"}), "t");
        assert_eq!(
            names(affected_daemonsets(&delta(None, Some(plain.clone())), &dss)),
            ["d"]
        );
        assert_eq!(
            names(affected_daemonsets(
                &delta(Some(labelled.clone()), None),
                &dss
            )),
            ["d", "gpu"]
        );
        // Losing the label must wake the DaemonSet that matched the old side.
        assert_eq!(
            names(affected_daemonsets(
                &delta(Some(labelled), Some(plain)),
                &dss
            )),
            ["d", "gpu"]
        );
    }

    fn pod(name: &str, node: &str, hash: Option<&str>, ready: bool) -> Value {
        let mut labels = json!({"a": "b"});
        if let Some(h) = hash {
            labels[REVISION_HASH] = json!(h);
        }
        let status = if ready { "True" } else { "False" };
        json!({"metadata": {"name": name, "labels": labels},
               "spec": {"nodeName": node},
               "status": {"phase": "Running", "conditions": [
                   {"type": "Ready", "status": status}]}})
    }
    fn nodes<'a>(pods: &'a [Value], names: &[&'a str]) -> BTreeMap<&'a str, Vec<&'a Value>> {
        let mut m: BTreeMap<&str, Vec<&Value>> = names.iter().map(|n| (*n, Vec::new())).collect();
        for p in pods {
            m.get_mut(p["spec"]["nodeName"].as_str().unwrap()).unwrap().push(p);
        }
        m
    }
    fn names(pods: &[&Value]) -> Vec<String> {
        pods.iter()
            .map(|p| p["metadata"]["name"].as_str().unwrap().to_string())
            .collect()
    }

    /// A pod carries the hash of the template it was made from and the
    /// generation, and a template change changes the hash (#369).
    #[test]
    fn a_pod_carries_its_template_hash_and_generation() {
        let mut d = ds(Value::Null);
        d["metadata"]["generation"] = json!(3);
        let pod = build_daemonset_pod("kube-system", "d", "u", "n1", &d).unwrap();
        let hash = template_hash(&d);
        assert_eq!(pod["metadata"]["labels"][REVISION_HASH], json!(hash));
        assert_eq!(pod["metadata"]["labels"][TEMPLATE_GENERATION], "3");
        assert_eq!(pod["metadata"]["labels"]["a"], "b");
        assert!(is_current(&pod, &hash));
        d["spec"]["template"]["spec"]["containers"][0]["image"] = json!("y");
        assert!(!is_current(&pod, &template_hash(&d)));
    }

    /// Ready old pods are replaced one node at a time (the default
    /// maxUnavailable 1); a pod with no hash label, made before pods carried
    /// one, is old — the keep-data install's case (stormcos#811).
    #[test]
    fn ready_old_pods_roll_one_node_at_a_time() {
        let pods = [
            pod("p1", "n1", Some("old"), true),
            pod("p2", "n2", None, true),
            pod("p3", "n3", Some("old"), true),
        ];
        let by = nodes(&pods, &["n1", "n2", "n3"]);
        let step = plan_rollout(&by, "new", 0, 1, is_pod_ready);
        assert_eq!(names(&step.delete), ["p1"]);
        assert!(step.surge.is_empty());
        // Two at a time with maxUnavailable 2.
        assert_eq!(names(&plan_rollout(&by, "new", 0, 2, is_pod_ready).delete), ["p1", "p2"]);
    }

    /// A node whose replacement is missing or not yet Ready spends the
    /// budget: nothing more is deleted until it serves.
    #[test]
    fn an_unready_replacement_holds_the_rollout() {
        let pods = [
            pod("p1", "n1", Some("new"), false),
            pod("p2", "n2", Some("old"), true),
        ];
        let step = plan_rollout(&nodes(&pods, &["n1", "n2", "n3"]), "new", 0, 2, is_pod_ready);
        // n1 (new, not Ready) and n3 (no pod) use both slots.
        assert!(step.delete.is_empty(), "{:?}", names(&step.delete));
        // Once n1's pod is Ready and n3 has a Ready new pod, n2 rolls.
        let pods = [
            pod("p1", "n1", Some("new"), true),
            pod("p2", "n2", Some("old"), true),
            pod("p3", "n3", Some("new"), true),
        ];
        let step = plan_rollout(&nodes(&pods, &["n1", "n2", "n3"]), "new", 0, 1, is_pod_ready);
        assert_eq!(names(&step.delete), ["p2"]);
    }

    /// An old pod that is not Ready serves nothing: it is replaced without
    /// waiting for budget.
    #[test]
    fn an_unready_old_pod_is_replaced_at_once() {
        let pods = [
            pod("p1", "n1", Some("old"), false),
            pod("p2", "n2", Some("old"), false),
            pod("p3", "n3", Some("new"), false),
        ];
        let step = plan_rollout(&nodes(&pods, &["n1", "n2", "n3"]), "new", 0, 1, is_pod_ready);
        assert_eq!(names(&step.delete), ["p1", "p2"]);
    }

    /// With `minReadySeconds`, a Ready replacement that has not been Ready
    /// that long still holds the rollout, and an old pod in the same state
    /// counts as not serving (#379).
    #[test]
    fn min_ready_seconds_holds_the_rollout_until_the_new_pod_is_available() {
        let since = |p: &mut Value, t: &str| {
            p["status"]["conditions"][0]["lastTransitionTime"] = json!(t);
        };
        let mut p1 = pod("p1", "n1", Some("new"), true);
        since(&mut p1, "2026-11-25T10:00:00Z");
        let mut p2 = pod("p2", "n2", Some("old"), true);
        since(&mut p2, "2026-11-25T09:00:00Z");
        let pods = [p1, p2];
        let by = nodes(&pods, &["n1", "n2"]);
        let at = |t: &str| chrono::DateTime::parse_from_rfc3339(t).unwrap().with_timezone(&chrono::Utc);
        let early = at("2026-11-25T10:00:05Z");
        let step = plan_rollout(&by, "new", 0, 1, |p| crate::availability::is_pod_available(p, 10, early));
        assert!(step.delete.is_empty(), "n1's pod is Ready 5s of 10: {:?}", names(&step.delete));
        let late = at("2026-11-25T10:00:11Z");
        let step = plan_rollout(&by, "new", 0, 1, |p| crate::availability::is_pod_available(p, 10, late));
        assert_eq!(names(&step.delete), ["p2"]);
    }

    /// Pods already of the current template are left alone.
    #[test]
    fn current_pods_are_not_touched() {
        let pods = [pod("p1", "n1", Some("new"), true)];
        let step = plan_rollout(&nodes(&pods, &["n1"]), "new", 0, 1, is_pod_ready);
        assert!(step.delete.is_empty() && step.surge.is_empty());
    }

    /// With surge the new pod starts beside the old, `maxSurge` nodes at a
    /// time, and the old goes once the new is Ready.
    #[test]
    fn surge_starts_the_new_pod_before_deleting_the_old() {
        let pods = [
            pod("p1", "n1", Some("old"), true),
            pod("p2", "n2", Some("old"), true),
        ];
        let step = plan_rollout(&nodes(&pods, &["n1", "n2"]), "new", 1, 0, is_pod_ready);
        assert!(step.delete.is_empty());
        assert_eq!(step.surge, ["n1"]);
        // n1's new pod not Ready yet: it is the one surging, n2 waits.
        let pods = [
            pod("p1", "n1", Some("old"), true),
            pod("q1", "n1", Some("new"), false),
            pod("p2", "n2", Some("old"), true),
        ];
        let step = plan_rollout(&nodes(&pods, &["n1", "n2"]), "new", 1, 0, is_pod_ready);
        assert!(step.delete.is_empty() && step.surge.is_empty());
        // Ready: the old on n1 goes, and n2 may surge.
        let pods = [
            pod("p1", "n1", Some("old"), true),
            pod("q1", "n1", Some("new"), true),
            pod("p2", "n2", Some("old"), true),
        ];
        let step = plan_rollout(&nodes(&pods, &["n1", "n2"]), "new", 1, 0, is_pod_ready);
        assert_eq!(names(&step.delete), ["p1"]);
        assert_eq!(step.surge, ["n2"]);
    }

    /// Upstream's defaults and rounding: maxUnavailable 1, maxSurge 0,
    /// percentages of the eligible nodes rounded up, both zero meaning one
    /// unavailable.
    #[test]
    fn rolling_limits_follow_upstream() {
        let mut d = ds(Value::Null);
        assert_eq!(update_strategy(&d), "RollingUpdate");
        assert_eq!(rolling_limits(&d, 10), (0, 1));
        d["spec"]["updateStrategy"] = json!({"type": "RollingUpdate",
            "rollingUpdate": {"maxUnavailable": "25%"}});
        assert_eq!(rolling_limits(&d, 10), (0, 3));
        d["spec"]["updateStrategy"]["rollingUpdate"] = json!({"maxUnavailable": 0, "maxSurge": "10%"});
        assert_eq!(rolling_limits(&d, 5), (1, 0));
        d["spec"]["updateStrategy"]["rollingUpdate"] = json!({"maxUnavailable": 0, "maxSurge": 0});
        assert_eq!(rolling_limits(&d, 5), (0, 1));
        d["spec"]["updateStrategy"] = json!({"type": "OnDelete"});
        assert_eq!(update_strategy(&d), "OnDelete");
    }

    /// An author's own toleration for one of the defaults is not duplicated.
    #[test]
    fn an_explicit_toleration_is_not_duplicated() {
        let pod = build_daemonset_pod(
            "kube-system",
            "d",
            "u",
            "node1",
            &ds(json!([{
                "key": "node.kubernetes.io/disk-pressure",
                "operator": "Exists",
                "effect": "NoSchedule"
            }])),
        )
        .unwrap();
        let tol = pod["spec"]["tolerations"].as_array().unwrap();
        let n = tol
            .iter()
            .filter(|t| t["key"].as_str() == Some("node.kubernetes.io/disk-pressure"))
            .count();
        assert_eq!(n, 1, "duplicated an explicit toleration");
    }
}
