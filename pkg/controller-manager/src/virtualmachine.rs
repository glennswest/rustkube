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
//! A start that fails on the node is retried by the kubelet itself under
//! `Always`, `RerunOnFailure` and `running: true` (rustkube-node#76): the VMI
//! stays `Pending` with `reason: FailedStart` and the attempt and error in
//! its message, and is never `Failed`. That reads `CrashLoopBackOff` too,
//! with the same `Failure` condition (#209) — not `Starting` forever. The
//! retry is the kubelet's, so the VMI is not replaced and `startFailure` is
//! not counted for it.
//!
//! ## Template edits on a running VM (#269)
//!
//! `spec.template` is copied into the VMI when it is created; an edit after
//! that is diffed against the VMI's spec on every pass, in KubeVirt's
//! LiveUpdate shape. What the node can change on a running machine (stormvm
//! hotplug, rustkube-node#246/#247) is written into the VMI's spec, and the
//! kubelet applies it:
//!
//! - `domain.cpu.sockets`, up to the VMI's `maxSockets`
//! - `domain.memory.guest`, up to the VMI's `maxGuest`
//! - an interface with the `bridge` binding on a non-pod network, added or
//!   removed with its `networks` entry
//! - a disk (virtio or SCSI, not a cloud-init seed) added with its volume;
//!   a hot-added one removed — the kubelet marks those `hotplugVolume` in
//!   `status.volumeStatus`, and one it has not reported yet was never given
//!   to the machine
//!
//! Anything else — another field, a change past the maxima or with none, a
//! disk the machine booted with removed — waits for the next start, and the
//! VM says so with the condition `RestartRequired` (True, the fields in its
//! message). So does a live change the kubelet refused (its
//! `HotplugSynchronized` False). A new VMI takes the whole template, so the
//! condition goes when the VM is restarted, or when the template is put back.
//!
//! The owner reference is what makes deleting the VM take the machine with it.
//! Without it a deleted VirtualMachine leaves its guest running on a node with
//! nothing in the API pointing at it, which is the failure this whole object
//! exists to prevent.

