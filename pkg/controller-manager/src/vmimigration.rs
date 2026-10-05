//! VirtualMachineInstanceMigration controller — the control-plane half of a
//! VMI live migration (#184; the node half is rustkube-node#40).
//!
//! A `VirtualMachineInstanceMigration` (kubevirt.io/v1, `spec.vmiName`) asks
//! for a running VMI to move to another node. As upstream KubeVirt, the work
//! is coordinated through the VMI's `status.migrationState`, which every
//! party watches; nothing is called directly. Who writes which field is in
//! [`apimachinery::kubevirt::active_migration`]. The phases:
//!
//! - **Pending**: the VMI must exist, be `Running` and be on a node. While
//!   another migration of the same VMI is in flight this one waits here. Then
//!   the controller writes the VMI's `migrationState` (`migrationUid`,
//!   `sourceNode`, `mode`) → **Scheduling**.
//! - **Scheduling**: the scheduler picks the target, with the filters and
//!   scores a VMI placement gets and the source node excluded, and writes
//!   `targetNode`; it says why on the migration if no node will do. After
//!   5 minutes unscheduled the migration fails.
//! - **Scheduled → PreparingTarget**: target chosen; the target kubelet
//!   starts a receiving machine and writes `targetNodeAddress` →
//!   **TargetReady**; the source kubelet starts sending and writes
//!   `startTimestamp` → **Running**. Not Running 15 minutes after creation:
//!   failed.
//! - **Succeeded**: the source reported `completed`; the controller moves the
//!   VMI's `status.nodeName` (and its `kubevirt.io/nodeName` label) to the
//!   target. **Failed**: the source reported `failed`, or the VMI went away.
//!
//! Deleting a migration that has not finished aborts it, as upstream: the
//! finalizer `kubevirt.io/migrationJobFinalize` holds the object until the
//! VMI says so. Before the source has started sending, the controller marks
//! the VMI's migration failed itself (nothing has moved); once it is
//! running, it sets `abortRequested` and waits for the source to report
//! `completed` or `failed` — at most 5 minutes, then lets the object go.
//!
//! The phase on the migration follows the VMI's `migrationState`, which it
//! also mirrors into its own `status.migrationState`, so `oc get vmim` and
//! the VMI agree.

use crate::owned::{self, Controller, Dependency, Deps};
use crate::runner::ApiClient;
use apimachinery::informer::Index;
use apimachinery::kubevirt::active_migration;
use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::{debug, info, warn};

pub const MIGRATIONS: &str = "/apis/kubevirt.io/v1/virtualmachineinstancemigrations";
const VMIS: &str = "/apis/kubevirt.io/v1/virtualmachineinstances";
/// Upstream's finalizer name.
pub const FINALIZER: &str = "kubevirt.io/migrationJobFinalize";
/// Upstream's `kubevirt.io/nodeName` VMI label, moved with the machine.
const NODE_LABEL: &str = "kubevirt.io/nodeName";
/// Unscheduled this long: no node will take it (upstream's unschedulable
/// pending timeout).
const SCHEDULING_TIMEOUT_SECS: i64 = 300;
/// Not sending this long after creation: the target never got ready
/// (upstream's catch-all pending timeout).
const PREPARE_TIMEOUT_SECS: i64 = 900;
/// How long an abort of a running migration waits for the source's answer.
const ABORT_WAIT_SECS: i64 = 300;

pub struct VmiMigrationController {
    api: Arc<ApiClient>,
}

/// What one reconcile does: the phase the migration is in, and the writes
/// that take it there.
#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    pub phase: &'static str,
    /// Why it failed, when the controller is the one saying so.
    pub failure: Option<String>,
    /// Merge patch for the VMI's `/status` (and labels).
    pub vmi_patch: Option<Value>,
    pub release_finalizer: bool,
    pub requeue_at: Option<DateTime<Utc>>,
}

fn ts(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn parse(t: &Value) -> Option<DateTime<Utc>> {
    t.as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc))
}

