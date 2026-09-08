//! Volume binding — placing a pod where its storage can actually be.
//!
//! Two things go wrong without this, and both look like a scheduler bug.
//!
//! A pod whose PVC is already bound to a volume that lives on one node —
//! local NVMe, a StormBlock volume with a master — can only run on that node.
//! `spec.nodeAffinity` on the PersistentVolume says so, and a scheduler that
//! does not read it will place the pod somewhere else, where the kubelet
//! then fails to mount forever.
//!
//! A pod whose PVC is *not* bound yet, under a `WaitForFirstConsumer` class,
//! is the other half: nothing may be provisioned until a node is chosen,
//! because the volume is created where the pod landed. The scheduler picks the
//! node and writes `volume.kubernetes.io/selected-node` on the claim; the
//! external provisioner reads it and creates the volume there. Until the claim
//! is Bound the pod is *not* bound — binding it first is how a pod ends up
//! running on a node whose volume creation then fails.
//!
//! `stormblock-csi`'s default class is `WaitForFirstConsumer` and its operator
//! publishes `CSIStorageCapacity`, so both paths are load-bearing here rather
//! than theoretical.

use serde_json::Value;
use std::collections::HashMap;

/// Written by us; read by the external provisioner.
pub const ANN_SELECTED_NODE: &str = "volume.kubernetes.io/selected-node";

/// Everything about storage that a scheduling pass needs, listed once.
#[derive(Default)]
pub struct VolumeState {
    /// Claims by `(namespace, name)`.
    pub claims: HashMap<(String, String), Value>,
    /// Volumes by name.
    pub volumes: HashMap<String, Value>,
    /// StorageClasses by name.
    pub classes: HashMap<String, Value>,
    /// Published capacity, for `WaitForFirstConsumer` placement.
    pub capacities: Vec<Value>,
    /// Drivers that declare `storageCapacity: true`, by name.
    pub capacity_tracking: Vec<String>,
}

impl VolumeState {
    pub fn claim(&self, namespace: &str, name: &str) -> Option<&Value> {
        self.claims
            .get(&(namespace.to_string(), name.to_string()))
    }
}

