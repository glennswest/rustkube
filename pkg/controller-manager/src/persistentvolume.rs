//! PersistentVolume / PersistentVolumeClaim lifecycle — the binder.
//!
//! The API served both objects and nothing ever acted on them (#56): a PVC
//! stayed `Pending` forever, no PV was ever bound, and every pod that mounted
//! one hung in `ContainerCreating`. This is upstream's `pv_controller`: the
//! part of the control plane that decides which volume satisfies which claim,
//! keeps the two pointing at each other, and holds the object alive while
//! something is still using it.
//!
//! **What this deliberately does not do is provision.** In this cluster the
//! volumes come from StormBlock — a PVC is a CoW clone of a blank filesystem
//! template on sbregistry — and the code that talks to the engine is the CSI
//! driver in `stormblock-csi`, driven by the standard
//! `kubernetes-csi/external-provisioner` sidecar. That sidecar is an upstream
//! Go binary and it acts on exactly one signal: the annotation
//! `volume.kubernetes.io/storage-provisioner` naming its driver, which the
//! in-tree controller — this file — is what puts there. Missing that
//! annotation is why nothing provisions; it is not a StormBlock detail, and an
//! OpenShift CSI driver, or any other out-of-tree provisioner, is unblocked by
//! the same line. So the split is: we own binding, the phases, the protection
//! finalizers and the hand-off signal; the driver owns the bytes.
//!
//! The consequence worth stating: **rustkube provisions nothing itself.** A
//! StorageClass whose provisioner is `kubernetes.io/no-provisioner` gets
//! static binding against PVs an administrator created, and everything else is
//! handed to the driver that claims it.

use crate::events::EventRecorder;
use crate::runner::ApiClient;
use apimachinery::quantity::parse_bytes;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::time::{self, Duration};
use tracing::{debug, error, info};

/// Set on a PVC to name the provisioner that must act on it. The current
/// spelling and the beta one are both written: external-provisioner has read
/// the beta key since forever and still does, and mixed-age sidecars are
/// normal in a cluster that runs vendor drivers.
const ANN_STORAGE_PROVISIONER: &str = "volume.kubernetes.io/storage-provisioner";
const ANN_STORAGE_PROVISIONER_BETA: &str = "volume.beta.kubernetes.io/storage-provisioner";
/// Written by the scheduler for `WaitForFirstConsumer` classes; the
/// provisioner reads it to place the volume where the pod already landed.
pub const ANN_SELECTED_NODE: &str = "volume.kubernetes.io/selected-node";
/// Set by whoever provisioned a PV, so the deleter knows whose it is.
const ANN_PROVISIONED_BY: &str = "pv.kubernetes.io/provisioned-by";
/// "This claim's spec.volumeName was filled in and is done."
const ANN_BIND_COMPLETED: &str = "pv.kubernetes.io/bind-completed";
/// "The controller chose this binding" — as opposed to a user pre-binding.
const ANN_BOUND_BY_CONTROLLER: &str = "pv.kubernetes.io/bound-by-controller";
/// The class marked default, used when a claim names none.
const ANN_IS_DEFAULT_CLASS: &str = "storageclass.kubernetes.io/is-default-class";
const ANN_IS_DEFAULT_CLASS_BETA: &str = "storageclass.beta.kubernetes.io/is-default-class";

/// Keeps a claim alive while a pod still mounts it.
const PVC_PROTECTION: &str = "kubernetes.io/pvc-protection";
/// Keeps a volume alive while a claim is still bound to it.
const PV_PROTECTION: &str = "kubernetes.io/pv-protection";

/// The provisioner name that means "there is no provisioner" — static PVs
/// only, which is how a local-disk class is written.
const NO_PROVISIONER: &str = "kubernetes.io/no-provisioner";

pub struct PersistentVolumeController {
    api: Arc<ApiClient>,
    events: EventRecorder,
}

impl PersistentVolumeController {
    pub fn new(api: Arc<ApiClient>) -> Self {
        let events = EventRecorder::new(api.clone(), "persistentvolume-controller");
        Self { api, events }
    }

    pub async fn run(&self) {
        info!("PersistentVolume controller started");
        let mut interval = time::interval(Duration::from_secs(3));
        loop {
            interval.tick().await;
            if let Err(e) = self.reconcile().await {
                error!("PersistentVolume reconcile error: {e}");
            }
        }
    }