/// When the migration entered `phase`, from its phaseTransitionTimestamps.
fn entered(migration: &Value, phase: &str) -> Option<DateTime<Utc>> {
    migration["status"]["phaseTransitionTimestamps"]
        .as_array()?
        .iter()
        .rev()
        .find(|p| p["phase"] == phase)
        .and_then(|p| parse(&p["phaseTransitionTimestamp"]))
}

fn is_final(phase: &str) -> bool {
    matches!(phase, "Succeeded" | "Failed")
}

fn has_finalizer(migration: &Value) -> bool {
    migration["metadata"]["finalizers"]
        .as_array()
        .is_some_and(|f| f.iter().any(|f| f == FINALIZER))
}

/// A VMI migrationState failed by the controller: nothing was sent, so the
/// target kubelet tears down what it prepared and the source carries on.
fn fail_on_vmi(reason: &str, now: DateTime<Utc>, abort: bool) -> Value {
    let mut state = json!({
        "failed": true,
        "failureReason": reason,
        "endTimestamp": ts(now),
    });
    if abort {
        state["abortRequested"] = json!(true);
        state["abortStatus"] = json!("Succeeded");
    }
    json!({"status": {"migrationState": state}})
}

/// The next step for `migration`, given its VMI as the informer holds it.
pub fn plan(migration: &Value, vmi: Option<&Value>, now: DateTime<Utc>) -> Plan {
    let uid = migration["metadata"]["uid"].as_str().unwrap_or("");
    let vmi_name = migration["spec"]["vmiName"].as_str().unwrap_or("");
    let phase = migration["status"]["phase"].as_str().unwrap_or("");
    let deleting = !migration["metadata"]["deletionTimestamp"].is_null();
    let created = parse(&migration["metadata"]["creationTimestamp"]).unwrap_or(now);
    let fail = |why: String| Plan { phase: "Failed", failure: Some(why), ..Default::default() };

    // This migration's state on the VMI, in flight or finished.
    let ours = vmi
        .map(|v| &v["status"]["migrationState"])
        .filter(|m| !uid.is_empty() && m["migrationUid"].as_str() == Some(uid));
    let in_flight = vmi.and_then(active_migration).filter(|m| m["migrationUid"].as_str() == Some(uid));

    if deleting {
        let release = Plan { phase: if is_final(phase) { phase_static(phase) } else { "Failed" },
            failure: (!is_final(phase)).then(|| "aborted: the migration was deleted".to_string()),
            release_finalizer: true, ..Default::default() };
        let Some(state) = in_flight else { return release };
        if state["startTimestamp"].as_str().is_none_or(str::is_empty) {
            // Nothing sent yet: aborting is the controller's to finish.
            return Plan {
                vmi_patch: Some(fail_on_vmi("aborted: the migration was deleted", now, true)),
                ..release
            };
        }
        let asked = parse(&migration["metadata"]["deletionTimestamp"]).unwrap_or(now);
        if now >= asked + Duration::seconds(ABORT_WAIT_SECS) {
            return release;
        }
        return Plan {
            phase: phase_static(phase),
            vmi_patch: (state["abortRequested"].as_bool() != Some(true))
                .then(|| json!({"status": {"migrationState": {"abortRequested": true}}})),
            requeue_at: Some(asked + Duration::seconds(ABORT_WAIT_SECS)),
            ..Default::default()
        };
    }
    if is_final(phase) {
        return Plan { phase: phase_static(phase), ..Default::default() };
    }

    let Some(vmi) = vmi.filter(|v| v["metadata"]["deletionTimestamp"].is_null()) else {
        return fail(format!("VirtualMachineInstance {vmi_name} does not exist"));
    };

    if matches!(phase, "" | "Pending") {
        if in_flight.is_some() {
            // Written already; the phase write was what failed.
            return Plan { phase: "Scheduling", ..Default::default() };
        }
        if vmi["status"]["phase"].as_str() != Some("Running") {
            return fail(format!(
                "VirtualMachineInstance {vmi_name} is not running (phase {})",
                vmi["status"]["phase"].as_str().unwrap_or("unset")
            ));
        }
        let Some(source) = vmi["status"]["nodeName"].as_str().filter(|n| !n.is_empty()) else {
            return fail(format!("VirtualMachineInstance {vmi_name} is not on a node"));
        };
        if let Some(other) = active_migration(vmi) {
            // One at a time, as upstream; this one goes when that one ends.
            debug!(
                other = other["migrationUid"].as_str().unwrap_or(""),
                "VMI {vmi_name} is already migrating"
            );
            return Plan { phase: "Pending", ..Default::default() };
        }
        return Plan {
            phase: "Scheduling",
            vmi_patch: Some(json!({"status": {"migrationState": {
                "migrationUid": uid,
                "sourceNode": source,
                "mode": "PreCopy",
                "targetNode": null,
                "targetNodeAddress": null,
                "targetDirectMigrationNodePorts": null,
                "targetNodeDomainDetected": null,
                "startTimestamp": null,
                "endTimestamp": null,
                "completed": false,
                "failed": false,
                "failureReason": null,
                "abortRequested": null,
                "abortStatus": null,
            }}})),
            ..Default::default()
        };
    }

    let Some(state) = ours else {
        return fail(format!(
            "VirtualMachineInstance {vmi_name} no longer records this migration"
        ));
    };
    if state["failed"].as_bool() == Some(true) {
        return fail(state["failureReason"].as_str().unwrap_or("the source reported failure").to_string());
    }
    let target = state["targetNode"].as_str().filter(|n| !n.is_empty());
    if state["completed"].as_bool() == Some(true) {
        let Some(target) = target else {
            return fail("reported completed with no target node".into());
        };
        let moved = vmi["status"]["nodeName"].as_str() == Some(target)
            && vmi["metadata"]["labels"][NODE_LABEL].as_str().is_none_or(|l| l == target);
        return Plan {
            phase: "Succeeded",
            vmi_patch: (!moved).then(|| {
                let mut patch = json!({"status": {"nodeName": target}});
                if vmi["metadata"]["labels"][NODE_LABEL].is_string() {
                    patch["metadata"] = json!({"labels": {NODE_LABEL: target}});
                }
                patch
            }),
            ..Default::default()
        };
    }
    if state["startTimestamp"].as_str().is_some_and(|s| !s.is_empty()) {
        // Sending: progress and its limits are the source's (stormvm).
        return Plan { phase: "Running", ..Default::default() };
    }
    let Some(_) = target else {
        let since = entered(migration, "Scheduling").unwrap_or(now);
        let deadline = since + Duration::seconds(SCHEDULING_TIMEOUT_SECS);
        if now >= deadline {
            let why = format!("no node could take the migration target within {SCHEDULING_TIMEOUT_SECS} s");
            return Plan { vmi_patch: Some(fail_on_vmi(&why, now, false)), ..fail(why) };
        }
        return Plan { phase: "Scheduling", requeue_at: Some(deadline), ..Default::default() };
    };
    let deadline = created + Duration::seconds(PREPARE_TIMEOUT_SECS);
    if now >= deadline {
        let why = format!("the migration did not start within {PREPARE_TIMEOUT_SECS} s");
        return Plan { vmi_patch: Some(fail_on_vmi(&why, now, false)), ..fail(why) };
    }
    let phase = if state["targetNodeAddress"].as_str().is_some_and(|a| !a.is_empty()) {
        "TargetReady"
    } else {
        "PreparingTarget"
    };
    Plan { phase, requeue_at: Some(deadline), ..Default::default() }
}