use crate::owned::{self, Controller, Deps};
use crate::runner::ApiClient;
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::{debug, info, warn};

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
        owned::run(&self.api, self).await;
    }

    async fn reconcile_vm(&self, vm: &Value, vmis: &[Value]) -> anyhow::Result<()> {
        if !vm["metadata"]["deletionTimestamp"].is_null() {
            return Ok(());
        }
        let namespace = vm["metadata"]["namespace"].as_str().unwrap_or("default");
        let name = vm["metadata"]["name"].as_str().unwrap_or("");
        let uid = vm["metadata"]["uid"].as_str().unwrap_or("");
        if name.is_empty() {
            return Ok(());
        }
        // The VMI this VM owns: named after it *and* owned by it. Name
        // alone would adopt a hand-applied VMI that happens to share the
        // name, and then delete it when the VM stops.
        let owned = vmis
            .iter()
            .find(|i| i["metadata"]["name"].as_str() == Some(name) && owned_by(i, uid));

        let want = apimachinery::kubevirt::wants_running(vm);
        let now = chrono::Utc::now();
        let mut start_failure = vm["status"]["startFailure"].clone();
        let mut restarting = false;
        let mut restart_required = Vec::new();
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
                    if !due {
                        if let Some(at) = start_failure["retryAfterTimestamp"]
                            .as_str()
                            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                        {
                            apimachinery::reactor::requeue_at_time(at.with_timezone(&chrono::Utc));
                        }
                    }
                    if due {
                        self.delete_vmi(namespace, name, vm, vmi).await;
                    }
                }
                Some(_) => {}
                // Up again: the crash loop, if there was one, is over.
                None => {
                    if is_ready(vmi) {
                        start_failure = Value::Null;
                    }
                    if vmi["metadata"]["deletionTimestamp"].is_null() {
                        let update = live_update(&vm["spec"]["template"]["spec"], vmi);
                        if let Some(spec) = &update.spec {
                            self.update_vmi(namespace, name, vm, vmi, spec, &update.applied).await?;
                        }
                        restart_required = update.restart;
                    }
                }
            },
            // Stopped: a later start begins without a backoff to serve.
            (false, None) => start_failure = Value::Null,
        }
        let status =
            desired_status(vm, want, owned, restarting, &start_failure, &restart_required, now);
        if let Err(e) = self.write_status(namespace, name, vm, status).await {
            warn!("VirtualMachine {namespace}/{name} status: {e}");
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

    /// Write a live change into the running VMI's spec (#269). The VMI's
    /// resourceVersion goes with it: a VMI changed since it was read is a
    /// 409, and the pass is retried from fresh state.
    async fn update_vmi(
        &self,
        namespace: &str,
        name: &str,
        vm: &Value,
        vmi: &Value,
        spec: &Value,
        applied: &[String],
    ) -> anyhow::Result<()> {
        let mut updated = vmi.clone();
        updated["spec"] = spec.clone();
        let resp = self
            .api
            .update(
                &format!("/apis/kubevirt.io/v1/namespaces/{namespace}/virtualmachineinstances/{name}"),
                &updated,
            )
            .await?;
        if resp["kind"] == "Status" && resp["status"] == "Failure" {
            anyhow::bail!(
                "VMI live update refused: {} {}",
                resp["code"],
                resp["message"].as_str().unwrap_or("")
            );
        }
        let what = applied.join("; ");
        info!("VirtualMachine {namespace}/{name}: live update: {what}");
        self.recorder
            .event(vm, "Normal", "LiveUpdate", &format!("Applied to the running VirtualMachineInstance: {what}"))
            .await;
        Ok(())
    }

    async fn delete_vmi(&self, namespace: &str, name: &str, vm: &Value, vmi: &Value) {
        // Already going: a second DELETE each tick would spam the log and the
        // event stream for the whole of a guest's shutdown grace.
        if !vmi["metadata"]["deletionTimestamp"].is_null() {
            return;
        }
        let path =
            format!("/apis/kubevirt.io/v1/namespaces/{namespace}/virtualmachineinstances/{name}");
        match self.api.delete_observed(&path, vmi).await {
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

/// Is the kubelet retrying a start that failed (rustkube-node#76)? The VMI
/// has not finished and is not running; its `reason` says why, and its
/// `message` names the attempt and the error (#209).
fn failing_start(vmi: &Value) -> bool {
    vmi["status"]["reason"].as_str() == Some("FailedStart")
        && finished_phase(vmi).is_none()
        && !is_ready(vmi)
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

/// What a template edit does to the running VMI (#269).
#[derive(Debug, Default)]
struct LiveUpdate {
    /// The VMI's new spec, when something is applied live.
    spec: Option<Value>,
    /// What was applied, for the log and the Event.
    applied: Vec<String>,
    /// What waits for the next start, for `RestartRequired`.
    restart: Vec<String>,
}

/// Diff the VM's template spec against its running VMI's spec.
fn live_update(tmpl: &Value, vmi: &Value) -> LiveUpdate {
    let cur = &vmi["spec"];
    let mut next = cur.clone();
    let mut up = LiveUpdate::default();
    const T: &str = "spec.template.spec";

    // vCPUs: sockets within the headroom the VMI was started with.
    let want = tmpl["domain"]["cpu"]["sockets"].as_u64().unwrap_or(1);
    let have = cur["domain"]["cpu"]["sockets"].as_u64().unwrap_or(1);
    if want != have {
        match cur["domain"]["cpu"]["maxSockets"].as_u64() {
            Some(max) if want >= 1 && want <= max => {
                next["domain"]["cpu"]["sockets"] = json!(want);
                up.applied.push(format!("cpu sockets {have} → {want}"));
            }
            Some(max) => up.restart.push(format!(
                "{T}.domain.cpu.sockets: {want} is past the running maxSockets {max}"
            )),
            None => up.restart.push(format!(
                "{T}.domain.cpu.sockets: {have} → {want} (no maxSockets on the running VMI)"
            )),
        }
    }

    // Memory: guest within maxGuest.
    let want = &tmpl["domain"]["memory"]["guest"];
    let have = &cur["domain"]["memory"]["guest"];
    let bytes = |q: &Value| q.as_str().map(apimachinery::quantity::parse_bytes);
    if bytes(want) != bytes(have) {
        let max = bytes(&cur["domain"]["memory"]["maxGuest"]);
        match (bytes(want), max) {
            (Some(w), Some(m)) if w > 0 && w <= m => {
                next["domain"]["memory"]["guest"] = want.clone();
                up.applied.push(format!("memory.guest {} → {}", have.as_str().unwrap_or("-"), want.as_str().unwrap_or("-")));
            }
            (Some(_), Some(_)) => up.restart.push(format!(
                "{T}.domain.memory.guest: {} is past the running maxGuest {}",
                want.as_str().unwrap_or("-"),
                cur["domain"]["memory"]["maxGuest"].as_str().unwrap_or("-")
            )),
            _ => up.restart.push(format!(
                "{T}.domain.memory.guest: {} → {} (no maxGuest on the running VMI)",
                have.as_str().unwrap_or("-"),
                want.as_str().unwrap_or("-")
            )),
        }
    }

    // Interfaces with their networks, by name.
    let hot_nic = |i: &Value, n: &Value| !i["bridge"].is_null() && n.is_object() && n["pod"].is_null();
    paired(
        tmpl,
        cur,
        &mut next,
        &mut up,
        (&["domain", "devices", "interfaces"], "networks"),
        "interface",
        hot_nic,
        |_| true,
    );

    // Disks with their volumes, by name.
    let hot_disk = |d: &Value, v: &Value| {
        d["disk"].is_object()
            && matches!(d["disk"]["bus"].as_str(), None | Some("virtio") | Some("scsi"))
            && v.is_object()
            && v["cloudInitNoCloud"].is_null()
            && v["cloudInitConfigDrive"].is_null()
    };
    let statuses = vmi["status"]["volumeStatus"].as_array().cloned();
    let hot_added = |name: &str| match &statuses {
        // Never reported: the kubelet has given the machine nothing yet.
        None => false,
        Some(list) => match list.iter().find(|s| s["name"].as_str() == Some(name)) {
            Some(s) => !s["hotplugVolume"].is_null(),
            None => true,
        },
    };
    paired(
        tmpl,
        cur,
        &mut next,
        &mut up,
        (&["domain", "devices", "disks"], "volumes"),
        "disk",
        hot_disk,
        // A disk removed live must be one added live.
        hot_added,
    );

    // Everything else: any difference waits for a restart.
    let mut a = tmpl.clone();
    let mut b = cur.clone();
    for v in [&mut a, &mut b] {
        strip(v, &["domain", "cpu"], "sockets");
        strip(v, &["domain", "memory"], "guest");
        strip(v, &["domain", "devices"], "interfaces");
        strip(v, &["domain", "devices"], "disks");
        strip(v, &[], "networks");
        strip(v, &[], "volumes");
    }
    if let Some(path) = first_difference(&a, &b, T) {
        up.restart.push(format!("{path} changed"));
    }

    // A live change the node refused waits for the next start too.
    if let Some(c) = vmi["status"]["conditions"]
        .as_array()
        .and_then(|cs| cs.iter().find(|c| c["type"] == "HotplugSynchronized"))
        .filter(|c| c["status"] == "False")
    {
        up.restart.push(format!(
            "the node refused a live change: {}",
            c["message"].as_str().unwrap_or("HotplugSynchronized False")
        ));
    }

    if !up.applied.is_empty() && next != *cur {
        up.spec = Some(next);
    } else {
        up.applied.clear();
    }
    up
}

/// Diff a named device list (`interfaces`, `disks`) and the spec list paired
/// with it by name (`networks`, `volumes`). An addition or removal `hot`
/// allows is applied to `next` (additions at the end, so nothing already
/// there is renumbered; a removal only if `removable` says the machine can
/// give it back); any other difference is a restart.
#[allow(clippy::too_many_arguments)]
fn paired(
    tmpl: &Value,
    cur: &Value,
    next: &mut Value,
    up: &mut LiveUpdate,
    (devices, specs): (&[&str], &str),
    what: &str,
    hot: impl Fn(&Value, &Value) -> bool,
    removable: impl Fn(&str) -> bool,
) {
    let field = format!("spec.template.spec.{}", devices.join("."));
    let dev = |v: &Value| at(v, devices).as_array().cloned().unwrap_or_default();
    let spec = |v: &Value| v[specs].as_array().cloned().unwrap_or_default();
    let named = |l: &[Value], n: &str| l.iter().find(|x| x["name"].as_str() == Some(n)).cloned().unwrap_or(Value::Null);
    let (td, ts, cd, cs) = (dev(tmpl), spec(tmpl), dev(cur), spec(cur));
    let mut names: Vec<String> = Vec::new();
    for x in td.iter().chain(&ts).chain(&cd).chain(&cs) {
        if let Some(n) = x["name"].as_str() {
            if !names.iter().any(|m| m == n) {
                names.push(n.to_string());
            }
        }
    }
    let (mut nd, mut ns) = (cd.clone(), cs.clone());
    let mut changed = false;
    for n in &names {
        let (t, tv, c, cv) = (named(&td, n), named(&ts, n), named(&cd, n), named(&cs, n));
        if t == c && tv == cv {
            continue;
        }
        let present = |d: &Value, v: &Value| !d.is_null() || !v.is_null();
        match (present(&t, &tv), present(&c, &cv)) {
            (true, false) if hot(&t, &tv) => {
                nd.push(t);
                ns.push(tv);
                changed = true;
                up.applied.push(format!("{what} added: {n}"));
            }
            (false, true) if hot(&c, &cv) && removable(n) => {
                nd.retain(|x| x["name"].as_str() != Some(n));
                ns.retain(|x| x["name"].as_str() != Some(n));
                changed = true;
                up.applied.push(format!("{what} removed: {n}"));
            }
            (true, false) => up.restart.push(format!("{field}: {n} added (not a live {what})")),
            (false, true) if hot(&c, &cv) => {
                up.restart.push(format!("{field}: {n} removed (the machine started with it)"))
            }
            (false, true) => up.restart.push(format!("{field}: {n} removed (not a live {what})")),
            _ => up.restart.push(format!("{field}: {n} changed")),
        }
    }
    if changed {
        set_at(next, devices, json!(nd));
        next[specs] = json!(ns);
    }
}

fn at<'a>(v: &'a Value, path: &[&str]) -> &'a Value {
    path.iter().fold(v, |v, k| &v[*k])
}

fn set_at(v: &mut Value, path: &[&str], value: Value) {
    let mut cur = v;
    for k in path {
        cur = &mut cur[*k];
    }
    *cur = value;
}

/// Remove `key` from the object at `path`, if there is one.
fn strip(v: &mut Value, path: &[&str], key: &str) {
    let mut cur = v;
    for k in path {
        match cur.get_mut(*k) {
            Some(next) => cur = next,
            None => return,
        }
    }
    if let Some(m) = cur.as_object_mut() {
        m.remove(key);
    }
}

/// The path of the first field where `a` and `b` differ, descending through
/// objects (a list or a scalar is one field). `None` when they are equal.
fn first_difference(a: &Value, b: &Value, prefix: &str) -> Option<String> {
    if a == b {
        return None;
    }
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            let mut keys: Vec<&String> = x.keys().chain(y.keys()).collect();
            keys.sort();
            keys.dedup();
            keys.into_iter().find_map(|k| {
                first_difference(
                    x.get(k).unwrap_or(&Value::Null),
                    y.get(k).unwrap_or(&Value::Null),
                    &format!("{prefix}.{k}"),
                )
            })
        }
        _ => Some(prefix.to_string()),
    }
}

