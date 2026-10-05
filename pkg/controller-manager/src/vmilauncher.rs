//! VMI launcher Pods — a VirtualMachineInstance's Pod on the pod network
//! (#203; owner's choice B on rustkube-node#88).
//!
//! A VMI on `networks: [{pod: {}}]` gets its own sandbox and a CNI ADD on
//! its node, but two things on the pod network read a *Pod* object, and a
//! VMI is not one: Cilium derives the endpoint's identity (and so
//! NetworkPolicy) from the Pod the CNI ADD names, and Services select Pods.
//! So, as KubeVirt does, each such VMI gets a `virt-launcher` Pod, and the
//! kubelet adopts it: it names that Pod in the CNI ADD, writes its status
//! (`phase`, `podIP`/`podIPs`, Ready) and confirms its deletion once the
//! machine is gone. The kubelet never runs it as a pod.
//!
//! The Pod, KubeVirt's shape, once the VMI has `status.nodeName`:
//!
//! - `virt-launcher-<vmi>-<5 random>` in the VMI's namespace;
//! - the VMI's labels, plus `kubevirt.io: virt-launcher`,
//!   `kubevirt.io/created-by: <vmi uid>` and `vm.kubevirt.io/name: <vmi>`;
//! - owned by the VMI (`controller`, `blockOwnerDeletion`), so deleting the
//!   VMI deletes it;
//! - `spec.nodeName` set — it is never scheduled on its own; the VMI was.
//!   One container, `compute`, whose image is a placeholder. No resource
//!   requests: the scheduler charges the VMI itself, and the Pod costs its
//!   node one pod slot, as upstream's does.
//!
//! One per VMI, on the VMI's node. While a live migration has a target
//! (#184) the target node gets one too, labelled
//! `kubevirt.io/migrationJobUID`, as upstream's target Pod is: the receiving
//! machine needs a pod identity before the VMI is moved to it. When the
//! migration succeeds the VMI's node becomes the target and the source's Pod
//! is deleted; when it fails the target's is. A launcher Pod on any other
//! node is deleted the same way — gracefully: the kubelet there confirms it
//! once no machine of the VMI runs there.
//!
//! A launcher that ended (`Succeeded`/`Failed`, written by the kubelet when
//! the machine stopped) is left as it is, and so is everything of a VMI that
//! has finished: a new machine is a new VMI, with a new uid and its own Pod.

use crate::owned::{self, Controller, Deps};
use crate::runner::ApiClient;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::sync::Arc;
use tracing::{debug, info};

/// KubeVirt's launcher label and value.
pub const LAUNCHER_LABEL: &str = "kubevirt.io";
pub const LAUNCHER: &str = "virt-launcher";
/// The VMI (by uid) a launcher Pod is for.
pub const CREATED_BY: &str = "kubevirt.io/created-by";
/// The VMI (by name), as KubeVirt labels it.
pub const VM_NAME: &str = "vm.kubevirt.io/name";
/// A migration target's Pod names its migration.
pub const MIGRATION_JOB: &str = "kubevirt.io/migrationJobUID";
/// stormvm's host-bridge annotation (`storm.io/bridge`, or
/// `storm.io/bridge.<interface>` for one interface): it wins over the pod
/// network, so such an interface needs no Pod (stormvm-spec `kube.rs`).
const ANN_BRIDGE: &str = "storm.io/bridge";
/// The `compute` container's image. Never pulled: the kubelet adopts a
/// launcher Pod instead of running it.
const PLACEHOLDER_IMAGE: &str = "storm.io/virt-launcher:placeholder";

pub struct VmiLauncherController {
    api: Arc<ApiClient>,
    recorder: crate::events::EventRecorder,
}

/// What one reconcile does.
#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    /// Nodes that get a new launcher Pod.
    pub create: Vec<String>,
    /// Launcher Pods to delete (on a node the VMI is not on).
    pub delete: Vec<Value>,
}

