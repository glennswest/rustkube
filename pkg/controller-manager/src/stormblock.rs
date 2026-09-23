//! The provisioner for the in-kubelet stormblock PVC path.
//!
//! On this cluster a PVC is served by the kubelet directly: it rounds the
//! claim up to a size class, CoW-clones the sealed blank through stormblock's
//! own API, attaches over ublk, and hands stormpump the device. No CSI driver
//! and no sidecars — which is deliberate, because the alternative is shipping
//! a Go sidecar as a golden in every node image, and pulling anything at join
//! time is the first thing `stormcos/docs/CLUSTER.md` rules out.
//!
//! What that path never did is tell the control plane. It provisions at pod
//! start from the PVC object alone, so nothing created a `PersistentVolume`,
//! set `spec.volumeName`, or moved either object to `Bound` (#71). A claim
//! backing a perfectly healthy pod read `Pending` for the life of that pod,
//! and everything that gates on `Bound` — a StatefulSet rollout, an operator's
//! readiness check, a human deciding whether to page somebody — saw a cluster
//! stuck when it was not.
//!
//! This controller is the other half: it writes the objects that describe what
//! the node has already done, or is about to do.
//!
//! **Both orders have to converge.** The kubelet may provision before this
//! runs or after it, and neither is an error — so this reconciles toward one
//! bound pair rather than assuming it goes first. The volume name is the
//! contract between the two halves: `pvc-<ns>-<claim>`, which is
//! `storage::volume_name()` on the node. One name, derived on both sides from
//! the same rule, is what makes the object this binds and the volume that one
//! mounts the same thing.

use crate::events::EventRecorder;
use crate::runner::ApiClient;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::time::{self, Duration};
use tracing::{debug, error, info};

/// The class this provisions for. A claim of any other class belongs to
/// somebody else, and the kubelet agrees — `storage::provisioned_here`
/// (rustkube-node#44) gates the node half on the same name.
pub const STORAGE_CLASS: &str = "stormblock";

/// Written on a PV so this controller can recognise its own work.
const PROVISIONED_BY: &str = "pv.kubernetes.io/provisioned-by";

/// The provisioner name in that annotation.
const PROVISIONER: &str = "stormblock.storm.io/in-kubelet";

/// Where the volume lives, when it is known. The scheduler writes the same
/// annotation on the claim for `WaitForFirstConsumer`, and a local volume is
/// only reachable from the node holding it.
const ANN_NODE: &str = "volume.kubernetes.io/selected-node";

pub struct StormblockProvisioner {
    api: Arc<ApiClient>,
    events: EventRecorder,
}