/// The claims a pod mounts, by name.
///
/// Generic ephemeral volumes count: their claim is named `<pod>-<volume>` and
/// is as real as any other, which is exactly why it must be scheduled for.
pub fn pod_claims(pod: &Value) -> Vec<String> {
    let pod_name = pod["metadata"]["name"].as_str().unwrap_or("");
    pod["spec"]["volumes"]
        .as_array()
        .map(|vs| {
            vs.iter()
                .filter_map(|v| {
                    if let Some(c) = v["persistentVolumeClaim"]["claimName"].as_str() {
                        return Some(c.to_string());
                    }
                    if v.get("ephemeral").is_some() {
                        let vol_name = v["name"].as_str().unwrap_or("");
                        return Some(format!("{pod_name}-{vol_name}"));
                    }
                    None
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Does this pod use any persistent storage at all?
pub fn uses_storage(pod: &Value) -> bool {
    !pod_claims(pod).is_empty()
}

/// The claims of this pod that are not bound to a volume yet.
pub fn unbound_claims(pod: &Value, namespace: &str, state: &VolumeState) -> Vec<String> {
    pod_claims(pod)
        .into_iter()
        .filter(|c| match state.claim(namespace, c) {
            Some(pvc) => pvc["spec"]["volumeName"]
                .as_str()
                .unwrap_or("")
                .is_empty(),
            // A claim we cannot see is not a claim we can call bound.
            None => true,
        })
        .collect()
}

/// Can this node host this pod's volumes?
pub fn filter_node(pod: &Value, namespace: &str, node: &Value, state: &VolumeState) -> Result<(), String> {
    let labels = node["metadata"]["labels"].as_object();
    let node_name = node["metadata"]["name"].as_str().unwrap_or("");

    for claim_name in pod_claims(pod) {
        let pvc = match state.claim(namespace, &claim_name) {
            Some(p) => p,
            // The claim does not exist yet (an ephemeral volume's claim is
            // created by the controller). Nothing to check against.
            None => continue,
        };

        let volume_name = pvc["spec"]["volumeName"].as_str().unwrap_or("");
        if !volume_name.is_empty() {
            // Bound: the volume's own node affinity decides.
            let pv = match state.volumes.get(volume_name) {
                Some(pv) => pv,
                None => continue,
            };
            if let Some(terms) = pv["spec"]["nodeAffinity"]["required"]["nodeSelectorTerms"]
                .as_array()
                .filter(|t| !t.is_empty())
            {
                let ok = terms
                    .iter()
                    .any(|t| crate::filter::node_selector_term_matches(t, labels, node_name));
                if !ok {
                    return Err(format!(
                        "volume {volume_name} (claim {claim_name}) is not available on this node"
                    ));
                }
            }
            continue;
        }

        // Unbound. If the claim already picked a node, that is the only node.
        if let Some(selected) = pvc["metadata"]["annotations"][ANN_SELECTED_NODE]
            .as_str()
            .filter(|s| !s.is_empty())
        {
            if selected != node_name {
                return Err(format!(
                    "claim {claim_name} already selected node {selected}"
                ));
            }
        }

        // Unbound and to be provisioned here: does the driver say it has room?
        if let Some(class_name) = pvc["spec"]["storageClassName"].as_str().filter(|c| !c.is_empty())
        {
            if let Some(class) = state.classes.get(class_name) {
                let provisioner = class["provisioner"].as_str().unwrap_or("");
                // Only enforced for drivers that publish capacity. A driver
                // that does not is not saying "no room" — it is saying
                // nothing, and rejecting every node on silence would leave
                // every pod pending.
                if state.capacity_tracking.iter().any(|d| d == provisioner)
                    && !capacity_fits(pvc, class_name, node, state)
                {
                    return Err(format!(
                        "no published storage capacity for class {class_name} on this node"
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Is there a published `CSIStorageCapacity` covering this node, this class,
/// and at least the requested size?
fn capacity_fits(pvc: &Value, class_name: &str, node: &Value, state: &VolumeState) -> bool {
    let want = apimachinery::quantity::parse_bytes(
        pvc["spec"]["resources"]["requests"]["storage"]
            .as_str()
            .unwrap_or("0"),
    );
    let labels = &node["metadata"]["labels"];
    state.capacities.iter().any(|cap| {
        if cap["storageClassName"].as_str() != Some(class_name) {
            return false;
        }
        let topology = &cap["nodeTopology"];
        if !topology.is_null() && !apimachinery::selector::matches(topology, labels) {
            return false;
        }
        // `maximumVolumeSize` is the honest field for "can you make one this
        // big"; `capacity` is the total left and is the fallback the spec
        // allows when the driver does not publish a maximum.
        let limit = cap["maximumVolumeSize"]
            .as_str()
            .or_else(|| cap["capacity"].as_str())
            .unwrap_or("0");
        apimachinery::quantity::parse_bytes(limit) >= want
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(name: &str, zone: &str) -> Value {
        json!({"metadata": {"name": name, "labels": {
            "kubernetes.io/hostname": name, "topology.stormblock.io/zone": zone}}})
    }

    fn state_with(pvc: Value, pv: Option<Value>) -> VolumeState {
        let mut st = VolumeState::default();
        st.claims.insert(("default".into(), "c1".into()), pvc);
        if let Some(pv) = pv {
            let name = pv["metadata"]["name"].as_str().unwrap().to_string();
            st.volumes.insert(name, pv);
        }
        st
    }

    fn pod_with_claim() -> Value {
        json!({"metadata": {"name": "p1"},
               "spec": {"volumes": [{"name": "data",
                                     "persistentVolumeClaim": {"claimName": "c1"}}]}})
    }

    #[test]
    fn a_bound_local_volume_pins_the_pod_to_its_node() {
        let pvc = json!({"spec": {"volumeName": "pv1"}});
        let pv = json!({"metadata": {"name": "pv1"}, "spec": {"nodeAffinity": {"required": {
            "nodeSelectorTerms": [{"matchExpressions": [
                {"key": "kubernetes.io/hostname", "operator": "In", "values": ["node-a"]}]}]}}}});
        let st = state_with(pvc, Some(pv));
        let pod = pod_with_claim();
        assert!(filter_node(&pod, "default", &node("node-a", "z1"), &st).is_ok());
        assert!(filter_node(&pod, "default", &node("node-b", "z1"), &st).is_err());
    }

    #[test]
    fn a_bound_volume_without_affinity_runs_anywhere() {
        let pvc = json!({"spec": {"volumeName": "pv1"}});
        let pv = json!({"metadata": {"name": "pv1"}, "spec": {}});
        let st = state_with(pvc, Some(pv));
        assert!(filter_node(&pod_with_claim(), "default", &node("any", "z1"), &st).is_ok());
    }

    #[test]
    fn a_claim_that_already_chose_a_node_does_not_move() {
        let pvc = json!({"metadata": {"annotations": {ANN_SELECTED_NODE: "node-a"}}, "spec": {}});
        let st = state_with(pvc, None);
        let pod = pod_with_claim();
        assert!(filter_node(&pod, "default", &node("node-a", "z1"), &st).is_ok());
        assert!(filter_node(&pod, "default", &node("node-b", "z1"), &st).is_err());
    }

    #[test]
    fn capacity_is_enforced_only_for_drivers_that_publish_it() {
        let pvc = json!({"spec": {"storageClassName": "stormblock",
                                  "resources": {"requests": {"storage": "50Gi"}}}});
        let mut st = state_with(pvc, None);
        st.classes.insert(
            "stormblock".into(),
            json!({"provisioner": "stormblock.csi.storm.io",
                   "volumeBindingMode": "WaitForFirstConsumer"}),
        );
        let pod = pod_with_claim();

        // Driver publishes nothing: silence is not a refusal.
        assert!(filter_node(&pod, "default", &node("node-a", "z1"), &st).is_ok());

        // Driver tracks capacity, and has published only a small one for z1.
        st.capacity_tracking.push("stormblock.csi.storm.io".into());
        st.capacities.push(json!({
            "storageClassName": "stormblock",
            "nodeTopology": {"matchLabels": {"topology.stormblock.io/zone": "z1"}},
            "maximumVolumeSize": "10Gi"
        }));
        st.capacities.push(json!({
            "storageClassName": "stormblock",
            "nodeTopology": {"matchLabels": {"topology.stormblock.io/zone": "z2"}},
            "maximumVolumeSize": "100Gi"
        }));
        assert!(
            filter_node(&pod, "default", &node("node-a", "z1"), &st).is_err(),
            "10Gi of headroom cannot hold a 50Gi claim"
        );
        assert!(filter_node(&pod, "default", &node("node-b", "z2"), &st).is_ok());
    }

    #[test]
    fn ephemeral_volumes_name_their_claim_after_the_pod() {
        let pod = json!({"metadata": {"name": "web"},
                         "spec": {"volumes": [{"name": "scratch", "ephemeral": {}}]}});
        assert_eq!(pod_claims(&pod), vec!["web-scratch".to_string()]);
    }

    #[test]
    fn unbound_claims_are_the_ones_without_a_volume() {
        let mut st = VolumeState::default();
        st.claims.insert(
            ("default".into(), "c1".into()),
            json!({"spec": {"volumeName": "pv1"}}),
        );
        assert!(unbound_claims(&pod_with_claim(), "default", &st).is_empty());

        st.claims
            .insert(("default".into(), "c1".into()), json!({"spec": {}}));
        assert_eq!(unbound_claims(&pod_with_claim(), "default", &st), vec!["c1"]);
    }
}