/// Is any of this VMI's interfaces on the pod network, as the kubelet's
/// spec reader (stormvm-spec) decides it: an interface whose network of the
/// same name is `pod: {}`, and that no `storm.io/bridge` annotation puts on
/// a host bridge instead.
pub fn on_pod_network(vmi: &Value) -> bool {
    let ann = |key: &str| vmi["metadata"]["annotations"][key].as_str().is_some();
    let empty = Vec::new();
    let networks = vmi["spec"]["networks"].as_array().unwrap_or(&empty);
    vmi["spec"]["domain"]["devices"]["interfaces"]
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .filter_map(|i| i["name"].as_str().filter(|n| !n.is_empty()))
        .any(|iface| {
            !ann(ANN_BRIDGE)
                && !ann(&format!("{ANN_BRIDGE}.{iface}"))
                && networks
                    .iter()
                    .any(|n| n["name"].as_str() == Some(iface) && !n["pod"].is_null())
        })
}

/// Is this Pod a launcher of the VMI with `uid`?
pub fn launcher_of(pod: &Value, uid: &str) -> bool {
    let labels = &pod["metadata"]["labels"];
    !uid.is_empty()
        && labels[LAUNCHER_LABEL].as_str() == Some(LAUNCHER)
        && labels[CREATED_BY].as_str() == Some(uid)
}

fn non_empty(v: &Value) -> Option<&str> {
    v.as_str().filter(|s| !s.is_empty())
}

/// The nodes this VMI needs a launcher on: its own, and a migration's
/// target from when it is chosen until the migration fails. A completed
/// migration keeps its target until the VMI has been moved there (which
/// makes it the VMI's own node).
fn wanted_nodes(vmi: &Value) -> BTreeSet<String> {
    let mut nodes = BTreeSet::new();
    if let Some(n) = non_empty(&vmi["status"]["nodeName"]) {
        nodes.insert(n.to_string());
    }
    let m = &vmi["status"]["migrationState"];
    if non_empty(&m["migrationUid"]).is_some() && m["failed"].as_bool() != Some(true) {
        if let Some(t) = non_empty(&m["targetNode"]) {
            nodes.insert(t.to_string());
        }
    }
    nodes
}

/// The next step for a VMI's launcher Pods, given the Pods it owns.
pub fn plan(vmi: &Value, owned: &[Value]) -> Plan {
    let uid = vmi["metadata"]["uid"].as_str().unwrap_or("");
    if uid.is_empty()
        || !vmi["metadata"]["deletionTimestamp"].is_null()
        || !on_pod_network(vmi)
        // Finished: its launcher says how it ended; nothing more is made.
        || matches!(vmi["status"]["phase"].as_str(), Some("Failed" | "Succeeded"))
    {
        return Plan::default();
    }
    let wanted = wanted_nodes(vmi);
    if wanted.is_empty() {
        // Not placed yet: no node to put the Pod on.
        return Plan::default();
    }
    let live: Vec<&Value> = owned
        .iter()
        .filter(|p| launcher_of(p, uid) && p["metadata"]["deletionTimestamp"].is_null())
        .collect();
    let node_of = |p: &Value| non_empty(&p["spec"]["nodeName"]).unwrap_or("").to_string();
    Plan {
        // A terminating Pod does not count: the kubelet lets it go only
        // when no machine of this VMI runs there, so a machine that needs
        // one (a migration back to its old node) needs a new one.
        create: wanted
            .iter()
            .filter(|n| !live.iter().any(|p| &node_of(p) == *n))
            .cloned()
            .collect(),
        delete: live
            .into_iter()
            .filter(|p| !wanted.contains(&node_of(p)))
            .cloned()
            .collect(),
    }
}