/// The VM's whole status, as this controller would write it. Fields it does
/// not own are kept. `restart_required` is what waits for the next start.
fn desired_status(
    vm: &Value,
    want: bool,
    vmi: Option<&Value>,
    restarting: bool,
    start_failure: &Value,
    restart_required: &[String],
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
    if let Some(i) = vmi.filter(|i| finished_phase(i) == Some("Failed") || failing_start(i)) {
        conditions.push(json!({
            "type": "Failure",
            "status": "True",
            "reason": i["status"]["reason"].as_str().unwrap_or("VMIFailed"),
            "message": i["status"]["message"].as_str().unwrap_or("VirtualMachineInstance failed"),
        }));
    }
    if !restart_required.is_empty() {
        conditions.push(json!({
            "type": "RestartRequired",
            "status": "True",
            "reason": "RestartRequired",
            "message": format!("takes effect at the next start: {}", restart_required.join("; ")),
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
        (true, Some(i)) if finished_phase(i) == Some("Failed") && restarting => "CrashLoopBackOff",
        // The kubelet retrying a start that failed (#209): the same word, as
        // the guest keeps failing and is being retried.
        (true, Some(i)) if failing_start(i) => "CrashLoopBackOff",
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
        assert!(!restarts_after(
            &json!({"spec": {"runStrategy": "Manual", "running": true}}),
            "Failed"
        ));
        // Plain `running: true` is upstream's Always.
        assert!(restarts_after(
            &json!({"spec": {"running": true}}),
            "Succeeded"
        ));
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
        let st = desired_status(&vm, true, Some(&failed), false, &Value::Null, &[], now);
        assert_eq!(st["printableStatus"], "Failed");
        assert_eq!(st["ready"], false);
        assert_eq!(
            st["other"], 1,
            "fields the controller does not own are kept"
        );
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
        let later = desired_status(
            &vm2,
            true,
            Some(&failed),
            false,
            &Value::Null,
            &[],
            at("2026-09-28T10:05:00Z"),
        );
        assert_eq!(later, vm2["status"]);
    }

    #[test]
    fn ready_follows_the_vmi_on_every_reconcile() {
        let now = at("2026-09-28T10:00:00Z");
        // Status last written while it was starting; the VMI is Running now.
        let vm = json!({"spec": {"running": true},
            "status": {"created": true, "ready": false, "printableStatus": "Starting"}});
        let st = desired_status(
            &vm,
            true,
            Some(&vmi("a", "Running")),
            false,
            &Value::Null,
            &[],
            now,
        );
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
        assert!(!owned_by(
            &json!({"metadata": {"ownerReferences": [{"uid": ""}]}}),
            ""
        ));
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
        assert_eq!(
            printable_status(true, Some(&failed), true),
            "CrashLoopBackOff"
        );
        assert_eq!(printable_status(true, Some(&failed), false), "Failed");
        assert_eq!(printable_status(true, Some(&done), false), "Stopped");
        assert_eq!(printable_status(true, Some(&done), true), "Starting");
    }

    /// The kubelet retrying a failed start (rustkube-node#76) leaves the VMI
    /// Pending with reason FailedStart. The VM must not read Starting (#209).
    #[test]
    fn a_start_the_kubelet_is_retrying_is_not_starting() {
        let now = at("2026-10-06T10:00:00Z");
        let msg = "start failed (attempt 2), retrying in 20s: multus network not supported";
        let retrying = json!({"metadata": {"uid": "a"},
            "status": {"phase": "Pending", "reason": "FailedStart", "message": msg}});
        assert_eq!(printable_status(true, Some(&retrying), true), "CrashLoopBackOff");
        // Whatever the strategy: the kubelet only retries where it restarts.
        assert_eq!(printable_status(true, Some(&retrying), false), "CrashLoopBackOff");
        let vm = json!({"spec": {"runStrategy": "Always"}, "status": {}});
        let st = desired_status(&vm, true, Some(&retrying), true, &Value::Null, &[], now);
        let failure = st["conditions"].as_array().unwrap().iter()
            .find(|c| c["type"] == "Failure").expect("a Failure condition");
        assert_eq!((failure["reason"].as_str(), failure["message"].as_str()), (Some("FailedStart"), Some(msg)));
        assert!(st.get("startFailure").is_none(), "the kubelet's retry is not counted here");
        // The next attempt's message replaces it; the transition time stays.
        let next = json!({"metadata": {"uid": "a"}, "status": {"phase": "Pending", "reason": "FailedStart",
            "message": "start failed (attempt 3), retrying in 40s: multus network not supported"}});
        let vm2 = json!({"spec": vm["spec"], "status": st.clone()});
        let st2 = desired_status(&vm2, true, Some(&next), true, &Value::Null, &[], at("2026-10-06T10:00:20Z"));
        let f2 = st2["conditions"].as_array().unwrap().iter().find(|c| c["type"] == "Failure").unwrap();
        assert!(f2["message"].as_str().unwrap().contains("attempt 3"));
        assert_eq!(f2["lastTransitionTime"], failure["lastTransitionTime"]);
        // Started at last: Running, and no Failure condition.
        let up = json!({"metadata": {"uid": "a"}, "status": {"phase": "Running", "reason": "FailedStart"}});
        let st3 = desired_status(&vm2, true, Some(&up), true, &Value::Null, &[], now);
        assert_eq!(st3["printableStatus"], "Running");
        assert!(!st3["conditions"].as_array().unwrap().iter().any(|c| c["type"] == "Failure"));
        // Another reason on a pending VMI is still just starting.
        let sched = json!({"status": {"phase": "Pending", "reason": "Unschedulable"}});
        assert_eq!(printable_status(true, Some(&sched), true), "Starting");
    }

    /// A running VMI with hotplug headroom, one boot disk, one pod NIC.
    fn running_vmi() -> Value {
        json!({"metadata": {"uid": "i"}, "spec": {
            "domain": {
                "cpu": {"cores": 2, "sockets": 1, "maxSockets": 4},
                "memory": {"guest": "2Gi", "maxGuest": "8Gi"},
                "devices": {
                    "disks": [{"name": "root", "disk": {"bus": "virtio"}}],
                    "interfaces": [{"name": "default", "masquerade": {}}],
                },
            },
            "networks": [{"name": "default", "pod": {}}],
            "volumes": [{"name": "root", "persistentVolumeClaim": {"claimName": "root"}}],
        }, "status": {"phase": "Running",
            "volumeStatus": [{"name": "root", "phase": "Ready"}]}})
    }

    #[test]
    fn an_unchanged_template_changes_nothing() {
        let vmi = running_vmi();
        let up = live_update(&vmi["spec"], &vmi);
        assert!(up.spec.is_none() && up.applied.is_empty() && up.restart.is_empty(), "{up:?}");
    }

    #[test]
    fn sockets_and_memory_within_the_headroom_are_applied_live() {
        let vmi = running_vmi();
        let mut t = vmi["spec"].clone();
        t["domain"]["cpu"]["sockets"] = json!(3);
        t["domain"]["memory"]["guest"] = json!("6Gi");
        let up = live_update(&t, &vmi);
        assert!(up.restart.is_empty(), "{:?}", up.restart);
        let spec = up.spec.expect("a VMI write");
        assert_eq!(spec, t, "the VMI's spec is the template's");
        assert_eq!(up.applied.len(), 2);
    }

    #[test]
    fn past_the_maxima_or_without_them_waits_for_a_restart() {
        let vmi = running_vmi();
        let mut t = vmi["spec"].clone();
        t["domain"]["cpu"]["sockets"] = json!(5);
        t["domain"]["memory"]["guest"] = json!("16Gi");
        let up = live_update(&t, &vmi);
        assert!(up.spec.is_none());
        assert_eq!(up.restart.len(), 2, "{:?}", up.restart);
        assert!(up.restart[0].contains("maxSockets 4"), "{}", up.restart[0]);
        assert!(up.restart[1].contains("maxGuest 8Gi"), "{}", up.restart[1]);
        // No maxima: a restart, not an error.
        let mut bare = running_vmi();
        bare["spec"]["domain"]["cpu"].as_object_mut().unwrap().remove("maxSockets");
        bare["spec"]["domain"]["memory"].as_object_mut().unwrap().remove("maxGuest");
        let mut t = bare["spec"].clone();
        t["domain"]["cpu"]["sockets"] = json!(2);
        t["domain"]["memory"]["guest"] = json!("3Gi");
        let up = live_update(&t, &bare);
        assert!(up.spec.is_none());
        assert_eq!(up.restart.len(), 2, "{:?}", up.restart);
        // Raising a maximum is itself a restart (the headroom is fixed at start).
        let mut t = vmi["spec"].clone();
        t["domain"]["cpu"]["maxSockets"] = json!(8);
        let up = live_update(&t, &vmi);
        assert_eq!(up.restart, vec!["spec.template.spec.domain.cpu.maxSockets changed"]);
    }

    #[test]
    fn a_bridge_nic_comes_and_goes_live_the_pod_network_does_not() {
        let vmi = running_vmi();
        let mut t = vmi["spec"].clone();
        t["domain"]["devices"]["interfaces"].as_array_mut().unwrap()
            .push(json!({"name": "lan", "bridge": {}}));
        t["networks"].as_array_mut().unwrap()
            .push(json!({"name": "lan", "multus": {"networkName": "lan"}}));
        let up = live_update(&t, &vmi);
        assert!(up.restart.is_empty(), "{:?}", up.restart);
        let spec = up.spec.expect("a VMI write");
        assert_eq!(spec["domain"]["devices"]["interfaces"][1]["name"], "lan");
        assert_eq!(spec["networks"][1]["name"], "lan");
        // And removed again, from a VMI that has it.
        let mut with = vmi.clone();
        with["spec"] = spec;
        let up = live_update(&vmi["spec"], &with);
        assert_eq!(up.spec.expect("a VMI write"), vmi["spec"]);
        // The pod-network NIC is not hot-pluggable: removing it waits.
        let mut t = vmi["spec"].clone();
        t["domain"]["devices"]["interfaces"] = json!([]);
        t["networks"] = json!([]);
        let up = live_update(&t, &vmi);
        assert!(up.spec.is_none());
        assert_eq!(up.restart.len(), 1, "{:?}", up.restart);
        // A masquerade NIC added: a restart.
        let mut t = vmi["spec"].clone();
        t["domain"]["devices"]["interfaces"].as_array_mut().unwrap()
            .push(json!({"name": "m", "masquerade": {}}));
        t["networks"].as_array_mut().unwrap().push(json!({"name": "m", "multus": {"networkName": "m"}}));
        assert!(live_update(&t, &vmi).spec.is_none());
    }

    #[test]
    fn a_disk_added_is_live_and_only_a_hot_added_one_comes_out() {
        let vmi = running_vmi();
        let mut t = vmi["spec"].clone();
        t["domain"]["devices"]["disks"].as_array_mut().unwrap()
            .push(json!({"name": "data", "disk": {"bus": "scsi"}}));
        t["volumes"].as_array_mut().unwrap().push(json!({"name": "data", "emptyDisk": {"capacity": "1Gi"}}));
        let up = live_update(&t, &vmi);
        assert!(up.restart.is_empty(), "{:?}", up.restart);
        let spec = up.spec.expect("a VMI write");
        assert_eq!(spec, t);
        // The kubelet hot-added it: removing it is live.
        let mut with = vmi.clone();
        with["spec"] = spec.clone();
        with["status"]["volumeStatus"] = json!([{"name": "root", "phase": "Ready"},
            {"name": "data", "phase": "Ready", "target": "hp1", "hotplugVolume": {}}]);
        let up = live_update(&vmi["spec"], &with);
        assert_eq!(up.spec.expect("a VMI write"), vmi["spec"]);
        // Not reported yet: never given to the machine, so live too.
        with["status"]["volumeStatus"] = json!([{"name": "root", "phase": "Ready"}]);
        assert!(live_update(&vmi["spec"], &with).spec.is_some());
        // The disk it booted with: a restart, and the VMI keeps it.
        let mut t = vmi["spec"].clone();
        t["domain"]["devices"]["disks"] = json!([]);
        t["volumes"] = json!([]);
        let up = live_update(&t, &vmi);
        assert!(up.spec.is_none());
        assert!(up.restart[0].contains("root removed (the machine started with it)"), "{:?}", up.restart);
        // Nothing reported at all (an older kubelet): every disk is a boot disk.
        let mut old = with.clone();
        old["status"].as_object_mut().unwrap().remove("volumeStatus");
        assert!(live_update(&vmi["spec"], &old).spec.is_none());
        // A cloud-init seed added: start-only.
        let mut t = vmi["spec"].clone();
        t["domain"]["devices"]["disks"].as_array_mut().unwrap()
            .push(json!({"name": "seed", "disk": {"bus": "virtio"}}));
        t["volumes"].as_array_mut().unwrap().push(json!({"name": "seed", "cloudInitNoCloud": {"userData": "x"}}));
        assert!(live_update(&t, &vmi).spec.is_none());
    }

    #[test]
    fn a_live_change_and_a_restart_one_together() {
        let vmi = running_vmi();
        let mut t = vmi["spec"].clone();
        t["domain"]["cpu"]["sockets"] = json!(2);
        t["domain"]["cpu"]["cores"] = json!(4);
        let up = live_update(&t, &vmi);
        let spec = up.spec.expect("the live part is applied");
        assert_eq!((spec["domain"]["cpu"]["sockets"].as_u64(), spec["domain"]["cpu"]["cores"].as_u64()), (Some(2), Some(2)));
        assert_eq!(up.restart, vec!["spec.template.spec.domain.cpu.cores changed"]);
    }

    #[test]
    fn restart_required_is_a_condition_and_goes_when_nothing_waits() {
        let now = at("2026-10-13T10:00:00Z");
        let vm = json!({"spec": {"running": true}, "status": {}});
        let up = vmi("a", "Running");
        let st = desired_status(&vm, true, Some(&up), false, &Value::Null,
            &["spec.template.spec.domain.cpu.cores changed".to_string()], now);
        let c = st["conditions"].as_array().unwrap().iter()
            .find(|c| c["type"] == "RestartRequired").expect("RestartRequired");
        assert_eq!(c["status"], "True");
        assert!(c["message"].as_str().unwrap().contains("domain.cpu.cores"));
        let vm2 = json!({"spec": vm["spec"], "status": st});
        let st2 = desired_status(&vm2, true, Some(&up), false, &Value::Null, &[], now);
        assert!(!st2["conditions"].as_array().unwrap().iter().any(|c| c["type"] == "RestartRequired"));
    }

    #[test]
    fn a_change_the_node_refused_waits_for_a_restart() {
        let mut vmi = running_vmi();
        vmi["status"]["conditions"] = json!([{"type": "HotplugSynchronized", "status": "False",
            "message": "add_nic: no spare hot-plug port"}]);
        let up = live_update(&vmi["spec"], &vmi);
        assert!(up.spec.is_none());
        assert_eq!(up.restart, vec!["the node refused a live change: add_nic: no spare hot-plug port"]);
    }
}


#[async_trait::async_trait]
impl Controller for VirtualMachineController {
    fn name(&self) -> &'static str {
        "virtualmachine"
    }
    fn primary(&self) -> &'static str {
        "/apis/kubevirt.io/v1/virtualmachines"
    }
    fn children(&self) -> Option<&'static str> {
        Some("/apis/kubevirt.io/v1/virtualmachineinstances")
    }
    async fn reconcile(&self, vm: &Value, children: &[Value], _deps: &Deps) -> anyhow::Result<()> {
        self.reconcile_vm(vm, children).await
    }
}
