//! Attach/detach — `VolumeAttachment` objects for CSI volumes.
//!
//! The third piece of making a PVC actually reach a pod, after binding
//! (`persistentvolume.rs`) and placement (`scheduler::volumebinding`). A CSI
//! driver that declares `attachRequired: true` — `csi.stormblock.io` does —
//! is told to attach a volume to a node by the *existence of a
//! `VolumeAttachment` object*, which the external-attacher sidecar watches
//! for and turns into `ControllerPublishVolume`. Nothing else creates that
//! object: it is the in-tree attach/detach controller's job, which is this
//! file. Without it the claim binds, the pod is scheduled, and the mount then
//! waits on an attach that was never asked for.
//!
//! Detach is the deletion of the same object. The sidecar holds a finalizer,
//! so deleting it here starts `ControllerUnpublishVolume` and the object goes
//! when the driver says the volume is off the node — which is why the detach
//! path must not be clever: delete, and let the driver finish.

use crate::runner::ApiClient;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::time::{self, Duration};
use tracing::{debug, error, info};

pub struct AttachDetachController {
    api: Arc<ApiClient>,
}

impl AttachDetachController {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self { api }
    }

    pub async fn run(&self) {
        info!("AttachDetach controller started");
        let mut interval = time::interval(Duration::from_secs(5));
        loop {
            interval.tick().await;
            if let Err(e) = self.reconcile().await {
                error!("AttachDetach reconcile error: {e}");
            }
        }
    }

    async fn reconcile(&self) -> anyhow::Result<()> {
        // Which drivers want an attach at all. A driver that says
        // `attachRequired: false` is told nothing — creating a
        // VolumeAttachment for it leaves an object nobody ever removes.
        let drivers: Value = self.api.list("/apis/storage.k8s.io/v1/csidrivers").await?;
        let mut attach_required: HashMap<String, bool> = HashMap::new();
        for d in drivers["items"].as_array().cloned().unwrap_or_default() {
            if let Some(name) = d["metadata"]["name"].as_str() {
                // Absent means true, per the CSIDriver defaulting rules.
                let required = d["spec"]["attachRequired"].as_bool().unwrap_or(true);
                attach_required.insert(name.to_string(), required);
            }
        }

        let pv_list: Value = self.api.list("/api/v1/persistentvolumes").await?;
        let pvs: HashMap<String, Value> = pv_list["items"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|pv| {
                pv["metadata"]["name"]
                    .as_str()
                    .map(|n| (n.to_string(), pv.clone()))
            })
            .collect();
        if pvs.is_empty() {
            return Ok(());
        }

        // What ought to be attached: every (volume, node) a live pod implies.
        let mut desired: HashMap<String, Desired> = HashMap::new();
        let ns_list: Value = self.api.list("/api/v1/namespaces").await?;
        for ns in ns_list["items"].as_array().cloned().unwrap_or_default() {
            let namespace = match ns["metadata"]["name"].as_str() {
                Some(n) => n.to_string(),
                None => continue,
            };
            let pod_list: Value = self
                .api
                .list(&format!("/api/v1/namespaces/{namespace}/pods"))
                .await?;
            let pods = pod_list["items"].as_array().cloned().unwrap_or_default();
            if pods.is_empty() {
                continue;
            }
            let pvc_list: Value = self
                .api
                .list(&format!(
                    "/api/v1/namespaces/{namespace}/persistentvolumeclaims"
                ))
                .await?;
            let pvcs: HashMap<String, Value> = pvc_list["items"]
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

            for pod in &pods {
                let node = match pod["spec"]["nodeName"].as_str().filter(|n| !n.is_empty()) {
                    Some(n) => n,
                    None => continue, // not placed yet: nothing to attach to
                };
                // A finished pod releases its volumes. A terminating one has
                // not: its containers may still be writing.
                if matches!(
                    pod["status"]["phase"].as_str().unwrap_or(""),
                    "Succeeded" | "Failed"
                ) {
                    continue;
                }
                for claim_name in claim_names(pod) {
                    let pvc = match pvcs.get(&claim_name) {
                        Some(c) => c,
                        None => continue,
                    };
                    let pv_name = match pvc["spec"]["volumeName"].as_str().filter(|v| !v.is_empty())
                    {
                        Some(v) => v,
                        None => continue, // not bound yet
                    };
                    let pv = match pvs.get(pv_name) {
                        Some(pv) => pv,
                        None => continue,
                    };
                    let (driver, handle) = match csi_source(pv) {
                        Some(s) => s,
                        None => continue, // not a CSI volume: nothing to attach
                    };
                    if !attach_required.get(&driver).copied().unwrap_or(true) {
                        continue;
                    }
                    let name = attachment_name(&handle, &driver, node);
                    desired.insert(
                        name,
                        Desired {
                            driver,
                            node: node.to_string(),
                            pv: pv_name.to_string(),
                        },
                    );
                }
            }
        }

        // What is attached now.
        let existing_list: Value = self
            .api
            .list("/apis/storage.k8s.io/v1/volumeattachments")
            .await?;
        let mut existing: HashSet<String> = HashSet::new();
        for va in existing_list["items"].as_array().cloned().unwrap_or_default() {
            let name = match va["metadata"]["name"].as_str() {
                Some(n) => n.to_string(),
                None => continue,
            };
            existing.insert(name.clone());
            if desired.contains_key(&name) {
                continue;
            }
            // Already on its way out; deleting twice achieves nothing.
            if !va["metadata"]["deletionTimestamp"].is_null() {
                continue;
            }
            let attacher = va["spec"]["attacher"].as_str().unwrap_or("");
            // Only CSI attachments this controller could have created. An
            // attachment for a driver we have never heard of belongs to
            // whoever made it.
            if !attach_required.contains_key(attacher) {
                continue;
            }
            let node = va["spec"]["nodeName"].as_str().unwrap_or("");
            match self
                .api
                .delete(&format!("/apis/storage.k8s.io/v1/volumeattachments/{name}"))
                .await
            {
                Ok(_) => info!("Detaching {name} ({attacher} on {node})"),
                Err(e) => debug!("could not delete VolumeAttachment {name}: {e}"),
            }
        }

        for (name, want) in desired {
            if existing.contains(&name) {
                continue;
            }
            let body = json!({
                "apiVersion": "storage.k8s.io/v1",
                "kind": "VolumeAttachment",
                "metadata": { "name": name },
                "spec": {
                    "attacher": want.driver,
                    "nodeName": want.node,
                    "source": { "persistentVolumeName": want.pv },
                }
            });
            match self
                .api
                .create("/apis/storage.k8s.io/v1/volumeattachments", &body)
                .await
            {
                Ok(_) => info!(
                    "Attaching {} to {} via {}",
                    want.pv, want.node, want.driver
                ),
                Err(e) => debug!("could not create VolumeAttachment {name}: {e}"),
            }
        }
        Ok(())
    }
}