fn phase_static(phase: &str) -> &'static str {
    match phase {
        "Succeeded" => "Succeeded",
        "Failed" => "Failed",
        "Scheduling" => "Scheduling",
        "Scheduled" => "Scheduled",
        "PreparingTarget" => "PreparingTarget",
        "TargetReady" => "TargetReady",
        "Running" => "Running",
        _ => "Pending",
    }
}

/// The migration's own status after `plan`: phase, transition times (a
/// target chosen passes through `Scheduled`), and a mirror of the VMI's
/// state for this migration. `None` when nothing changes.
pub fn desired_status(migration: &Value, vmi: Option<&Value>, plan: &Plan, now: DateTime<Utc>) -> Option<Value> {
    let uid = migration["metadata"]["uid"].as_str().unwrap_or("");
    let current = &migration["status"];
    let mut transitions = current["phaseTransitionTimestamps"].as_array().cloned().unwrap_or_default();
    let last = current["phase"].as_str().unwrap_or("");
    if last != plan.phase {
        let ordered = ["Pending", "Scheduling", "Scheduled", "PreparingTarget", "TargetReady", "Running", "Succeeded"];
        let from = ordered.iter().position(|p| *p == last);
        let to = ordered.iter().position(|p| *p == plan.phase);
        // Phases skipped between two observations still happened, in order.
        let passed: Vec<&str> = match (from, to) {
            (Some(f), Some(t)) if t > f => ordered[f + 1..=t].to_vec(),
            (None, Some(t)) if last.is_empty() => ordered[..=t].to_vec(),
            _ => vec![plan.phase],
        };
        for p in passed {
            transitions.push(json!({"phase": p, "phaseTransitionTimestamp": ts(now)}));
        }
    }
    let mut mirror = vmi
        .map(|v| v["status"]["migrationState"].clone())
        .filter(|m| m["migrationUid"].as_str() == Some(uid))
        .unwrap_or_else(|| current["migrationState"].clone());
    if let Some(why) = &plan.failure {
        if !mirror.is_object() {
            mirror = json!({});
        }
        mirror["failed"] = json!(true);
        mirror["failureReason"] = json!(why);
    }
    let mut status = json!({"phase": plan.phase, "phaseTransitionTimestamps": transitions});
    if mirror.is_object() {
        status["migrationState"] = mirror;
    }
    let unchanged = current["phase"] == status["phase"]
        && current["phaseTransitionTimestamps"] == status["phaseTransitionTimestamps"]
        && current["migrationState"] == status["migrationState"].clone();
    (!unchanged).then_some(status)
}