    async fn reconcile(&self) -> anyhow::Result<()> {
        let pv_list: Value = self.api.list("/api/v1/persistentvolumes").await?;
        let mut pvs = pv_list["items"].as_array().cloned().unwrap_or_default();

        let class_list: Value = self
            .api
            .list("/apis/storage.k8s.io/v1/storageclasses")
            .await?;
        let classes: HashMap<String, Value> = class_list["items"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|c| {
                c["metadata"]["name"]
                    .as_str()
                    .map(|n| (n.to_string(), c.clone()))
            })
            .collect();
        let default_class = default_class_name(&classes);

        // Claims first: a bind is written volume-side then claim-side, and the
        // volume pass below picks up whatever the claim pass decided.
        let ns_list: Value = self.api.list("/api/v1/namespaces").await?;
        for ns in ns_list["items"].as_array().cloned().unwrap_or_default() {
            let namespace = match ns["metadata"]["name"].as_str() {
                Some(n) => n.to_string(),
                None => continue,
            };
            let pvc_list: Value = self
                .api
                .list(&format!(
                    "/api/v1/namespaces/{namespace}/persistentvolumeclaims"
                ))
                .await?;
            let pvcs = pvc_list["items"].as_array().cloned().unwrap_or_default();
            if pvcs.is_empty() {
                continue;
            }
            // Only paid for when the namespace actually has claims.
            let pod_list: Value = self
                .api
                .list(&format!("/api/v1/namespaces/{namespace}/pods"))
                .await?;
            let pods = pod_list["items"].as_array().cloned().unwrap_or_default();

            for pvc in &pvcs {
                if let Err(e) = self
                    .sync_claim(&namespace, pvc, &mut pvs, &classes, default_class.as_deref(), &pods)
                    .await
                {
                    let name = pvc["metadata"]["name"].as_str().unwrap_or("?");
                    debug!("PVC {namespace}/{name}: {e}");
                }
            }
        }

        // Volumes: re-read, because the claim pass just wrote claimRefs.
        let pv_list: Value = self.api.list("/api/v1/persistentvolumes").await?;
        for pv in pv_list["items"].as_array().cloned().unwrap_or_default() {
            if let Err(e) = self.sync_volume(&pv).await {
                let name = pv["metadata"]["name"].as_str().unwrap_or("?");
                debug!("PV {name}: {e}");
            }
        }
        Ok(())
    }

