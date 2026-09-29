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
use tracing::{debug, info};
use crate::owned::{self, Controller, Dependency, Deps};
use apimachinery::informer::{Index, Key};

pub struct AttachDetachController {
    api: Arc<ApiClient>,
}

impl AttachDetachController {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self { api }
    }

    pub async fn run(&self) {
        owned::run(&self.api, self).await;
    }

    async fn reconcile_volume(&self, pv: &Value, deps: &Deps) -> anyhow::Result<()> {
        let pv_name = pv["metadata"]["name"].as_str().unwrap_or("");
        let claims = deps.feed(0).select(&Index::Volume(pv_name.into()))?;
        let Some((driver, handle)) = csi_source(pv) else { return Ok(()); };
        let drivers = deps.feed(2).select(&Index::Name("".into(),driver.clone()))?;
        let required = drivers.first().map(|d| d["spec"]["attachRequired"].as_bool().unwrap_or(true)).unwrap_or(true);
        let mut desired: HashMap<String, Desired> = HashMap::new();
        for pvc in claims {
            let ns = pvc["metadata"]["namespace"].as_str().unwrap_or("");
            let name = pvc["metadata"]["name"].as_str().unwrap_or("");
            for pod in deps.feed(1).select(&Index::Claim(ns.into(),name.into()))? {
                if !required || matches!(pod["status"]["phase"].as_str(),Some("Succeeded" | "Failed")) { continue; }
                let Some(node) = pod["spec"]["nodeName"].as_str().filter(|n| !n.is_empty()) else { continue; };
                desired.insert(attachment_name(&handle,&driver,node), Desired {
                    driver: driver.clone(), node: node.into(), pv: pv_name.into(),
                });
            }
        }
        let existing_list = deps.feed(3).select(&Index::Volume(pv_name.into()))?;
        let mut existing: HashSet<String> = HashSet::new();
        for va in existing_list {
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
            if drivers.is_empty() || attacher != driver {
                continue;
            }
            let node = va["spec"]["nodeName"].as_str().unwrap_or("");
            match self
                .api
                .delete_observed(&format!("/apis/storage.k8s.io/v1/volumeattachments/{name}"), &va)
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

#[async_trait::async_trait]
impl Controller for AttachDetachController {
    fn name(&self) -> &'static str { "attachdetach" }
    fn primary(&self) -> &'static str { "/api/v1/persistentvolumes" }
    fn dependencies(&self) -> Vec<Dependency> { vec![
        Dependency { path: "/api/v1/persistentvolumeclaims".into(), route: Arc::new(|delta, primary| {
            delta.affected.iter().flat_map(|i| match i {
                Index::Volume(name) => owned::keys_at(primary, Index::Name("".into(),name.clone())),
                _ => Vec::new(),
            }).collect()
        }) },
        Dependency { path: "/api/v1/pods".into(), route: Arc::new(|delta, primary| {
            delta.affected.iter().flat_map(|i| match i {
                Index::Claim(..) => owned::keys_at(primary, i.clone()), _ => Vec::new(),
            }).collect()
        }) },
        Dependency { path: "/apis/storage.k8s.io/v1/csidrivers".into(), route: Arc::new(|delta, primary| {
            delta.old.iter().chain(delta.new.iter()).flat_map(|driver|
                owned::keys_at(primary, Index::Driver(driver["metadata"]["name"].as_str().unwrap_or("").into()))).collect()
        }) },
        Dependency { path: "/apis/storage.k8s.io/v1/volumeattachments".into(), route: Arc::new(|delta, primary| {
            delta.old.iter().chain(delta.new.iter()).flat_map(|va| {
                let name = va["spec"]["source"]["persistentVolumeName"].as_str().unwrap_or("");
                let mut keys = owned::keys_at(primary, Index::Name("".into(),name.into()));
                if keys.is_empty() { keys.push(Key { namespace: "".into(), name: name.into(), uid: format!("missing:{}",va["metadata"]["uid"].as_str().unwrap_or("")) }); }
                keys
            }).collect()
        }) },
    ] }
    async fn reconcile(&self, pv: &Value, _: &[Value], deps: &Deps) -> anyhow::Result<()> { self.reconcile_volume(pv,deps).await }
    async fn deleted(&self, key: &Key, _: &[Value], deps: &Deps) -> anyhow::Result<()> {
        let response = self.api.get(&format!("/api/v1/persistentvolumes/{}",key.name)).await?;
        if response.status().is_success() { return Ok(()); }
        anyhow::ensure!(response.status().as_u16() == 404,"PV absence not established");
        // A missing PV cannot prove a running Pod has released the volume.
        for pvc in deps.feed(0).select(&Index::Volume(key.name.clone()))? {
            let ns = pvc["metadata"]["namespace"].as_str().unwrap_or("");
            let name = pvc["metadata"]["name"].as_str().unwrap_or("");
            if deps.feed(1).select(&Index::Claim(ns.into(),name.into()))?.iter().any(|p|
                !matches!(p["status"]["phase"].as_str(),Some("Succeeded" | "Failed"))) { return Ok(()); }
        }
        for va in deps.feed(3).select(&Index::Volume(key.name.clone()))? {
            let driver = va["spec"]["attacher"].as_str().unwrap_or("");
            if deps.feed(2).select(&Index::Name("".into(),driver.into()))?.is_empty() { continue; }
            let name = va["metadata"]["name"].as_str().unwrap_or("");
            self.api.delete_observed(&format!("/apis/storage.k8s.io/v1/volumeattachments/{name}"),&va).await?;
        }
        Ok(())
    }
}
