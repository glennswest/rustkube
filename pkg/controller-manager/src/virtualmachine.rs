//! VirtualMachine controller — the declarative half of a VM.
//!
//! A `VirtualMachineInstance` is a *running* machine; a `VirtualMachine` is the
//! object that says one should exist. Everything a person does to a VM goes
//! through the second: `virtctl start` and `virtctl stop` do not start or stop
//! anything themselves, they set `spec.running` and leave a controller to
//! reconcile. Without that controller the field is decoration — stormpump's
//! manifest applies both CRDs and its own comment says an instance is applied
//! directly *until a controller exists* (#62).
//!
//! What this reconciles is deliberately narrow: one VMI per VM, named after
//! it, owned by it.
//!
//! - `spec.running: true` (or `runStrategy: Always`/`RerunOnFailure`) → the VMI
//!   exists
//! - `spec.running: false` (or `runStrategy: Halted`) → it does not
//! - `spec.template` is the VMI's spec, the way a Deployment's `template` is a
//!   pod's
//!
//! The owner reference is what makes deleting the VM take the machine with it.
//! Without it a deleted VirtualMachine leaves its guest running on a node with
//! nothing in the API pointing at it, which is the failure this whole object
//! exists to prevent.

use crate::runner::ApiClient;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::time::{self, Duration};
use tracing::{debug, error, info};

pub struct VirtualMachineController {
    api: Arc<ApiClient>,
    recorder: crate::events::EventRecorder,
}

impl VirtualMachineController {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self {
            recorder: crate::events::EventRecorder::new(api.clone(), "virtualmachine-controller"),
            api,
        }
    }

    pub async fn run(&self) {
        info!("VirtualMachine controller started");
        let mut interval = time::interval(Duration::from_secs(2));
        loop {
            interval.tick().await;
            if let Err(e) = self.reconcile_all().await {
                error!("VirtualMachine reconcile error: {e}");
            }
        }
    }

    async fn reconcile_all(&self) -> anyhow::Result<()> {
        let ns_list: Value = self.api.list("/api/v1/namespaces").await?;
        for ns in ns_list["items"].as_array().cloned().unwrap_or_default() {
            let name = ns["metadata"]["name"].as_str().unwrap_or("default");
            if let Err(e) = self.reconcile_namespace(name).await {
                // Debug, not error: a cluster with no kubevirt.io CRDs applied
                // answers 404 here on every tick, and that is not a fault.
                debug!("VirtualMachine reconcile in {name}: {e}");
            }
        }
        Ok(())
    }

    async fn reconcile_namespace(&self, namespace: &str) -> anyhow::Result<()> {
        let vms: Value = self
            .api
            .list(&format!(
                "/apis/kubevirt.io/v1/namespaces/{namespace}/virtualmachines"
            ))
            .await?;
        let vms = vms["items"].as_array().cloned().unwrap_or_default();
        if vms.is_empty() {
            return Ok(());
        }

        let vmis: Value = self
            .api
            .list(&format!(
                "/apis/kubevirt.io/v1/namespaces/{namespace}/virtualmachineinstances"
            ))
            .await?;
        let vmis = vmis["items"].as_array().cloned().unwrap_or_default();

        for vm in &vms {
            let name = vm["metadata"]["name"].as_str().unwrap_or("");
            let uid = vm["metadata"]["uid"].as_str().unwrap_or("");
            if name.is_empty() {
                continue;
            }
            // The VMI this VM owns: named after it *and* owned by it. Name
            // alone would adopt a hand-applied VMI that happens to share the
            // name, and then delete it when the VM stops.
            let owned = vmis.iter().find(|i| {
                i["metadata"]["name"].as_str() == Some(name) && owned_by(i, uid)
            });

            let want = apimachinery::kubevirt::wants_running(vm);
            match (want, owned) {
                (true, None) => self.create_vmi(namespace, vm).await,
                (false, Some(vmi)) => self.delete_vmi(namespace, name, vm, vmi).await,
                _ => {}
            }
            if let Err(e) = self.write_status(namespace, name, vm, want, owned).await {
                debug!("VirtualMachine {namespace}/{name} status: {e}");
            }
        }
        Ok(())
    }

    async fn create_vmi(&self, namespace: &str, vm: &Value) {
        let name = vm["metadata"]["name"].as_str().unwrap_or("");
        let uid = vm["metadata"]["uid"].as_str().unwrap_or("");
        let template = &vm["spec"]["template"];
        let vmi = json!({
            "apiVersion": "kubevirt.io/v1",
            "kind": "VirtualMachineInstance",
            "metadata": {
                "name": name,
                "namespace": namespace,
                "labels": template["metadata"]["labels"],
                "annotations": template["metadata"]["annotations"],
                "ownerReferences": [{
                    "apiVersion": "kubevirt.io/v1",
                    "kind": "VirtualMachine",
                    "name": name,
                    "uid": uid,
                    "controller": true,
                    "blockOwnerDeletion": true
                }]
            },
            "spec": template["spec"],
        });
        match self
            .api
            .create(
                &format!("/apis/kubevirt.io/v1/namespaces/{namespace}/virtualmachineinstances"),
                &vmi,
            )
            .await
        {
            Ok(_) => {
                info!("VirtualMachine {namespace}/{name}: started");
                self.recorder
                    .event(
                        vm,
                        "Normal",
                        "SuccessfulCreate",
                        &format!("Created VirtualMachineInstance {name}"),
                    )
                    .await;
            }
            Err(e) => {
                // A VMI that already exists under a *different* owner is the
                // interesting case, and it is left alone rather than adopted:
                // taking over something someone applied by hand would delete
                // their machine the first time the VM was stopped.
                debug!("VirtualMachine {namespace}/{name}: create VMI: {e}");
            }
        }
    }

    async fn delete_vmi(&self, namespace: &str, name: &str, vm: &Value, vmi: &Value) {
        // Already going: a second DELETE each tick would spam the log and the
        // event stream for the whole of a guest's shutdown grace.
        if !vmi["metadata"]["deletionTimestamp"].is_null() {
            return;
        }
        let path =
            format!("/apis/kubevirt.io/v1/namespaces/{namespace}/virtualmachineinstances/{name}");
        match self.api.delete(&path).await {
            Ok(_) => {
                info!("VirtualMachine {namespace}/{name}: stopped");
                self.recorder
                    .event(
                        vm,
                        "Normal",
                        "SuccessfulDelete",
                        &format!("Deleted VirtualMachineInstance {name}"),
                    )
                    .await;
            }
            Err(e) => debug!("VirtualMachine {namespace}/{name}: delete VMI: {e}"),
        }
    }

    async fn write_status(
        &self,
        namespace: &str,
        name: &str,
        vm: &Value,
        want: bool,
        vmi: Option<&Value>,
    ) -> anyhow::Result<()> {
        let status = json!({
            "created": vmi.is_some(),
            "ready": vmi.map(|i| is_ready(i)).unwrap_or(false),
            "printableStatus": printable_status(want, vmi),
        });
        // Only when it changed. A status write per VM per two seconds is a
        // resourceVersion bump per VM per two seconds, which every informer in
        // the cluster then has to look at.
        if vm["status"]["created"] == status["created"]
            && vm["status"]["ready"] == status["ready"]
            && vm["status"]["printableStatus"] == status["printableStatus"]
        {
            return Ok(());
        }
        let mut updated = vm.clone();
        updated["status"] = status;
        self.api
            .update_status(
                &format!("/apis/kubevirt.io/v1/namespaces/{namespace}/virtualmachines/{name}"),
                &updated,
            )
            .await?;
        Ok(())
    }
}