/// The launcher Pod for `vmi` on `node`, with its random suffix.
pub fn launcher_pod(vmi: &Value, node: &str, suffix: &str) -> Value {
    let meta = &vmi["metadata"];
    let name = meta["name"].as_str().unwrap_or("");
    let uid = meta["uid"].as_str().unwrap_or("");
    let mut labels = match &meta["labels"] {
        Value::Object(m) => m.clone(),
        _ => Default::default(),
    };
    labels.insert(LAUNCHER_LABEL.into(), json!(LAUNCHER));
    labels.insert(CREATED_BY.into(), json!(uid));
    labels.insert(VM_NAME.into(), json!(name));
    let m = &vmi["status"]["migrationState"];
    let is_target = non_empty(&vmi["status"]["nodeName"]) != Some(node)
        && non_empty(&m["targetNode"]) == Some(node);
    if is_target {
        if let Some(job) = non_empty(&m["migrationUid"]) {
            labels.insert(MIGRATION_JOB.into(), json!(job));
        }
    }
    let mut spec = json!({
        "nodeName": node,
        "restartPolicy": "Never",
        "automountServiceAccountToken": false,
        "containers": [{"name": "compute", "image": PLACEHOLDER_IMAGE}],
    });
    if let Some(g) = vmi["spec"]["terminationGracePeriodSeconds"].as_i64() {
        spec["terminationGracePeriodSeconds"] = json!(g);
    }
    json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": format!("virt-launcher-{name}-{suffix}"),
            "namespace": meta["namespace"].as_str().unwrap_or("default"),
            "labels": labels,
            "annotations": {"kubevirt.io/domain": name},
            "ownerReferences": [{
                "apiVersion": "kubevirt.io/v1",
                "kind": "VirtualMachineInstance",
                "name": name,
                "uid": uid,
                "controller": true,
                "blockOwnerDeletion": true
            }]
        },
        "spec": spec,
    })
}

/// KubeVirt's launcher suffix: five lowercase alphanumerics.
fn suffix() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..5].to_string()
}

