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
//! - `spec.running: true` (or `runStrategy: Always`/`RerunOnFailure`/`Once`)
//!   → the VMI exists
//! - `spec.running: false` (or `runStrategy: Halted`) → it does not
//! - `spec.template` is the VMI's spec, the way a Deployment's `template` is a
//!   pod's
//!
//! A VMI that has *finished* — phase `Failed` or `Succeeded` — is still there,
//! so "the VMI exists" alone would leave a crashed guest down for good under a
//! strategy that promises to restart it (#104). The run strategy decides, as
//! upstream's does:
//!
//! - `Always` (and plain `running: true`): recreated after either
//! - `RerunOnFailure`: recreated after `Failed`, left after `Succeeded`
//! - `Once`, `Manual`: left, whichever way it ended
//!
//! A failed instance is recreated with backoff — 10 s doubling to 5 min —
//! kept in `status.startFailure` (upstream's field) so it survives a
//! controller restart; the VM reads `CrashLoopBackOff` while it waits. One
//! left failed reads `Failed`, and either way the VMI's `status.message` is
//! on the VM's `Failure` condition, so `oc describe vm` says why.
//!
//! The owner reference is what makes deleting the VM take the machine with it.
//! Without it a deleted VirtualMachine leaves its guest running on a node with
//! nothing in the API pointing at it, which is the failure this whole object
//! exists to prevent.

use crate::runner::ApiClient;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::time::{self, Duration};
use tracing::{debug, error, info, warn};

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
            let now = chrono::Utc::now();
            let mut start_failure = vm["status"]["startFailure"].clone();
            let mut restarting = false;
            match (want, owned) {
                (true, None) => self.create_vmi(namespace, vm).await,
                (false, Some(vmi)) => self.delete_vmi(namespace, name, vm, vmi).await,
                (true, Some(vmi)) => match finished_phase(vmi) {
                    Some(phase) if restarts_after(vm, phase) => {
                        restarting = true;
                        let due = if phase == "Failed" {
                            start_failure = record_failure(&start_failure, vmi, now);
                            retry_due(&start_failure, now)
                        } else {
                            // A clean shutdown under `Always` is not a crash:
                            // no backoff, the same as upstream.
                            true
                        };
                        // Delete now, create on the next tick once it is gone:
                        // the replacement has the same name, so it cannot be
                        // created while the finished one is still there.
                        if due {
                            self.delete_vmi(namespace, name, vm, vmi).await;
                        }
                    }
                    Some(_) => {}
                    // Up again: the crash loop, if there was one, is over.
                    None if is_ready(vmi) => start_failure = Value::Null,
                    None => {}
                },
                // Stopped: a later start begins without a backoff to serve.
                (false, None) => start_failure = Value::Null,
            }
            let status = desired_status(vm, want, owned, restarting, &start_failure, now);
            if let Err(e) = self.write_status(namespace, name, vm, status).await {
                warn!("VirtualMachine {namespace}/{name} status: {e}");
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
        status: Value,
    ) -> anyhow::Result<()> {
        // Only when it changed. A status write per VM per two seconds is a
        // resourceVersion bump per VM per two seconds, which every informer in
        // the cluster then has to look at.
        if vm["status"] == status {
            return Ok(());
        }
        let mut updated = vm.clone();
        updated["status"] = status;
        let resp = self
            .api
            .update_status(
                &format!("/apis/kubevirt.io/v1/namespaces/{namespace}/virtualmachines/{name}"),
                &updated,
            )
            .await?;
        // The client does not look at the HTTP status, so a refused write
        // (a 409 on a stale resourceVersion, a 404) came back as a parsed
        // `Status` and was taken for success — which is one way a VM reads
        // `Starting` next to a `Running` VMI (#104). Say so instead.
        if resp["kind"] == "Status" && resp["status"] == "Failure" {
            anyhow::bail!(
                "status write refused: {} {}",
                resp["code"],
                resp["message"].as_str().unwrap_or("")
            );
        }
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

/// `Failed` or `Succeeded`: the guest has ended and this VMI will not run it
/// again. Only a new VMI can.
fn finished_phase(vmi: &Value) -> Option<&str> {
    match vmi["status"]["phase"].as_str() {
        Some(p @ ("Failed" | "Succeeded")) => Some(p),
        _ => None,
    }
}

/// Does this VM's run strategy replace an instance that ended in `phase`?
///
/// Plain `running: true` is upstream's `Always`. `Manual` is not: under it
/// only start and stop act, and `Once` means exactly once. `Halted` never gets
/// here — it does not want a VMI at all.
fn restarts_after(vm: &Value, phase: &str) -> bool {
    match vm["spec"]["runStrategy"].as_str() {
        Some("Always") => true,
        Some("RerunOnFailure") => phase == "Failed",
        Some(_) => false,
        None => vm["spec"]["running"].as_bool().unwrap_or(false),
    }
}

/// Backoff before the `count`th failed instance is replaced: 10 s, doubling,
/// capped at 5 min — upstream's schedule, without its jitter.
fn backoff_secs(count: u64) -> i64 {
    let exp = count.saturating_sub(1).min(5) as u32;
    (10i64 << exp).min(300)
}

/// `status.startFailure` after seeing this failed VMI. Counted once per VMI
/// (by uid), not once per tick: the same failed instance is seen every two
/// seconds until it is replaced.
fn record_failure(prev: &Value, vmi: &Value, now: chrono::DateTime<chrono::Utc>) -> Value {
    let uid = vmi["metadata"]["uid"].as_str().unwrap_or("");
    if !prev.is_null() && prev["lastFailedVMIUID"].as_str() == Some(uid) {
        return prev.clone();
    }
    let count = prev["consecutiveFailCount"].as_u64().unwrap_or(0) + 1;
    let retry = now + chrono::Duration::seconds(backoff_secs(count));
    json!({
        "consecutiveFailCount": count,
        "lastFailedVMIUID": uid,
        "retryAfterTimestamp": retry.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    })
}

/// Has the backoff in `startFailure` run out? An unreadable timestamp counts
/// as run out: a VM stuck behind a retry time nobody can parse is the worse
/// failure.
fn retry_due(start_failure: &Value, now: chrono::DateTime<chrono::Utc>) -> bool {
    start_failure["retryAfterTimestamp"]
        .as_str()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|t| now >= t)
        .unwrap_or(true)
}