impl StormblockProvisioner {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self {
            events: EventRecorder::new(api.clone(), "stormblock-provisioner"),
            api,
        }
    }

    pub async fn run(&self) {
        info!("stormblock provisioner started");
        let mut interval = time::interval(Duration::from_secs(2));
        loop {
            interval.tick().await;
            if let Err(e) = self.reconcile().await {
                error!("stormblock provisioner: {e}");
            }
        }
    }

    async fn reconcile(&self) -> anyhow::Result<()> {
        let ns_list: Value = self.api.list("/api/v1/namespaces").await?;
        for ns in ns_list["items"].as_array().cloned().unwrap_or_default() {
            let name = ns["metadata"]["name"].as_str().unwrap_or("default");
            if let Err(e) = self.provision_namespace(name).await {
                debug!("stormblock provisioner in {name}: {e}");
            }
        }
        self.reclaim().await
    }

    /// Give every unbound claim of our class a volume object.
    async fn provision_namespace(&self, namespace: &str) -> anyhow::Result<()> {
        let claims: Value = self
            .api
            .list(&format!(
                "/api/v1/namespaces/{namespace}/persistentvolumeclaims"
            ))
            .await?;
        let claims = claims["items"].as_array().cloned().unwrap_or_default();
        if claims.is_empty() {
            return Ok(());
        }

        for pvc in &claims {
            let name = pvc["metadata"]["name"].as_str().unwrap_or("");
            if name.is_empty() || !pvc["metadata"]["deletionTimestamp"].is_null() {
                continue;
            }
            // Ours? The empty string is an explicit opt-out asking for an
            // administrator's volume, so it is not ours either — the same
            // distinction `claim_class` draws and the kubelet now draws.
            if pvc["spec"]["storageClassName"].as_str() != Some(STORAGE_CLASS) {
                continue;
            }
            // Already has a volume: the binder owns it from here.
            if pvc["spec"]["volumeName"]
                .as_str()
                .is_some_and(|v| !v.is_empty())
            {
                continue;
            }
            if let Err(e) = self.ensure_volume(namespace, name, pvc).await {
                debug!("stormblock: claim {namespace}/{name}: {e}");
            }
        }
        Ok(())
    }

    /// The PV for one claim, created if it is not there.
    ///
    /// Not bound here: `persistentvolume.rs` is the binder, it already matches
    /// capacity, access modes and class, and a second writer of `claimRef`
    /// would race it. This only has to make a volume the binder can find, and
    /// pre-binding it with `claimRef` is how upstream's provisioners hand one
    /// over without that race.
    async fn ensure_volume(
        &self,
        namespace: &str,
        claim: &str,
        pvc: &Value,
    ) -> anyhow::Result<()> {
        // **WaitForFirstConsumer, honoured.** A stormblock clone is made by, and
        // lives on, the node that runs the first pod using it, and until the
        // scheduler has picked that node there is nowhere to say the volume
        // is. This created the PV as soon as the claim existed, with no node
        // and no volume source, and the binder bound it at once — so the
        // scheduler never saw an unbound claim, never chose a node for it, and
        // the claim read Bound to a volume that existed nowhere.
        let Some(node) = pvc["metadata"]["annotations"][ANN_NODE].as_str() else {
            return Ok(());
        };
        let pv_name = volume_name(namespace, claim);
        let path = format!("/api/v1/persistentvolumes/{pv_name}");
        if self.api.get(&path).await.map(|r| r.status().is_success()).unwrap_or(false) {
            return Ok(());
        }

        // What the claim asked for. The node rounds this up to a size class
        // and that class is the ceiling; reporting the request rather than the
        // class would promise less than the volume has, and reporting the
        // class would need this side to know the ladder. The node writes the
        // real capacity onto the PV when it provisions.
        let request = pvc["spec"]["resources"]["requests"]["storage"]
            .as_str()
            .unwrap_or("1Mi");
        let modes = pvc["spec"]["accessModes"].clone();
        let reclaim = pvc["spec"]["persistentVolumeReclaimPolicy"]
            .as_str()
            .unwrap_or("Delete");

        let mut pv = json!({
            "apiVersion": "v1",
            "kind": "PersistentVolume",
            "metadata": {
                "name": pv_name,
                "annotations": { PROVISIONED_BY: PROVISIONER },
            },
            "spec": {
                "capacity": { "storage": request },
                "accessModes": if modes.is_null() { json!(["ReadWriteOnce"]) } else { modes },
                "persistentVolumeReclaimPolicy": reclaim,
                "storageClassName": STORAGE_CLASS,
                // The volume behind it: the name the node clones to. The
                // kubelet resolves a bound claim through this handle, and the
                // node's own PV for the claim (same name) carries the same one.
                "csi": { "driver": "stormblock.storm.io", "volumeHandle": pv_name },
                // Pre-bound to the claim that caused it, so no other claim can
                // take it between this write and the binder's next pass.
                "claimRef": {
                    "kind": "PersistentVolumeClaim",
                    "apiVersion": "v1",
                    "namespace": namespace,
                    "name": claim,
                    "uid": pvc["metadata"]["uid"].clone(),
                },
            },
            "status": { "phase": "Available" },
        });

        // A stormblock clone is attached over ublk on one node, so the volume
        // is only reachable there. Saying so in `nodeAffinity` is what stops
        // the scheduler placing a later pod somewhere the data is not — the
        // volume-aware filters already read it (#56).
        pv["spec"]["nodeAffinity"] = json!({
            "required": { "nodeSelectorTerms": [{
                "matchExpressions": [{
                    "key": "kubernetes.io/hostname",
                    "operator": "In",
                    "values": [node],
                }]
            }]}
        });

        self.api.create("/api/v1/persistentvolumes", &pv).await?;
        info!("stormblock: created PV {pv_name} for claim {namespace}/{claim}");
        self.events
            .event(
                pvc,
                "Normal",
                "Provisioning",
                &format!("stormblock volume {pv_name} for this claim"),
            )
            .await;
        Ok(())
    }

    /// A released volume: honour its reclaim policy.
    ///
    /// `Retain` is the whole policy — the volume stays, the PV stays
    /// `Released`, and an administrator decides. Nothing to do but say so
    /// once.
    ///
    /// `Delete` is **not** finished here, and the PV is deliberately left
    /// `Released` rather than removed. Deleting the object without deleting
    /// the clone behind it would turn a visible leak into an invisible one:
    /// the slab keeps the data, nothing in the API refers to it any more, and
    /// the space is unaccounted for. Worse, the volume name is derived from
    /// the claim's — so a later, unrelated claim of that name in that
    /// namespace would silently adopt the previous tenant's contents, which
    /// is the failure worth being loudest about.
    ///
    /// The engine can only be reached from the node holding the volume:
    /// stormblock's management API is loopback by default, which is why the
    /// kubelet talks to `127.0.0.1:9090` and this cannot. The route already
    /// exists in the shape of rustkube#61 — control plane to kubelet, kubelet
    /// to the loopback service — and needs an endpoint on the node
    /// (rustkube-node#46). Until that lands this reports the leak on every
    /// pass rather than hiding it.
    async fn reclaim(&self) -> anyhow::Result<()> {
        let pvs: Value = self.api.list("/api/v1/persistentvolumes").await?;
        for pv in pvs["items"].as_array().cloned().unwrap_or_default() {
            if pv["metadata"]["annotations"][PROVISIONED_BY].as_str() != Some(PROVISIONER) {
                continue;
            }
            if pv["status"]["phase"].as_str() != Some("Released") {
                continue;
            }
            let name = pv["metadata"]["name"].as_str().unwrap_or("");
            match pv["spec"]["persistentVolumeReclaimPolicy"].as_str() {
                Some("Retain") | None => {
                    self.events
                        .event(
                            &pv,
                            "Normal",
                            "VolumeRetained",
                            "Released; the stormblock volume is kept for an administrator, \
                             as reclaimPolicy: Retain asks",
                        )
                        .await;
                }
                _ => {
                    self.events
                        .event(
                            &pv,
                            "Normal",
                            "VolumeReclaiming",
                            &format!(
                                "Released with reclaimPolicy: Delete: the node holding \
                                 stormblock volume {name} deletes it and then this PV \
                                 (rustkube-node reclaim_released)."
                            ),
                        )
                        .await;
                }
            }
        }
        Ok(())
    }
}

/// The volume name for a claim.
///
/// **The contract with the node.** `storage::volume_name()` in the kubelet
/// derives the same string, and the two must agree or the object this binds is
/// not the volume that one mounts. Keyed on namespace and claim rather than
/// the pod's UID so a restarted pod is reunited with its data, which is the
/// entire difference between a claim and a scratch directory.
pub fn volume_name(namespace: &str, claim: &str) -> String {
    format!("pvc-{namespace}-{claim}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_volume_name_is_the_one_the_kubelet_derives() {
        // Written out rather than computed, because the point of the test is
        // that this string matches a rule in another repository. If the node's
        // `storage::volume_name` changes, this has to be changed with it, and
        // a test that recomputed the rule would agree with itself and miss it.
        assert_eq!(volume_name("default", "data"), "pvc-default-data");
        assert_eq!(volume_name("team-a", "postgres-0"), "pvc-team-a-postgres-0");
    }
}