impl VmiLauncherController {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self {
            recorder: crate::events::EventRecorder::new(api.clone(), "virtualmachine-controller"),
            api,
        }
    }

    pub async fn run(&self) {
        owned::run(&self.api, self).await;
    }

    async fn reconcile_vmi(&self, vmi: &Value, owned: &[Value]) -> anyhow::Result<()> {
        let plan = plan(vmi, owned);
        let namespace = vmi["metadata"]["namespace"].as_str().unwrap_or("default");
        let name = vmi["metadata"]["name"].as_str().unwrap_or("");
        for node in &plan.create {
            let pod = launcher_pod(vmi, node, &suffix());
            let created = self
                .api
                .create(&format!("/api/v1/namespaces/{namespace}/pods"), &pod)
                .await?;
            let pod_name = created["metadata"]["name"].as_str().unwrap_or("?");
            info!("VirtualMachineInstance {namespace}/{name}: launcher Pod {pod_name} on {node}");
            self.recorder
                .event(
                    vmi,
                    "Normal",
                    "SuccessfulCreate",
                    &format!("Created virtual machine pod {pod_name}"),
                )
                .await;
        }
        for pod in &plan.delete {
            let pod_name = pod["metadata"]["name"].as_str().unwrap_or("");
            let node = pod["spec"]["nodeName"].as_str().unwrap_or("");
            match self
                .api
                .delete_observed(&format!("/api/v1/namespaces/{namespace}/pods/{pod_name}"), pod)
                .await
            {
                Ok(_) => {
                    info!(
                        "VirtualMachineInstance {namespace}/{name}: launcher Pod {pod_name} \
                         on {node} deleted (the VMI is not there)"
                    );
                    self.recorder
                        .event(
                            vmi,
                            "Normal",
                            "SuccessfulDelete",
                            &format!("Deleted virtual machine pod {pod_name}"),
                        )
                        .await;
                }
                // A precondition miss is a newer Pod: the watch brings it.
                Err(e) => debug!("launcher Pod {namespace}/{pod_name}: delete: {e}"),
            }
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Controller for VmiLauncherController {
    fn name(&self) -> &'static str {
        "vmi-launcher"
    }
    fn primary(&self) -> &'static str {
        "/apis/kubevirt.io/v1/virtualmachineinstances"
    }
    fn children(&self) -> Option<&'static str> {
        Some("/api/v1/pods")
    }
    async fn reconcile(&self, vmi: &Value, pods: &[Value], _deps: &Deps) -> anyhow::Result<()> {
        self.reconcile_vmi(vmi, pods).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vmi() -> Value {
        json!({
            "apiVersion": "kubevirt.io/v1", "kind": "VirtualMachineInstance",
            "metadata": {"name": "web-1", "namespace": "apps", "uid": "vmi-1",
                "labels": {"app": "web"}},
            "spec": {
                "domain": {"devices": {"interfaces": [{"name": "default", "bridge": {}}]}},
                "networks": [{"name": "default", "pod": {}}],
                "terminationGracePeriodSeconds": 30
            },
            "status": {"phase": "Scheduled", "nodeName": "n1"}
        })
    }

    fn launcher(name: &str, node: &str) -> Value {
        let mut p = launcher_pod(&vmi(), node, "abcde");
        p["metadata"]["name"] = json!(name);
        p["metadata"]["uid"] = json!(format!("pod-{name}"));
        p["metadata"]["resourceVersion"] = json!("5");
        p
    }

    #[test]
    fn the_pod_has_the_shape_the_kubelet_adopts() {
        let p = launcher_pod(&vmi(), "n1", "abcde");
        let m = &p["metadata"];
        assert_eq!(m["name"], "virt-launcher-web-1-abcde");
        assert_eq!(m["namespace"], "apps");
        assert_eq!(m["labels"]["app"], "web", "the VMI's own labels, for Services");
        assert_eq!(m["labels"]["kubevirt.io"], "virt-launcher");
        assert_eq!(m["labels"]["kubevirt.io/created-by"], "vmi-1");
        assert_eq!(m["labels"]["vm.kubevirt.io/name"], "web-1");
        assert!(m["labels"].get(MIGRATION_JOB).is_none());
        let owner = &m["ownerReferences"][0];
        assert_eq!(owner["kind"], "VirtualMachineInstance");
        assert_eq!(owner["uid"], "vmi-1");
        assert_eq!(owner["controller"], true);
        assert_eq!(owner["blockOwnerDeletion"], true);
        assert_eq!(p["spec"]["nodeName"], "n1");
        assert_eq!(p["spec"]["containers"][0]["name"], "compute");
        assert!(p["spec"]["containers"][0]["resources"].is_null(), "the VMI is charged, not the Pod");
        assert_eq!(p["spec"]["terminationGracePeriodSeconds"], 30);
        assert!(p.get("status").is_none(), "status is the kubelet's");
        assert!(launcher_of(&p, "vmi-1") && !launcher_of(&p, "vmi-2"));
        let s = suffix();
        assert_eq!(s.len(), 5);
        assert!(s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
    }

    #[test]
    fn only_a_vmi_on_the_pod_network_gets_one() {
        assert!(on_pod_network(&vmi()));
        let mut bridged = vmi();
        bridged["metadata"]["annotations"] = json!({"storm.io/bridge": "stormbr0"});
        assert!(!on_pod_network(&bridged), "a host bridge wins over the pod network");
        let mut one_leg = vmi();
        one_leg["metadata"]["annotations"] = json!({"storm.io/bridge.default": "stormbr0"});
        assert!(!on_pod_network(&one_leg));
        let mut two = vmi();
        two["spec"]["domain"]["devices"]["interfaces"] =
            json!([{"name": "default"}, {"name": "net1"}]);
        two["spec"]["networks"] =
            json!([{"name": "default", "pod": {}}, {"name": "net1", "multus": {"networkName": "x"}}]);
        two["metadata"]["annotations"] = json!({"storm.io/bridge.net1": "stormbr0"});
        assert!(on_pod_network(&two), "the other leg is still on it");
        let mut multus = vmi();
        multus["spec"]["networks"] = json!([{"name": "default", "multus": {"networkName": "x"}}]);
        assert!(!on_pod_network(&multus));
        let mut none = vmi();
        none["spec"]["domain"]["devices"]["interfaces"] = json!([]);
        assert!(!on_pod_network(&none), "a network no interface uses attaches nothing");
        assert_eq!(plan(&multus, &[]), Plan::default());
    }

    #[test]
    fn one_per_vmi_once_it_has_a_node() {
        let mut unplaced = vmi();
        unplaced["status"] = json!({"phase": "Pending"});
        assert_eq!(plan(&unplaced, &[]), Plan::default());
        assert_eq!(plan(&vmi(), &[]).create, vec!["n1".to_string()]);
        // Made already: nothing, whatever its phase (an ended one is left).
        let mut ended = launcher("a", "n1");
        ended["status"] = json!({"phase": "Failed"});
        assert_eq!(plan(&vmi(), &[launcher("a", "n1")]), Plan::default());
        assert_eq!(plan(&vmi(), &[ended]), Plan::default());
        // Someone else's Pod of the same owner index does not count.
        let mut other = launcher("b", "n1");
        other["metadata"]["labels"]["kubevirt.io/created-by"] = json!("vmi-2");
        assert_eq!(plan(&vmi(), &[other]).create, vec!["n1".to_string()]);
    }

    #[test]
    fn a_finished_or_deleted_vmi_is_left_alone() {
        for phase in ["Failed", "Succeeded"] {
            let mut v = vmi();
            v["status"]["phase"] = json!(phase);
            assert_eq!(plan(&v, &[]), Plan::default());
            assert_eq!(plan(&v, &[launcher("a", "n9")]), Plan::default());
        }
        let mut going = vmi();
        going["metadata"]["deletionTimestamp"] = json!("2026-10-05T00:00:00Z");
        assert_eq!(plan(&going, &[]), Plan::default(), "the GC takes the Pod with it");
    }

    #[test]
    fn a_migration_target_gets_its_own_and_the_loser_goes() {
        let mut v = vmi();
        v["status"]["phase"] = json!("Running");
        v["status"]["migrationState"] = json!({"migrationUid": "mig-1", "sourceNode": "n1"});
        let src = launcher("a", "n1");
        // No target chosen yet: nothing to do.
        assert_eq!(plan(&v, &[src.clone()]), Plan::default());
        // Target chosen: a Pod there, labelled with the migration.
        v["status"]["migrationState"]["targetNode"] = json!("n2");
        assert_eq!(plan(&v, &[src.clone()]).create, vec!["n2".to_string()]);
        let tgt_pod = launcher_pod(&v, "n2", "fghij");
        assert_eq!(tgt_pod["metadata"]["labels"][MIGRATION_JOB], "mig-1");
        let tgt = launcher("b", "n2");
        assert_eq!(plan(&v, &[src.clone(), tgt.clone()]), Plan::default());
        // Completed, the VMI not yet moved: both stay.
        v["status"]["migrationState"]["completed"] = json!(true);
        assert_eq!(plan(&v, &[src.clone(), tgt.clone()]), Plan::default());
        // Moved: the source's goes.
        v["status"]["nodeName"] = json!("n2");
        let p = plan(&v, &[src.clone(), tgt.clone()]);
        assert!(p.create.is_empty());
        assert_eq!(p.delete, vec![src.clone()]);
        // A failed migration: the target's goes, the source's stays.
        let mut f = vmi();
        f["status"]["phase"] = json!("Running");
        f["status"]["migrationState"] =
            json!({"migrationUid": "mig-2", "sourceNode": "n1", "targetNode": "n2", "failed": true});
        assert_eq!(plan(&f, &[src.clone(), tgt.clone()]).delete, vec![tgt.clone()]);
    }

    #[test]
    fn a_terminating_pod_is_not_the_launcher() {
        // Back on a node whose old Pod is still being let go: a new one.
        let mut old = launcher("a", "n1");
        old["metadata"]["deletionTimestamp"] = json!("2026-10-05T00:00:00Z");
        let p = plan(&vmi(), &[old]);
        assert_eq!(p.create, vec!["n1".to_string()]);
        assert!(p.delete.is_empty(), "not deleted twice");
    }
}