impl VmiMigrationController {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self { api }
    }

    pub async fn run(&self) {
        owned::run(&self.api, self).await;
    }

    async fn set_finalizers(&self, migration: &Value, finalizers: Vec<Value>) -> anyhow::Result<()> {
        let ns = migration["metadata"]["namespace"].as_str().unwrap_or("default");
        let name = migration["metadata"]["name"].as_str().unwrap_or("");
        self.api
            .patch_merge(
                &format!("/apis/kubevirt.io/v1/namespaces/{ns}/virtualmachineinstancemigrations/{name}"),
                &json!({"metadata": {
                    "resourceVersion": migration["metadata"]["resourceVersion"],
                    "finalizers": finalizers,
                }}),
            )
            .await?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl Controller for VmiMigrationController {
    fn name(&self) -> &'static str {
        "vmi-migration"
    }
    fn primary(&self) -> &'static str {
        MIGRATIONS
    }
    fn dependencies(&self) -> Vec<Dependency> {
        vec![Dependency {
            path: VMIS.into(),
            // A VMI wakes the migrations that name it (`spec.vmiName`).
            route: Arc::new(|delta, primary| {
                delta
                    .old
                    .iter()
                    .chain(delta.new.iter())
                    .flat_map(|v| {
                        owned::keys_at(
                            primary,
                            Index::Reference(
                                v["metadata"]["namespace"].as_str().unwrap_or("").into(),
                                "VirtualMachineInstance".into(),
                                v["metadata"]["name"].as_str().unwrap_or("").into(),
                            ),
                        )
                    })
                    .collect()
            }),
        }]
    }
    async fn reconcile(&self, migration: &Value, _: &[Value], deps: &Deps) -> anyhow::Result<()> {
        let ns = migration["metadata"]["namespace"].as_str().unwrap_or("default");
        let name = migration["metadata"]["name"].as_str().unwrap_or("");
        let vmi_name = migration["spec"]["vmiName"].as_str().unwrap_or("");
        let phase = migration["status"]["phase"].as_str().unwrap_or("");
        let deleting = !migration["metadata"]["deletionTimestamp"].is_null();
        if deleting && !has_finalizer(migration) {
            return Ok(());
        }
        if !deleting && !is_final(phase) && !has_finalizer(migration) {
            // Held until its VMI is told, so a delete is an abort.
            let mut finalizers = migration["metadata"]["finalizers"].as_array().cloned().unwrap_or_default();
            finalizers.push(json!(FINALIZER));
            return self.set_finalizers(migration, finalizers).await;
        }
        let vmi = deps
            .feed(0)
            .select(&Index::Name(ns.into(), vmi_name.into()))?
            .into_iter()
            .next();
        let now = Utc::now();
        let step = plan(migration, vmi.as_ref(), now);

        if let (Some(patch), Some(vmi)) = (&step.vmi_patch, &vmi) {
            let mut body = patch.clone();
            body["metadata"]["uid"] = vmi["metadata"]["uid"].clone();
            body["metadata"]["resourceVersion"] = vmi["metadata"]["resourceVersion"].clone();
            self.api
                .patch_merge(&format!("/apis/kubevirt.io/v1/namespaces/{ns}/virtualmachineinstances/{vmi_name}/status"), &body)
                .await?;
        }
        if !deleting || step.release_finalizer {
            if let Some(status) = desired_status(migration, vmi.as_ref(), &step, now) {
                let path = format!("/apis/kubevirt.io/v1/namespaces/{ns}/virtualmachineinstancemigrations/{name}/status");
                match self.api.patch_merge(&path, &json!({"status": status})).await {
                    Ok(_) if status["phase"] != migration["status"]["phase"] => {
                        info!("VirtualMachineInstanceMigration {ns}/{name} ({vmi_name}): {}", step.phase);
                        if let Some(why) = &step.failure {
                            warn!("VirtualMachineInstanceMigration {ns}/{name} failed: {why}");
                        }
                    }
                    Ok(_) => {}
                    // A deleting object's status may already be past writing.
                    Err(e) if deleting => debug!("{ns}/{name}: final status not written: {e}"),
                    Err(e) => return Err(e),
                }
            }
        }
        if step.release_finalizer {
            let finalizers = migration["metadata"]["finalizers"]
                .as_array()
                .map(|f| f.iter().filter(|f| *f != FINALIZER).cloned().collect())
                .unwrap_or_default();
            self.set_finalizers(migration, finalizers).await?;
        }
        if let Some(at) = step.requeue_at {
            apimachinery::reactor::requeue_at_time(at);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-05T12:00:00Z").unwrap().with_timezone(&Utc)
    }

    fn migration(phase: &str) -> Value {
        json!({
            "metadata": {"name": "m", "namespace": "ns", "uid": "m1",
                         "creationTimestamp": "2026-10-05T11:59:00Z",
                         "finalizers": [FINALIZER]},
            "spec": {"vmiName": "vm"},
            "status": {"phase": phase, "phaseTransitionTimestamps": [
                {"phase": "Scheduling", "phaseTransitionTimestamp": "2026-10-05T11:59:00Z"}]},
        })
    }

    fn vmi(state: Value) -> Value {
        json!({
            "metadata": {"name": "vm", "namespace": "ns", "uid": "v1", "labels": {NODE_LABEL: "a"}},
            "status": {"phase": "Running", "nodeName": "a", "migrationState": state},
        })
    }

    fn ours(extra: Value) -> Value {
        let mut s = json!({"migrationUid": "m1", "sourceNode": "a", "completed": false, "failed": false});
        for (k, v) in extra.as_object().unwrap() {
            s[k] = v.clone();
        }
        s
    }

    #[test]
    fn a_new_migration_claims_the_vmi_and_waits_for_a_target() {
        let p = plan(&migration(""), Some(&vmi(Value::Null)), now());
        assert_eq!(p.phase, "Scheduling");
        let state = &p.vmi_patch.unwrap()["status"]["migrationState"];
        assert_eq!(state["migrationUid"], "m1");
        assert_eq!(state["sourceNode"], "a");
        assert_eq!(state["mode"], "PreCopy");
        assert!(state["targetNode"].is_null());
        assert_eq!(state["completed"], false);
    }

    #[test]
    fn only_a_running_vmi_on_a_node_migrates() {
        assert_eq!(plan(&migration(""), None, now()).phase, "Failed");
        let mut v = vmi(Value::Null);
        v["status"]["phase"] = json!("Scheduling");
        let p = plan(&migration(""), Some(&v), now());
        assert_eq!(p.phase, "Failed");
        assert!(p.failure.unwrap().contains("not running"));
        let mut v = vmi(Value::Null);
        v["status"]["nodeName"] = json!("");
        assert_eq!(plan(&migration(""), Some(&v), now()).phase, "Failed");
    }

    #[test]
    fn a_second_migration_waits_for_the_first() {
        let busy = vmi(json!({"migrationUid": "other", "sourceNode": "a"}));
        let p = plan(&migration("Pending"), Some(&busy), now());
        assert_eq!(p, Plan { phase: "Pending", ..Default::default() });
        // Once that one is done, this one goes.
        let done = vmi(json!({"migrationUid": "other", "completed": true}));
        assert_eq!(plan(&migration("Pending"), Some(&done), now()).phase, "Scheduling");
    }

    #[test]
    fn phases_follow_the_vmi() {
        let m = migration("Scheduling");
        assert_eq!(plan(&m, Some(&vmi(ours(json!({})))), now()).phase, "Scheduling");
        let t = ours(json!({"targetNode": "b"}));
        assert_eq!(plan(&m, Some(&vmi(t)), now()).phase, "PreparingTarget");
        let t = ours(json!({"targetNode": "b", "targetNodeAddress": "10.0.0.2"}));
        assert_eq!(plan(&m, Some(&vmi(t)), now()).phase, "TargetReady");
        let t = ours(json!({"targetNode": "b", "targetNodeAddress": "10.0.0.2",
                            "startTimestamp": "2026-10-05T11:59:30Z"}));
        assert_eq!(plan(&m, Some(&vmi(t)), now()).phase, "Running");
    }

    #[test]
    fn success_moves_the_vmi_to_the_target() {
        let done = vmi(ours(json!({"targetNode": "b", "startTimestamp": "x", "completed": true})));
        let p = plan(&migration("Running"), Some(&done), now());
        assert_eq!(p.phase, "Succeeded");
        let patch = p.vmi_patch.unwrap();
        assert_eq!(patch["status"]["nodeName"], "b");
        assert_eq!(patch["metadata"]["labels"][NODE_LABEL], "b");
        // Already moved: nothing more to write.
        let mut moved = done.clone();
        moved["status"]["nodeName"] = json!("b");
        moved["metadata"]["labels"][NODE_LABEL] = json!("b");
        assert_eq!(plan(&migration("Running"), Some(&moved), now()).vmi_patch, None);
    }

    #[test]
    fn failure_is_the_sources_report() {
        let failed = vmi(ours(json!({"targetNode": "b", "failed": true, "failureReason": "qemu said no"})));
        let p = plan(&migration("Running"), Some(&failed), now());
        assert_eq!(p.phase, "Failed");
        assert_eq!(p.failure.as_deref(), Some("qemu said no"));
        assert_eq!(p.vmi_patch, None, "the VMI stays where it is");
    }

    #[test]
    fn a_vmi_that_goes_away_fails_the_migration() {
        assert_eq!(plan(&migration("Running"), None, now()).phase, "Failed");
        let other = vmi(json!({"migrationUid": "someone-else"}));
        assert_eq!(plan(&migration("PreparingTarget"), Some(&other), now()).phase, "Failed");
    }

    #[test]
    fn an_unschedulable_target_times_out() {
        let m = migration("Scheduling");
        let p = plan(&m, Some(&vmi(ours(json!({})))), now());
        assert_eq!(p.requeue_at, Some(now() + Duration::seconds(240)));
        let later = now() + Duration::seconds(300);
        let p = plan(&m, Some(&vmi(ours(json!({})))), later);
        assert_eq!(p.phase, "Failed");
        assert_eq!(p.vmi_patch.unwrap()["status"]["migrationState"]["failed"], true);
    }

    #[test]
    fn a_target_that_never_gets_ready_times_out() {
        let m = migration("PreparingTarget");
        let v = vmi(ours(json!({"targetNode": "b"})));
        assert_eq!(plan(&m, Some(&v), now()).phase, "PreparingTarget");
        let p = plan(&m, Some(&v), now() + Duration::seconds(900));
        assert_eq!(p.phase, "Failed");
        // A running migration is the source's to time out.
        let running = vmi(ours(json!({"targetNode": "b", "startTimestamp": "x"})));
        assert_eq!(plan(&m, Some(&running), now() + Duration::seconds(9000)).phase, "Running");
    }

    #[test]
    fn deleting_before_sending_aborts_on_the_vmi() {
        let mut m = migration("PreparingTarget");
        m["metadata"]["deletionTimestamp"] = json!("2026-10-05T12:00:00Z");
        let p = plan(&m, Some(&vmi(ours(json!({"targetNode": "b"})))), now());
        assert!(p.release_finalizer);
        assert_eq!(p.phase, "Failed");
        let state = &p.vmi_patch.unwrap()["status"]["migrationState"];
        assert_eq!(state["failed"], true);
        assert_eq!(state["abortRequested"], true);
        assert_eq!(state["abortStatus"], "Succeeded");
    }

    #[test]
    fn deleting_while_sending_asks_the_source_and_waits() {
        let mut m = migration("Running");
        m["metadata"]["deletionTimestamp"] = json!("2026-10-05T12:00:00Z");
        let sending = vmi(ours(json!({"targetNode": "b", "startTimestamp": "x"})));
        let p = plan(&m, Some(&sending), now());
        assert!(!p.release_finalizer);
        assert_eq!(p.vmi_patch.unwrap()["status"]["migrationState"]["abortRequested"], true);
        // Asked once.
        let asked = vmi(ours(json!({"targetNode": "b", "startTimestamp": "x", "abortRequested": true})));
        let p = plan(&m, Some(&asked), now());
        assert_eq!(p.vmi_patch, None);
        assert!(!p.release_finalizer);
        // The source answered.
        let answered = vmi(ours(json!({"targetNode": "b", "startTimestamp": "x", "failed": true})));
        assert!(plan(&m, Some(&answered), now()).release_finalizer);
        // It never answered.
        assert!(plan(&m, Some(&asked), now() + Duration::seconds(300)).release_finalizer);
    }

    #[test]
    fn deleting_a_finished_migration_just_lets_it_go() {
        let mut m = migration("Succeeded");
        m["metadata"]["deletionTimestamp"] = json!("2026-10-05T12:00:00Z");
        let p = plan(&m, Some(&vmi(ours(json!({"completed": true})))), now());
        assert_eq!(p, Plan { phase: "Succeeded", release_finalizer: true, ..Default::default() });
    }

    #[test]
    fn status_records_every_phase_passed_and_mirrors_the_vmi() {
        let m = migration("Scheduling");
        let v = vmi(ours(json!({"targetNode": "b", "targetNodeAddress": "10.0.0.2"})));
        let p = plan(&m, Some(&v), now());
        let st = desired_status(&m, Some(&v), &p, now()).unwrap();
        assert_eq!(st["phase"], "TargetReady");
        let phases: Vec<&str> = st["phaseTransitionTimestamps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["phase"].as_str().unwrap())
            .collect();
        assert_eq!(phases, ["Scheduling", "Scheduled", "PreparingTarget", "TargetReady"]);
        assert_eq!(st["migrationState"]["targetNode"], "b");
        // Written: the same pass again changes nothing.
        let mut written = m.clone();
        written["status"] = st;
        assert_eq!(desired_status(&written, Some(&v), &plan(&written, Some(&v), now()), now()), None);
    }

    #[test]
    fn a_controller_failure_is_recorded_on_the_migration() {
        let m = migration("");
        let p = plan(&m, None, now());
        let st = desired_status(&m, None, &p, now()).unwrap();
        assert_eq!(st["phase"], "Failed");
        assert_eq!(st["migrationState"]["failed"], true);
        assert!(st["migrationState"]["failureReason"].as_str().unwrap().contains("does not exist"));
    }
}