struct Desired {
    driver: String,
    node: String,
    pv: String,
}

/// The claims a pod mounts (generic ephemeral volumes included).
fn claim_names(pod: &Value) -> Vec<String> {
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
                        return Some(format!("{pod_name}-{}", v["name"].as_str().unwrap_or("")));
                    }
                    None
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `(driver, volumeHandle)` if this PV is a CSI volume.
fn csi_source(pv: &Value) -> Option<(String, String)> {
    let driver = pv["spec"]["csi"]["driver"].as_str()?;
    let handle = pv["spec"]["csi"]["volumeHandle"].as_str().unwrap_or("");
    Some((driver.to_string(), handle.to_string()))
}

/// The upstream attachment name: `csi-` + SHA-256 of handle+driver+node.
///
/// Deterministic on purpose — the same volume on the same node is the same
/// object, so a controller restart does not attach a second time — and the
/// exact upstream formula, so an upstream kubelet waiting on a name finds the
/// object this wrote.
pub fn attachment_name(volume_handle: &str, driver: &str, node: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(volume_handle.as_bytes());
    hasher.update(driver.as_bytes());
    hasher.update(node.as_bytes());
    format!("csi-{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_name_is_stable_and_specific() {
        let a = attachment_name("vol-1", "csi.stormblock.io", "node-a");
        assert_eq!(a, attachment_name("vol-1", "csi.stormblock.io", "node-a"));
        assert_ne!(a, attachment_name("vol-1", "csi.stormblock.io", "node-b"));
        assert_ne!(a, attachment_name("vol-2", "csi.stormblock.io", "node-a"));
        assert!(a.starts_with("csi-") && a.len() == 68);
    }

    #[test]
    fn matches_the_upstream_formula() {
        // sha256("volhandlecsi.example.comnode1"), as kubernetes'
        // getAttachmentName computes it.
        let expected = {
            let mut h = Sha256::new();
            h.update(b"volhandlecsi.example.comnode1");
            format!("csi-{:x}", h.finalize())
        };
        assert_eq!(attachment_name("volhandle", "csi.example.com", "node1"), expected);
    }

    #[test]
    fn only_csi_volumes_attach() {
        let csi = json!({"spec": {"csi": {"driver": "csi.stormblock.io", "volumeHandle": "h"}}});
        assert_eq!(
            csi_source(&csi),
            Some(("csi.stormblock.io".into(), "h".into()))
        );
        let hostpath = json!({"spec": {"hostPath": {"path": "/data"}}});
        assert_eq!(csi_source(&hostpath), None);
    }

    #[test]
    fn claims_include_ephemeral_ones() {
        let pod = json!({"metadata": {"name": "web"}, "spec": {"volumes": [
            {"name": "data", "persistentVolumeClaim": {"claimName": "c1"}},
            {"name": "scratch", "ephemeral": {}},
            {"name": "cfg", "configMap": {"name": "x"}}
        ]}});
        assert_eq!(claim_names(&pod), vec!["c1".to_string(), "web-scratch".to_string()]);
    }
}