/// The VM's whole status, as this controller would write it. Fields it does
/// not own are kept.
fn desired_status(
    vm: &Value,
    want: bool,
    vmi: Option<&Value>,
    restarting: bool,
    start_failure: &Value,
    now: chrono::DateTime<chrono::Utc>,
) -> Value {
    let mut status = match &vm["status"] {
        Value::Object(m) => Value::Object(m.clone()),
        _ => json!({}),
    };
    let ready = vmi.map(is_ready).unwrap_or(false);
    status["created"] = json!(vmi.is_some());
    status["ready"] = json!(ready);
    status["printableStatus"] = json!(printable_status(want, vmi, restarting));

    let mut conditions = vec![json!({
        "type": "Ready",
        "status": if ready { "True" } else { "False" },
    })];
    if let Some(i) = vmi.filter(|i| finished_phase(i) == Some("Failed")) {
        conditions.push(json!({
            "type": "Failure",
            "status": "True",
            "reason": i["status"]["reason"].as_str().unwrap_or("VMIFailed"),
            "message": i["status"]["message"].as_str().unwrap_or("VirtualMachineInstance failed"),
        }));
    }
    // A condition's transition time moves only when its status does, or
    // every tick would be a write.
    for c in &mut conditions {
        let prev = vm["status"]["conditions"]
            .as_array()
            .and_then(|cs| cs.iter().find(|p| p["type"] == c["type"]));
        c["lastTransitionTime"] = match prev {
            Some(p) if p["status"] == c["status"] && !p["lastTransitionTime"].is_null() => {
                p["lastTransitionTime"].clone()
            }
            _ => json!(now.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
        };
    }
    status["conditions"] = json!(conditions);

    let obj = status.as_object_mut().expect("status is an object");
    if start_failure.is_null() {
        obj.remove("startFailure");
    } else {
        obj.insert("startFailure".into(), start_failure.clone());
    }
    status
}

/// The one-word summary `oc get vm` prints.
///
/// `restarting` is whether the run strategy replaces a finished VMI.
fn printable_status(want: bool, vmi: Option<&Value>, restarting: bool) -> &'static str {
    match (want, vmi) {
        (_, Some(i)) if !i["metadata"]["deletionTimestamp"].is_null() => "Terminating",
        (true, Some(i)) if is_ready(i) => "Running",
        // Upstream's word for a failed guest waiting out its backoff.
        (true, Some(i)) if finished_phase(i) == Some("Failed") && restarting => {
            "CrashLoopBackOff"
        }
        // Failed and staying that way (`Once`, `Manual`). Upstream would say
        // `Stopped`, which hides the one thing a reader needs to know.
        (true, Some(i)) if finished_phase(i) == Some("Failed") => "Failed",
        (true, Some(i)) if finished_phase(i) == Some("Succeeded") && !restarting => "Stopped",
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

    fn at(t: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(t).unwrap().into()
    }

    fn vmi(uid: &str, phase: &str) -> Value {
        json!({"metadata": {"uid": uid}, "status": {"phase": phase, "message": "disk gone"}})
    }

    #[test]
    fn run_strategy_decides_what_is_replaced() {
        let s = |rs: &str| json!({"spec": {"runStrategy": rs}});
        assert!(restarts_after(&s("Always"), "Failed"));
        assert!(restarts_after(&s("Always"), "Succeeded"));
        assert!(restarts_after(&s("RerunOnFailure"), "Failed"));
        assert!(!restarts_after(&s("RerunOnFailure"), "Succeeded"));
        assert!(!restarts_after(&s("Once"), "Failed"));
        assert!(!restarts_after(&json!({"spec": {"runStrategy": "Manual", "running": true}}), "Failed"));
        // Plain `running: true` is upstream's Always.
        assert!(restarts_after(&json!({"spec": {"running": true}}), "Succeeded"));
    }

    #[test]
    fn backoff_doubles_to_five_minutes() {
        let got: Vec<i64> = (1..=8).map(backoff_secs).collect();
        assert_eq!(got, vec![10, 20, 40, 80, 160, 300, 300, 300]);
    }

    #[test]
    fn a_failed_vmi_is_counted_once_not_once_per_tick() {
        let t0 = at("2026-09-28T10:00:00Z");
        let first = record_failure(&Value::Null, &vmi("a", "Failed"), t0);
        assert_eq!(first["consecutiveFailCount"], 1);
        assert_eq!(first["retryAfterTimestamp"], "2026-09-28T10:00:10Z");
        // The same VMI two seconds later: nothing moves.
        let again = record_failure(&first, &vmi("a", "Failed"), at("2026-09-28T10:00:02Z"));
        assert_eq!(again, first);
        assert!(!retry_due(&again, at("2026-09-28T10:00:09Z")));
        assert!(retry_due(&again, at("2026-09-28T10:00:10Z")));
        // Its replacement fails too: count 2, 20 s.
        let second = record_failure(&first, &vmi("b", "Failed"), at("2026-09-28T10:00:15Z"));
        assert_eq!(second["consecutiveFailCount"], 2);
        assert_eq!(second["retryAfterTimestamp"], "2026-09-28T10:00:35Z");
        assert!(retry_due(&json!({"retryAfterTimestamp": "garbage"}), t0));
    }

    #[test]
    fn a_failed_vm_says_why_and_the_status_is_stable() {
        let now = at("2026-09-28T10:00:00Z");
        let failed = vmi("a", "Failed");
        let vm = json!({"spec": {"runStrategy": "Once"}, "status": {"other": 1}});
        let st = desired_status(&vm, true, Some(&failed), false, &Value::Null, now);
        assert_eq!(st["printableStatus"], "Failed");
        assert_eq!(st["ready"], false);
        assert_eq!(st["other"], 1, "fields the controller does not own are kept");
        let failure = st["conditions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["type"] == "Failure")
            .unwrap();
        assert_eq!(failure["message"], "disk gone");
        assert!(st.get("startFailure").is_none());
        // Written once, the next tick computes the same thing: no write loop.
        let vm2 = json!({"spec": vm["spec"], "status": st});
        let later = desired_status(&vm2, true, Some(&failed), false, &Value::Null,
            at("2026-09-28T10:05:00Z"));
        assert_eq!(later, vm2["status"]);
    }

    #[test]
    fn ready_follows_the_vmi_on_every_reconcile() {
        let now = at("2026-09-28T10:00:00Z");
        // Status last written while it was starting; the VMI is Running now.
        let vm = json!({"spec": {"running": true},
            "status": {"created": true, "ready": false, "printableStatus": "Starting"}});
        let st = desired_status(&vm, true, Some(&vmi("a", "Running")), false, &Value::Null, now);
        assert_eq!(st["ready"], true);
        assert_eq!(st["printableStatus"], "Running");
    }
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
        let failed = json!({"status": {"phase": "Failed"}});
        let done = json!({"status": {"phase": "Succeeded"}});
        assert_eq!(printable_status(true, Some(&running), false), "Running");
        assert_eq!(printable_status(true, Some(&pending), false), "Starting");
        assert_eq!(printable_status(true, None, false), "Starting");
        assert_eq!(printable_status(false, None, false), "Stopped");
        assert_eq!(printable_status(false, Some(&pending), false), "Stopping");
        // Terminating beats everything: it is what is actually happening.
        assert_eq!(printable_status(true, Some(&going), true), "Terminating");
        // A failed guest never reads Starting (#104).
        assert_eq!(printable_status(true, Some(&failed), true), "CrashLoopBackOff");
        assert_eq!(printable_status(true, Some(&failed), false), "Failed");
        assert_eq!(printable_status(true, Some(&done), false), "Stopped");
        assert_eq!(printable_status(true, Some(&done), true), "Starting");
    }
}