/// Is this VMI ours?
fn owned_by(vmi: &Value, vm_uid: &str) -> bool {
    !vm_uid.is_empty()
        && vmi["metadata"]["ownerReferences"]
            .as_array()
            .map(|refs| refs.iter().any(|r| r["uid"].as_str() == Some(vm_uid)))
            .unwrap_or(false)
}

/// A VMI is ready when its own status says the guest is running.
fn is_ready(vmi: &Value) -> bool {
    vmi["status"]["phase"].as_str() == Some("Running")
}

/// The one-word summary `oc get vm` prints.
fn printable_status(want: bool, vmi: Option<&Value>) -> &'static str {
    match (want, vmi) {
        (_, Some(i)) if !i["metadata"]["deletionTimestamp"].is_null() => "Terminating",
        (true, Some(i)) if is_ready(i) => "Running",
        // Asked for and created, but the guest is not up yet — which is where
        // a VM sits while its disks are cloned, and the state a reader most
        // often wants distinguished from "Running".
        (true, Some(_)) => "Starting",
        (true, None) => "Starting",
        (false, Some(_)) => "Stopping",
        (false, None) => "Stopped",
    }
}

#[cfg(test)]
mod tests {
    use super::*;




    #[test]
    fn a_vmi_is_only_ours_if_it_says_so() {
        // Name alone would adopt a hand-applied VMI and then delete it the
        // first time the VM was stopped.
        let mine = json!({"metadata": {"ownerReferences": [{"uid": "vm-1"}]}});
        assert!(owned_by(&mine, "vm-1"));
        assert!(!owned_by(&mine, "vm-2"));
        assert!(!owned_by(&json!({"metadata": {}}), "vm-1"));
        // An empty uid matches nothing, rather than everything.
        assert!(!owned_by(&json!({"metadata": {"ownerReferences": [{"uid": ""}]}}), ""));
    }

    #[test]
    fn printable_status_says_what_a_reader_needs() {
        let running = json!({"status": {"phase": "Running"}});
        let pending = json!({"status": {"phase": "Scheduling"}});
        let going = json!({"metadata": {"deletionTimestamp": "now"}});
        assert_eq!(printable_status(true, Some(&running)), "Running");
        assert_eq!(printable_status(true, Some(&pending)), "Starting");
        assert_eq!(printable_status(true, None), "Starting");
        assert_eq!(printable_status(false, None), "Stopped");
        assert_eq!(printable_status(false, Some(&pending)), "Stopping");
        // Terminating beats everything: it is what is actually happening.
        assert_eq!(printable_status(true, Some(&going)), "Terminating");
    }
}