    /// One claim: protect it, resolve its class, bind it, or hand it to a
    /// provisioner.
    async fn sync_claim(
        &self,
        namespace: &str,
        pvc: &Value,
        pvs: &mut Vec<Value>,
        classes: &HashMap<String, Value>,
        default_class: Option<&str>,
        pods: &[Value],
    ) -> anyhow::Result<()> {
        let name = pvc["metadata"]["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("claim has no name"))?
            .to_string();
        let path = format!("/api/v1/namespaces/{namespace}/persistentvolumeclaims/{name}");

        // Deleting: the finalizer comes off once nothing mounts it. This is
        // the whole point of pvc-protection — deleting a claim out from under
        // a running pod is how a filesystem gets pulled away mid-write.
        if !pvc["metadata"]["deletionTimestamp"].is_null() {
            if let Some(user) = pod_using_claim(pods, &name) {
                self.events
                    .event(
                        pvc,
                        "Normal",
                        "VolumeInUse",
                        &format!("Claim is in use by pod {user}; deletion is deferred"),
                    )
                    .await;
                return Ok(());
            }
            self.remove_finalizer(&path, pvc, PVC_PROTECTION).await?;
            return Ok(());
        }

        if !has_finalizer(pvc, PVC_PROTECTION) {
            self.add_finalizer(&path, pvc, PVC_PROTECTION).await?;
        }

        // Resolve the class once, and write it back: a claim that got the
        // default must keep the class it got, or tomorrow's default silently
        // becomes its class.
        let class_name = claim_class(pvc, default_class);
        if pvc["spec"]["storageClassName"].as_str().is_none() {
            if let Some(cn) = &class_name {
                let patch = json!({"spec": {"storageClassName": cn}});
                let _ = self.api.patch(&path, &patch).await;
            }
        }

        // Already bound: keep the statuses honest and stop.
        if let Some(volume_name) = pvc["spec"]["volumeName"].as_str().filter(|v| !v.is_empty()) {
            return self.sync_bound_claim(namespace, &name, pvc, volume_name).await;
        }

        // Unbound. Look for a volume that satisfies it.
        let class = class_name.as_ref().and_then(|c| classes.get(c));
        if let Some(pv) = pick_volume(pvs, pvc, namespace, &name, class_name.as_deref()) {
            let pv_name = pv["metadata"]["name"].as_str().unwrap_or("").to_string();
            self.bind(namespace, &name, pvc, &pv).await?;
            // Keep the in-memory list honest so two claims in the same pass
            // cannot both take the same volume.
            if let Some(slot) = pvs
                .iter_mut()
                .find(|p| p["metadata"]["name"].as_str() == Some(pv_name.as_str()))
            {
                slot["spec"]["claimRef"] = json!({
                    "kind": "PersistentVolumeClaim",
                    "namespace": namespace,
                    "name": name,
                });
                slot["status"]["phase"] = json!("Bound");
            }
            return Ok(());
        }

        // Nothing matched. Either a provisioner owns this claim, or there is
        // nothing that can ever satisfy it and saying so is the useful act.
        match class {
            Some(class) => {
                let provisioner = class["provisioner"].as_str().unwrap_or("");
                if provisioner.is_empty() || provisioner == NO_PROVISIONER {
                    self.events
                        .event(
                            pvc,
                            "Normal",
                            "WaitForFirstConsumer",
                            &format!(
                                "storage class \"{}\" provisions nothing; waiting for an \
                                 administrator to create a matching PersistentVolume",
                                class["metadata"]["name"].as_str().unwrap_or("")
                            ),
                        )
                        .await;
                    return self.set_claim_phase(&path, pvc, "Pending").await;
                }
                // WaitForFirstConsumer: the volume must be created where the
                // pod will run, so nothing happens until the scheduler has
                // picked a node and said so on the claim. Handing it to the
                // provisioner now would place the volume somewhere the pod may
                // never be scheduled — the whole failure mode the mode exists
                // to avoid, and an expensive one when the volume is a clone.
                let wffc = class["volumeBindingMode"].as_str() == Some("WaitForFirstConsumer");
                let selected = pvc["metadata"]["annotations"][ANN_SELECTED_NODE]
                    .as_str()
                    .filter(|s| !s.is_empty());
                if wffc && selected.is_none() {
                    self.events
                        .event(
                            pvc,
                            "Normal",
                            "WaitForFirstConsumer",
                            "waiting for first consumer to be created before binding",
                        )
                        .await;
                    return self.set_claim_phase(&path, pvc, "Pending").await;
                }
                if pvc["metadata"]["annotations"][ANN_STORAGE_PROVISIONER].as_str()
                    != Some(provisioner)
                {
                    let patch = json!({"metadata": {"annotations": {
                        ANN_STORAGE_PROVISIONER: provisioner,
                        ANN_STORAGE_PROVISIONER_BETA: provisioner,
                    }}});
                    self.api.patch(&path, &patch).await?;
                    info!(
                        "PVC {namespace}/{name} handed to external provisioner {provisioner}"
                    );
                }
                self.events
                    .event(
                        pvc,
                        "Normal",
                        "ExternalProvisioning",
                        &format!(
                            "Waiting for a volume to be created either by the external \
                             provisioner '{provisioner}' or manually by the system administrator"
                        ),
                    )
                    .await;
                self.set_claim_phase(&path, pvc, "Pending").await
            }
            None => {
                self.events
                    .event(
                        pvc,
                        "Warning",
                        "FailedBinding",
                        "no persistent volumes available for this claim and no storage class is set",
                    )
                    .await;
                self.set_claim_phase(&path, pvc, "Pending").await
            }
        }
    }

    /// A claim that names a volume: confirm the volume still exists and agrees,
    /// and report Bound (or Lost, which is the one state a claim cannot recover
    /// from on its own).
    async fn sync_bound_claim(
        &self,
        namespace: &str,
        name: &str,
        pvc: &Value,
        volume_name: &str,
    ) -> anyhow::Result<()> {
        let path = format!("/api/v1/namespaces/{namespace}/persistentvolumeclaims/{name}");
        let resp = self
            .api
            .get(&format!("/api/v1/persistentvolumes/{volume_name}"))
            .await?;
        if resp.status().as_u16() == 404 {
            self.events
                .event(
                    pvc,
                    "Warning",
                    "ClaimLost",
                    &format!("Bound claim has lost its PersistentVolume {volume_name}"),
                )
                .await;
            return self.set_claim_phase(&path, pvc, "Lost").await;
        }
        let pv: Value = resp.json().await?;

        // The volume may name a different claim — a stale volumeName on a
        // claim that was rebuilt. That is Lost too, not a silent share.
        let ref_ns = pv["spec"]["claimRef"]["namespace"].as_str().unwrap_or("");
        let ref_name = pv["spec"]["claimRef"]["name"].as_str().unwrap_or("");
        if !ref_name.is_empty() && (ref_ns != namespace || ref_name != name) {
            self.events
                .event(
                    pvc,
                    "Warning",
                    "ClaimMisbound",
                    &format!(
                        "Volume {volume_name} is bound to {ref_ns}/{ref_name}, not to this claim"
                    ),
                )
                .await;
            return self.set_claim_phase(&path, pvc, "Lost").await;
        }

        // The claim's status reports what it actually got, which for a
        // dynamically provisioned volume can exceed what it asked for.
        let capacity = pv["spec"]["capacity"]["storage"].as_str().unwrap_or("");
        let want_phase = "Bound";
        let phase_now = pvc["status"]["phase"].as_str().unwrap_or("");
        let cap_now = pvc["status"]["capacity"]["storage"].as_str().unwrap_or("");
        let modes: Vec<Value> = pv["spec"]["accessModes"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if phase_now == want_phase && cap_now == capacity {
            return Ok(());
        }
        let mut updated = pvc.clone();
        updated["status"] = json!({
            "phase": want_phase,
            "accessModes": modes,
            "capacity": { "storage": capacity },
        });
        self.api.update_status(&path, &updated).await?;
        if phase_now != want_phase {
            info!("PVC {namespace}/{name} bound to {volume_name}");
            self.events
                .event(
                    pvc,
                    "Normal",
                    "Bound",
                    &format!("Claim bound to volume {volume_name}"),
                )
                .await;
        }
        Ok(())
    }

    /// Write the binding: volume first, then claim.
    ///
    /// The order is not arbitrary. If the claimRef lands and the process dies,
    /// the volume is reserved for a claim that will retry and find it —
    /// wasteful at worst. The other order hands a claim a volume that someone
    /// else can still take, and two claims sharing one volume is data loss.
    async fn bind(
        &self,
        namespace: &str,
        name: &str,
        pvc: &Value,
        pv: &Value,
    ) -> anyhow::Result<()> {
        let pv_name = pv["metadata"]["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("volume has no name"))?;
        let uid = pvc["metadata"]["uid"].as_str().unwrap_or("");

        let mut volume = pv.clone();
        volume["spec"]["claimRef"] = json!({
            "kind": "PersistentVolumeClaim",
            "apiVersion": "v1",
            "namespace": namespace,
            "name": name,
            "uid": uid,
        });
        let anns = volume["metadata"]["annotations"]
            .as_object_mut()
            .map(|m| m as &mut serde_json::Map<String, Value>);
        match anns {
            Some(m) => {
                m.insert(ANN_BOUND_BY_CONTROLLER.into(), json!("yes"));
            }
            None => {
                volume["metadata"]["annotations"] =
                    json!({ ANN_BOUND_BY_CONTROLLER: "yes" });
            }
        }
        self.api
            .update(&format!("/api/v1/persistentvolumes/{pv_name}"), &volume)
            .await?;

        let claim_path = format!("/api/v1/namespaces/{namespace}/persistentvolumeclaims/{name}");
        let patch = json!({
            "metadata": {"annotations": {
                ANN_BIND_COMPLETED: "yes",
                ANN_BOUND_BY_CONTROLLER: "yes",
            }},
            "spec": {"volumeName": pv_name},
        });
        self.api.patch(&claim_path, &patch).await?;

        info!("Bound PVC {namespace}/{name} to PV {pv_name}");
        self.events
            .event(
                pvc,
                "Normal",
                "ProvisioningSucceeded",
                &format!("Successfully bound volume {pv_name}"),
            )
            .await;
        Ok(())
    }

    /// One volume: protect it, and keep its phase telling the truth about
    /// whether a claim still holds it.
    async fn sync_volume(&self, pv: &Value) -> anyhow::Result<()> {
        let name = pv["metadata"]["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("volume has no name"))?
            .to_string();
        let path = format!("/api/v1/persistentvolumes/{name}");

        let claim_ref = &pv["spec"]["claimRef"];
        let bound_claim = match (
            claim_ref["namespace"].as_str(),
            claim_ref["name"].as_str(),
        ) {
            (Some(ns), Some(cn)) if !ns.is_empty() && !cn.is_empty() => Some((ns, cn)),
            _ => None,
        };

        // Deleting: hold the volume while a claim is still bound, then let go.
        if !pv["metadata"]["deletionTimestamp"].is_null() {
            let still_bound = match bound_claim {
                Some((ns, cn)) => self.claim_exists(ns, cn).await,
                None => false,
            };
            if still_bound {
                self.events
                    .event(
                        pv,
                        "Normal",
                        "VolumeInUse",
                        "Volume is still bound to a claim; deletion is deferred",
                    )
                    .await;
                return Ok(());
            }
            self.remove_finalizer(&path, pv, PV_PROTECTION).await?;
            return Ok(());
        }

        if !has_finalizer(pv, PV_PROTECTION) {
            self.add_finalizer(&path, pv, PV_PROTECTION).await?;
        }

        let phase_now = pv["status"]["phase"].as_str().unwrap_or("");
        let want = match bound_claim {
            None => "Available",
            Some((ns, cn)) => {
                if self.claim_exists(ns, cn).await {
                    "Bound"
                } else {
                    "Released"
                }
            }
        };

        if want == "Released" && phase_now != "Released" {
            let policy = pv["spec"]["persistentVolumeReclaimPolicy"]
                .as_str()
                .unwrap_or("Retain");
            let provisioned_by = pv["metadata"]["annotations"][ANN_PROVISIONED_BY]
                .as_str()
                .unwrap_or("");
            match policy {
                // The driver that made it deletes it — it holds its own
                // finalizer and watches for exactly this. Deleting the API
                // object here would strand the volume on the array.
                "Delete" if !provisioned_by.is_empty() => {
                    self.events
                        .event(
                            pv,
                            "Normal",
                            "VolumeDelete",
                            &format!(
                                "Claim released; deletion is up to the provisioner \
                                 '{provisioned_by}'"
                            ),
                        )
                        .await;
                }
                // Nothing here can erase a volume nobody claims to own: there
                // is no in-tree deleter, and saying so is better than a phase
                // that implies something is happening.
                "Delete" => {
                    self.events
                        .event(
                            pv,
                            "Warning",
                            "VolumeFailedDelete",
                            "reclaim policy is Delete but no provisioner is recorded on this \
                             volume; it will stay Released until deleted by hand",
                        )
                        .await;
                }
                "Recycle" => {
                    self.events
                        .event(
                            pv,
                            "Warning",
                            "VolumeFailedRecycle",
                            "the Recycle reclaim policy is removed upstream; treating as Retain",
                        )
                        .await;
                }
                _ => {}
            }
        }

        if phase_now != want {
            let mut updated = pv.clone();
            updated["status"] = json!({ "phase": want });
            self.api.update_status(&path, &updated).await?;
            info!("PV {name} -> {want}");
        }
        Ok(())
    }

    async fn claim_exists(&self, namespace: &str, name: &str) -> bool {
        match self
            .api
            .get(&format!(
                "/api/v1/namespaces/{namespace}/persistentvolumeclaims/{name}"
            ))
            .await
        {
            Ok(r) => r.status().is_success(),
            // A failed GET is not evidence of absence: treating an unreachable
            // apiserver as "the claim is gone" would release bound volumes
            // across the cluster on a blip.
            Err(_) => true,
        }
    }

    async fn set_claim_phase(&self, path: &str, pvc: &Value, phase: &str) -> anyhow::Result<()> {
        if pvc["status"]["phase"].as_str() == Some(phase) {
            return Ok(());
        }
        let mut updated = pvc.clone();
        updated["status"]["phase"] = json!(phase);
        self.api.update_status(path, &updated).await?;
        Ok(())
    }

    async fn add_finalizer(&self, path: &str, obj: &Value, finalizer: &str) -> anyhow::Result<()> {
        let mut finalizers: Vec<Value> = obj["metadata"]["finalizers"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        finalizers.push(json!(finalizer));
        let patch = json!({"metadata": {"finalizers": finalizers}});
        self.api.patch(path, &patch).await?;
        Ok(())
    }

    async fn remove_finalizer(
        &self,
        path: &str,
        obj: &Value,
        finalizer: &str,
    ) -> anyhow::Result<()> {
        if !has_finalizer(obj, finalizer) {
            return Ok(());
        }
        let finalizers: Vec<Value> = obj["metadata"]["finalizers"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|f| f.as_str() != Some(finalizer))
            .collect();
        // A merge patch cannot delete a list entry, so the whole list is sent.
        let patch = json!({"metadata": {"finalizers": finalizers}});
        self.api.patch(path, &patch).await?;
        Ok(())
    }
}

/// The name of the default StorageClass, if exactly one is marked.
///
/// Two defaults is a misconfiguration upstream resolves by taking the newest;
/// doing the same keeps a cluster working rather than leaving every claim
/// without a class while someone finds the second annotation.
pub fn default_class_name(classes: &HashMap<String, Value>) -> Option<String> {
    let mut defaults: Vec<&Value> = classes
        .values()
        .filter(|c| {
            let a = &c["metadata"]["annotations"];
            a[ANN_IS_DEFAULT_CLASS].as_str() == Some("true")
                || a[ANN_IS_DEFAULT_CLASS_BETA].as_str() == Some("true")
        })
        .collect();
    defaults.sort_by_key(|c| {
        c["metadata"]["creationTimestamp"]
            .as_str()
            .unwrap_or("")
            .to_string()
    });
    defaults
        .last()
        .and_then(|c| c["metadata"]["name"].as_str())
        .map(str::to_string)
}

/// Which class a claim belongs to.
///
/// `storageClassName: ""` is not "no opinion" — it explicitly opts out of
/// dynamic provisioning and asks for a static PV with no class. Only an absent
/// field takes the default.
pub fn claim_class(pvc: &Value, default_class: Option<&str>) -> Option<String> {
    match pvc["spec"]["storageClassName"].as_str() {
        Some("") => None,
        Some(c) => Some(c.to_string()),
        None => match pvc["metadata"]["annotations"]["volume.beta.kubernetes.io/storage-class"]
            .as_str()
        {
            Some("") => None,
            Some(c) => Some(c.to_string()),
            None => default_class.map(str::to_string),
        },
    }
}

fn has_finalizer(obj: &Value, finalizer: &str) -> bool {
    obj["metadata"]["finalizers"]
        .as_array()
        .map(|fs| fs.iter().any(|f| f.as_str() == Some(finalizer)))
        .unwrap_or(false)
}

/// The name of a pod that mounts this claim, if any.
pub fn pod_using_claim(pods: &[Value], claim: &str) -> Option<String> {
    pods.iter()
        .filter(|p| {
            // A finished pod holds nothing open.
            !matches!(
                p["status"]["phase"].as_str().unwrap_or(""),
                "Succeeded" | "Failed"
            )
        })
        .find(|p| {
            p["spec"]["volumes"]
                .as_array()
                .map(|vs| {
                    vs.iter().any(|v| {
                        v["persistentVolumeClaim"]["claimName"].as_str() == Some(claim)
                    })
                })
                .unwrap_or(false)
        })
        .and_then(|p| p["metadata"]["name"].as_str())
        .map(str::to_string)
}

/// Does this volume satisfy this claim?
pub fn volume_satisfies(
    pv: &Value,
    pvc: &Value,
    namespace: &str,
    claim_name: &str,
    class: Option<&str>,
) -> bool {
    // Being deleted, or already spoken for by someone else.
    if !pv["metadata"]["deletionTimestamp"].is_null() {
        return false;
    }
    let ref_ns = pv["spec"]["claimRef"]["namespace"].as_str().unwrap_or("");
    let ref_name = pv["spec"]["claimRef"]["name"].as_str().unwrap_or("");
    let prebound_to_us = ref_ns == namespace && ref_name == claim_name;
    if !ref_name.is_empty() && !prebound_to_us {
        return false;
    }
    let phase = pv["status"]["phase"].as_str().unwrap_or("");
    if !prebound_to_us && !matches!(phase, "" | "Pending" | "Available") {
        return false;
    }

    // Class must match exactly, including "both have none".
    let pv_class = pv["spec"]["storageClassName"].as_str().unwrap_or("");
    if pv_class != class.unwrap_or("") {
        return false;
    }

    // Filesystem vs Block is not a preference.
    let want_mode = pvc["spec"]["volumeMode"].as_str().unwrap_or("Filesystem");
    let have_mode = pv["spec"]["volumeMode"].as_str().unwrap_or("Filesystem");
    if want_mode != have_mode {
        return false;
    }

    // Every mode the claim asks for must be offered.
    let want_modes: Vec<&str> = pvc["spec"]["accessModes"]
        .as_array()
        .map(|m| m.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let have_modes: Vec<&str> = pv["spec"]["accessModes"]
        .as_array()
        .map(|m| m.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if !want_modes.iter().all(|m| have_modes.contains(m)) {
        return false;
    }

    // Capacity: at least what was asked for.
    let want = parse_bytes(
        pvc["spec"]["resources"]["requests"]["storage"]
            .as_str()
            .unwrap_or("0"),
    );
    let have = parse_bytes(pv["spec"]["capacity"]["storage"].as_str().unwrap_or("0"));
    if have < want {
        return false;
    }

    // A claim may also select on the volume's labels.
    let selector = &pvc["spec"]["selector"];
    if !selector.is_null()
        && !apimachinery::selector::matches(selector, &pv["metadata"]["labels"])
    {
        return false;
    }
    true
}

/// Pick the volume that fits with the least waste.
///
/// Smallest sufficient capacity first: handing a 1Ti volume to a claim asking
/// for 1Gi is a bind that works and a cluster that runs out of big volumes.
/// A volume pre-bound to this exact claim wins outright — an administrator
/// said so.
pub fn pick_volume<'a>(
    pvs: &'a [Value],
    pvc: &Value,
    namespace: &str,
    claim_name: &str,
    class: Option<&str>,
) -> Option<&'a Value> {
    let mut best: Option<(&Value, u64, bool)> = None;
    for pv in pvs {
        if !volume_satisfies(pv, pvc, namespace, claim_name, class) {
            continue;
        }
        let prebound = pv["spec"]["claimRef"]["name"].as_str() == Some(claim_name)
            && pv["spec"]["claimRef"]["namespace"].as_str() == Some(namespace);
        let size = parse_bytes(pv["spec"]["capacity"]["storage"].as_str().unwrap_or("0"));
        let better = match best {
            None => true,
            Some((_, best_size, best_prebound)) => match (prebound, best_prebound) {
                (true, false) => true,
                (false, true) => false,
                _ => size < best_size,
            },
        };
        if better {
            best = Some((pv, size, prebound));
        }
    }
    best.map(|(pv, _, _)| pv)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn claim(size: &str, class: Option<&str>, modes: &[&str]) -> Value {
        let mut spec = json!({
            "resources": {"requests": {"storage": size}},
            "accessModes": modes,
        });
        if let Some(c) = class {
            spec["storageClassName"] = json!(c);
        }
        json!({"metadata": {"name": "c1", "namespace": "default", "uid": "u1"}, "spec": spec})
    }

    fn volume(name: &str, size: &str, class: &str, modes: &[&str]) -> Value {
        json!({
            "metadata": {"name": name},
            "spec": {
                "capacity": {"storage": size},
                "accessModes": modes,
                "storageClassName": class,
            },
            "status": {"phase": "Available"}
        })
    }

    #[test]
    fn picks_the_smallest_volume_that_fits() {
        let pvs = vec![
            volume("big", "100Gi", "", &["ReadWriteOnce"]),
            volume("right", "10Gi", "", &["ReadWriteOnce"]),
            volume("small", "1Gi", "", &["ReadWriteOnce"]),
        ];
        let pvc = claim("5Gi", Some(""), &["ReadWriteOnce"]);
        let chosen = pick_volume(&pvs, &pvc, "default", "c1", None).unwrap();
        assert_eq!(chosen["metadata"]["name"], "right");
    }

    #[test]
    fn a_volume_of_another_class_never_matches() {
        let pvs = vec![volume("v", "10Gi", "fast", &["ReadWriteOnce"])];
        let pvc = claim("1Gi", Some("slow"), &["ReadWriteOnce"]);
        assert!(pick_volume(&pvs, &pvc, "default", "c1", Some("slow")).is_none());
        assert!(pick_volume(&pvs, &pvc, "default", "c1", Some("fast")).is_some());
    }

    #[test]
    fn access_modes_must_all_be_offered() {
        let pvs = vec![volume("v", "10Gi", "", &["ReadWriteOnce"])];
        let pvc = claim("1Gi", Some(""), &["ReadWriteMany"]);
        assert!(pick_volume(&pvs, &pvc, "default", "c1", None).is_none());
    }

    #[test]
    fn a_volume_bound_to_another_claim_is_not_available() {
        let mut pv = volume("v", "10Gi", "", &["ReadWriteOnce"]);
        pv["spec"]["claimRef"] = json!({"namespace": "other", "name": "theirs"});
        let pvc = claim("1Gi", Some(""), &["ReadWriteOnce"]);
        assert!(pick_volume(&[pv], &pvc, "default", "c1", None).is_none());
    }

    #[test]
    fn a_prebound_volume_wins_even_when_larger() {
        let mut mine = volume("mine", "100Gi", "", &["ReadWriteOnce"]);
        mine["spec"]["claimRef"] = json!({"namespace": "default", "name": "c1"});
        mine["status"]["phase"] = json!("Available");
        let pvs = vec![volume("tight", "1Gi", "", &["ReadWriteOnce"]), mine];
        let pvc = claim("1Gi", Some(""), &["ReadWriteOnce"]);
        let chosen = pick_volume(&pvs, &pvc, "default", "c1", None).unwrap();
        assert_eq!(chosen["metadata"]["name"], "mine");
    }

    #[test]
    fn block_and_filesystem_do_not_mix() {
        let mut pv = volume("v", "10Gi", "", &["ReadWriteOnce"]);
        pv["spec"]["volumeMode"] = json!("Block");
        let pvc = claim("1Gi", Some(""), &["ReadWriteOnce"]);
        assert!(pick_volume(&[pv], &pvc, "default", "c1", None).is_none());
    }

    #[test]
    fn a_selector_narrows_the_candidates() {
        let mut pv = volume("v", "10Gi", "", &["ReadWriteOnce"]);
        pv["metadata"]["labels"] = json!({"tier": "nvme"});
        let mut pvc = claim("1Gi", Some(""), &["ReadWriteOnce"]);
        pvc["spec"]["selector"] = json!({"matchLabels": {"tier": "spinning"}});
        assert!(pick_volume(std::slice::from_ref(&pv), &pvc, "default", "c1", None).is_none());
        pvc["spec"]["selector"] = json!({"matchLabels": {"tier": "nvme"}});
        assert!(pick_volume(&[pv], &pvc, "default", "c1", None).is_some());
    }

    #[test]
    fn empty_class_opts_out_of_the_default_but_absent_takes_it() {
        let explicit = claim("1Gi", Some(""), &["ReadWriteOnce"]);
        assert_eq!(claim_class(&explicit, Some("stormblock")), None);
        let absent = claim("1Gi", None, &["ReadWriteOnce"]);
        assert_eq!(
            claim_class(&absent, Some("stormblock")).as_deref(),
            Some("stormblock")
        );
        let named = claim("1Gi", Some("fast"), &["ReadWriteOnce"]);
        assert_eq!(claim_class(&named, Some("stormblock")).as_deref(), Some("fast"));
    }

    #[test]
    fn the_newest_of_two_defaults_wins() {
        let mut classes = HashMap::new();
        classes.insert(
            "old".to_string(),
            json!({"metadata": {"name": "old", "creationTimestamp": "2026-01-01T00:00:00Z",
                   "annotations": {"storageclass.kubernetes.io/is-default-class": "true"}}}),
        );
        classes.insert(
            "new".to_string(),
            json!({"metadata": {"name": "new", "creationTimestamp": "2026-06-01T00:00:00Z",
                   "annotations": {"storageclass.kubernetes.io/is-default-class": "true"}}}),
        );
        assert_eq!(default_class_name(&classes).as_deref(), Some("new"));
    }

    #[test]
    fn a_running_pod_holds_its_claim_a_finished_one_does_not() {
        let running = json!({
            "metadata": {"name": "user"}, "status": {"phase": "Running"},
            "spec": {"volumes": [{"persistentVolumeClaim": {"claimName": "c1"}}]}
        });
        let done = json!({
            "metadata": {"name": "done"}, "status": {"phase": "Succeeded"},
            "spec": {"volumes": [{"persistentVolumeClaim": {"claimName": "c1"}}]}
        });
        assert_eq!(pod_using_claim(&[running.clone()], "c1").as_deref(), Some("user"));
        assert_eq!(pod_using_claim(&[done], "c1"), None);
        assert_eq!(pod_using_claim(&[running], "other"), None);
    }
}
