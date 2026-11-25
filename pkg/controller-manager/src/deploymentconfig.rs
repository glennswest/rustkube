//! OpenShift DeploymentConfigs (#375, sc#151, sc#162): each version a
//! ReplicationController, rolled out by this process.
//!
//! Two controllers, as openshift-controller-manager's, with the deployer pod
//! folded in (the choice recorded on #375: no deployer image to ship):
//!
//! - [`DeploymentConfigController`] (its `deploymentconfig` controller):
//!   a ConfigChange trigger moves `status.latestVersion` on, the version's
//!   ReplicationController `<dc>-<version>` is made (OpenShift's annotations,
//!   the config encoded on it), an older rollout still running is cancelled,
//!   a finished one's replicas settle (the last complete version gets
//!   `spec.replicas`, every other 0), old versions past
//!   `revisionHistoryLimit` are deleted, and the status and conditions are
//!   computed as upstream's.
//! - [`Deployer`] (its `deployer` controller and `openshift-deploy`): a
//!   version that is New, Pending or Running is rolled out by a task of this
//!   process — Rolling (kubectl's rolling updater: surge, readiness, scale
//!   down) or Recreate (down to zero, up through one accepted pod), with the
//!   strategy's lifecycle hooks as hook pods `<rc>-hook-pre|mid|post` from
//!   the named container's image and `failurePolicy` applied. Its log, in
//!   the deployer's words, is recorded on the ReplicationController
//!   (`deploy.rustkube.io/log`), which is what `oc logs dc/…` reads. A
//!   cancelled version (`oc rollout cancel`, or a newer one) is stopped and
//!   marked Failed; `oc rollout retry` sets it New again and it reruns.
//!   A **Custom** strategy (#392) is OpenShift's deployer pod after all:
//!   `<rc>-deploy` from `customParams.image`, as the namespace's `deployer`
//!   account, which does the scaling itself through the API; the version's
//!   phase follows the pod's, and the pod's output is copied onto the
//!   ReplicationController when it ends.
//!
//! Ported from openshift-controller-manager `pkg/apps` and openshift/oc
//! `pkg/cli/deployer` (read 2026-11-23).

use crate::owned::{self, Controller, Deps};
use crate::runner::ApiClient;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{info, warn};

pub const DC_PATH: &str = "/apis/apps.openshift.io/v1/deploymentconfigs";
const RC_PATH: &str = "/api/v1/replicationcontrollers";

const CONFIG_ANNOTATION: &str = "openshift.io/deployment-config.name";
const VERSION_ANNOTATION: &str = "openshift.io/deployment-config.latest-version";
const PHASE_ANNOTATION: &str = "openshift.io/deployment.phase";
const ENCODED_CONFIG_ANNOTATION: &str = "openshift.io/encoded-deployment-config";
const DESIRED_REPLICAS_ANNOTATION: &str = "kubectl.kubernetes.io/desired-replicas";
const REPLICAS_ANNOTATION: &str = "openshift.io/deployment.replicas";
const STATUS_REASON_ANNOTATION: &str = "openshift.io/deployment.status-reason";
const CANCELLED_ANNOTATION: &str = "openshift.io/deployment.cancelled";
const DEPLOYMENT_ANNOTATION: &str = "openshift.io/deployment.name";
const IGNORE_POD_ANNOTATION: &str = "deploy.openshift.io/deployer-pod.ignore";
const DEPLOYER_POD_FOR_LABEL: &str = "openshift.io/deployer-pod-for.name";
const DEPLOYER_POD_TYPE_LABEL: &str = "openshift.io/deployer-pod.type";
const DC_LABEL: &str = "deploymentconfig";
const DEPLOYMENT_LABEL: &str = "deployment";
pub const LOG_ANNOTATION: &str = "deploy.rustkube.io/log";
/// The log kept on a ReplicationController: its tail past this.
const LOG_LIMIT: usize = 64 * 1024;
const MAX_DEPLOYMENT_DURATION: i64 = 21600;
const DEFAULT_TIMEOUT: i64 = 600;
const CANCELLED_BY_USER: &str = "cancelled by the user";
const CANCELLED_BY_NEWER: &str = "newer deployment was found running";
const DEPLOYER_POD_GONE: &str = "deployer pod no longer exists";
/// The account a Custom deployer pod runs as (#392).
const DEPLOYER_SERVICE_ACCOUNT: &str = "deployer";

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("")
}
fn ann<'a>(o: &'a Value, k: &str) -> &'a str {
    o["metadata"]["annotations"][k].as_str().unwrap_or("")
}
fn now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}
fn ns_of(o: &Value) -> String {
    s(&o["metadata"], "namespace").to_string()
}
fn name_of(o: &Value) -> String {
    s(&o["metadata"], "name").to_string()
}
fn version_of(rc: &Value) -> i64 {
    ann(rc, VERSION_ANNOTATION).parse().unwrap_or(0)
}
fn phase_of(rc: &Value) -> &str {
    ann(rc, PHASE_ANNOTATION)
}
fn is_terminated(rc: &Value) -> bool {
    matches!(phase_of(rc), "Complete" | "Failed")
}
fn is_cancelled(rc: &Value) -> bool {
    ann(rc, CANCELLED_ANNOTATION) == "true"
}
fn rc_name(config: &str, version: i64) -> String {
    format!("{config}-{version}")
}
fn spec_replicas(rc: &Value) -> i64 {
    rc["spec"]["replicas"].as_i64().unwrap_or(0)
}
fn status_count(rc: &Value, k: &str) -> i64 {
    rc["status"][k].as_i64().unwrap_or(0)
}

/// The DeploymentConfig a version was made from.
fn decode(rc: &Value) -> Option<Value> {
    serde_json::from_str(ann(rc, ENCODED_CONFIG_ANNOTATION)).ok()
}

/// `ResolveFenceposts`: surge rounded up, unavailable down, never both 0.
pub fn fenceposts(max_surge: &Value, max_unavailable: &Value, desired: i64) -> (i64, i64) {
    let resolve = |v: &Value, up: bool| match v {
        Value::Number(n) => n.as_i64().unwrap_or(0),
        Value::String(p) => {
            let p = p.trim_end_matches('%').parse::<i64>().unwrap_or(0) * desired;
            if up { (p + 99) / 100 } else { p / 100 }
        }
        _ => 0,
    };
    let surge = resolve(max_surge, true);
    let mut unavailable = resolve(max_unavailable, false);
    if surge == 0 && unavailable == 0 {
        unavailable = 1;
    }
    (surge, unavailable)
}

/// `MaxUnavailable`: a Rolling config's, else 0.
fn max_unavailable(dc: &Value) -> i64 {
    let st = &dc["spec"]["strategy"];
    if st["type"] != "Rolling" {
        return 0;
    }
    let p = &st["rollingParams"];
    fenceposts(&p["maxSurge"], &p["maxUnavailable"], dc["spec"]["replicas"].as_i64().unwrap_or(0)).1
}

/// `ActiveDeployment`: the latest complete version.
fn active(rcs: &[Value]) -> Option<&Value> {
    rcs.iter().filter(|r| phase_of(r) == "Complete").max_by_key(|r| version_of(r))
}

/// `MakeDeployment`: the version's ReplicationController.
pub fn make_deployment(dc: &Value) -> Value {
    let name = name_of(dc);
    let version = dc["status"]["latestVersion"].as_i64().unwrap_or(0);
    let rc = rc_name(&name, version);
    let mut labels = dc["metadata"]["labels"].as_object().cloned().unwrap_or_default();
    labels.insert(CONFIG_ANNOTATION.into(), json!(name));
    let mut selector = dc["spec"]["selector"].as_object().cloned().unwrap_or_default();
    selector.insert(DC_LABEL.into(), json!(name));
    selector.insert(DEPLOYMENT_LABEL.into(), json!(rc));
    let mut template = dc["spec"]["template"].clone();
    let mut pod_labels = template["metadata"]["labels"].as_object().cloned().unwrap_or_default();
    pod_labels.insert(DC_LABEL.into(), json!(name));
    pod_labels.insert(DEPLOYMENT_LABEL.into(), json!(rc));
    template["metadata"]["labels"] = json!(pod_labels);
    let mut pod_annotations = template["metadata"]["annotations"].as_object().cloned().unwrap_or_default();
    pod_annotations.insert(DEPLOYMENT_ANNOTATION.into(), json!(rc));
    pod_annotations.insert(CONFIG_ANNOTATION.into(), json!(name));
    pod_annotations.insert(VERSION_ANNOTATION.into(), json!(version.to_string()));
    template["metadata"]["annotations"] = json!(pod_annotations);
    for c in template["spec"]["containers"].as_array_mut().into_iter().flatten() {
        if let Some(i) = c["image"].as_str().map(|i| i.trim().to_string()) {
            c["image"] = json!(i);
        }
    }
    let mut annotations = serde_json::Map::new();
    annotations.insert(CONFIG_ANNOTATION.into(), json!(name));
    annotations.insert(ENCODED_CONFIG_ANNOTATION.into(), json!(dc.to_string()));
    annotations.insert(PHASE_ANNOTATION.into(), json!("New"));
    annotations.insert(VERSION_ANNOTATION.into(), json!(version.to_string()));
    annotations.insert(DESIRED_REPLICAS_ANNOTATION.into(),
                       json!(dc["spec"]["replicas"].as_i64().unwrap_or(0).to_string()));
    annotations.insert(REPLICAS_ANNOTATION.into(), json!("0"));
    if let Some(m) = dc["status"]["details"]["message"].as_str().filter(|m| !m.is_empty()) {
        annotations.insert(STATUS_REASON_ANNOTATION.into(), json!(m));
    }
    if let Some(v) = dc["metadata"]["annotations"][IGNORE_POD_ANNOTATION].as_str() {
        annotations.insert(IGNORE_POD_ANNOTATION.into(), json!(v));
    }
    json!({
        "apiVersion": "v1",
        "kind": "ReplicationController",
        "metadata": {
            "name": rc,
            "namespace": ns_of(dc),
            "labels": labels,
            "annotations": annotations,
            "ownerReferences": [{"apiVersion": "apps.openshift.io/v1", "kind": "DeploymentConfig", "name": name,
                                 "uid": dc["metadata"]["uid"], "controller": true, "blockOwnerDeletion": true}],
        },
        "spec": {
            "replicas": 0,
            "selector": selector,
            "minReadySeconds": dc["spec"]["minReadySeconds"].as_i64().unwrap_or(0),
            "template": template,
        },
    })
}

/// `SetDeploymentCondition`: replaced, unless it says the same.
fn set_condition(status: &mut Value, cond: Value) {
    let mut conds = status["conditions"].as_array().cloned().unwrap_or_default();
    if let Some(cur) = conds.iter().find(|c| c["type"] == cond["type"]) {
        if cur["status"] == cond["status"] && cur["reason"] == cond["reason"] {
            return;
        }
    }
    conds.retain(|c| c["type"] != cond["type"]);
    conds.push(cond);
    status["conditions"] = json!(conds);
}

fn condition(ty: &str, status: &str, reason: &str, message: &str) -> Value {
    let t = now();
    let mut c = json!({"type": ty, "status": status, "lastUpdateTime": t, "lastTransitionTime": t, "message": message});
    if !reason.is_empty() {
        c["reason"] = json!(reason);
    }
    c
}

/// `calculateStatus` and `updateConditions`.
pub fn calculate_status(dc: &Value, rcs: &[Value], update_observed: bool, extra: Option<Value>) -> Value {
    let latest = dc["status"]["latestVersion"].as_i64().unwrap_or(0);
    let latest_rc = rcs.iter().find(|r| name_of(r) == rc_name(&name_of(dc), latest));
    let sum = |k: &str| rcs.iter().map(|r| status_count(r, k)).sum::<i64>();
    let available = sum("availableReplicas");
    let total: i64 = rcs.iter().map(spec_replicas).sum();
    let observed = if update_observed {
        dc["metadata"]["generation"].clone()
    } else {
        dc["status"]["observedGeneration"].clone()
    };
    let mut status = json!({
        "latestVersion": latest,
        "observedGeneration": observed.as_i64().unwrap_or(0),
        "replicas": sum("replicas"),
        "updatedReplicas": latest_rc.map(|r| status_count(r, "replicas")).unwrap_or(0),
        "availableReplicas": available,
        "unavailableReplicas": (total - available).max(0),
        "readyReplicas": sum("readyReplicas"),
        "conditions": dc["status"]["conditions"].clone(),
    });
    if !dc["status"]["details"].is_null() {
        status["details"] = dc["status"]["details"].clone();
    }
    let want = dc["spec"]["replicas"].as_i64().unwrap_or(0);
    if available >= want - max_unavailable(dc) && available > 0 {
        set_condition(&mut status, condition("Available", "True", "", "Deployment config has minimum availability."));
    } else {
        set_condition(&mut status, condition("Available", "False", "", "Deployment config does not have minimum availability."));
    }
    if let Some(rc) = latest_rc {
        let n = name_of(rc);
        match phase_of(rc) {
            "Pending" => set_condition(&mut status, condition("Progressing", "Unknown", "",
                &format!("replication controller {n:?} is waiting for pod \"{n}-deploy\" to run"))),
            "Running" => {
                let old_old = dc["status"]["replicas"].as_i64().unwrap_or(0) - dc["status"]["updatedReplicas"].as_i64().unwrap_or(0);
                let new_old = status["replicas"].as_i64().unwrap_or(0) - status["updatedReplicas"].as_i64().unwrap_or(0);
                let progressing = status["updatedReplicas"].as_i64() > dc["status"]["updatedReplicas"].as_i64() || new_old < old_old;
                if progressing {
                    if let Some(c) = status["conditions"].as_array_mut() {
                        c.retain(|c| c["type"] != "Progressing");
                    }
                    set_condition(&mut status, condition("Progressing", "True", "ReplicationControllerUpdated",
                                                         &format!("replication controller {n:?} is progressing")));
                }
            }
            "Failed" => {
                let c = if is_cancelled(rc) {
                    condition("Progressing", "False", "RolloutCancelled", &format!("rollout of replication controller {n:?} was cancelled"))
                } else {
                    condition("Progressing", "False", "ProgressDeadlineExceeded", &format!("replication controller {n:?} has failed progressing"))
                };
                set_condition(&mut status, c);
            }
            "Complete" => set_condition(&mut status, condition("Progressing", "True", "NewReplicationControllerAvailable",
                                                               &format!("replication controller {n:?} successfully rolled out"))),
            _ => {}
        }
    }
    if let Some(c) = extra {
        set_condition(&mut status, c);
    }
    status
}

/// `RecordConfigChangeCause`.
fn config_change_cause() -> Value {
    json!({"message": "config change", "causes": [{"type": "ConfigChange"}]})
}

/// `HasUpdatedImages`: the automatic ImageChange triggers' last images that
/// the version's containers do not run.
fn updated_images(dc: &Value, rc: &Value) -> Vec<String> {
    let spec = &rc["spec"]["template"]["spec"];
    let running: Vec<&str> = ["containers", "initContainers"]
        .iter()
        .flat_map(|l| spec[*l].as_array().into_iter().flatten())
        .map(|c| s(c, "image"))
        .collect();
    let mut out = Vec::new();
    for t in dc["spec"]["triggers"].as_array().into_iter().flatten() {
        let p = &t["imageChangeParams"];
        if t["type"] != "ImageChange" || p["automatic"] != true {
            continue;
        }
        let last = s(p, "lastTriggeredImage");
        if last.is_empty() || running.contains(&last) || out.iter().any(|o| o == last) {
            continue;
        }
        out.push(last.to_string());
    }
    out
}

/// `triggerActivated`: the details of the version a trigger asks for. An
/// initial version waits for every ImageChange trigger's image; after that
/// an automatic trigger's image the latest version does not run fires
/// (cause ImageChange), then a ConfigChange trigger on a template change.
pub fn trigger_activated(dc: &Value, latest_rc: Option<&Value>) -> Option<Value> {
    let triggers = dc["spec"]["triggers"].as_array()?;
    let images: Vec<&Value> = triggers.iter().filter(|t| t["type"] == "ImageChange").collect();
    let config = triggers.iter().any(|t| t["type"] == "ConfigChange");
    if dc["spec"]["paused"] == true || (images.is_empty() && !config) {
        return None;
    }
    if dc["status"]["latestVersion"].as_i64().unwrap_or(0) == 0 {
        if !images.is_empty() {
            let resolved = images.iter().all(|t| !s(&t["imageChangeParams"], "lastTriggeredImage").is_empty());
            return resolved.then(config_change_cause);
        }
        return Some(config_change_cause());
    }
    let rc = latest_rc?;
    if !images.is_empty() {
        let updated = updated_images(dc, rc);
        if !updated.is_empty() {
            let causes: Vec<Value> = updated
                .iter()
                .map(|i| json!({"type": "ImageChange", "imageTrigger": {"from": {"kind": "DockerImage", "name": i}}}))
                .collect();
            return Some(json!({"message": "image change", "causes": causes}));
        }
    }
    if config && decode(rc).is_none_or(|d| d["spec"]["template"] != dc["spec"]["template"]) {
        return Some(config_change_cause());
    }
    None
}

/// `DeploymentsForCleanup`: what `revisionHistoryLimit` counts, oldest first.
fn for_cleanup<'a>(dc: &Value, rcs: &'a [Value]) -> Vec<&'a Value> {
    let mut sorted: Vec<&Value> = rcs.iter().collect();
    sorted.sort_by_key(|r| version_of(r));
    let latest = dc["status"]["latestVersion"].as_i64().unwrap_or(0);
    match active(rcs) {
        None => sorted.into_iter().filter(|r| version_of(r) != latest).collect(),
        Some(a) => {
            let av = version_of(a);
            sorted.into_iter().filter(|r| name_of(r) != name_of(a) && version_of(r) < av).collect()
        }
    }
}

pub struct DeploymentConfigController {
    api: Arc<ApiClient>,
}

impl DeploymentConfigController {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self { api }
    }

    pub async fn run(&self) {
        owned::run(&self.api, self).await;
    }

    fn dc_path(dc: &Value) -> String {
        format!("/apis/apps.openshift.io/v1/namespaces/{}/deploymentconfigs/{}", ns_of(dc), name_of(dc))
    }

    async fn update_status(&self, dc: &Value, rcs: &[Value], update_observed: bool, extra: Option<Value>) -> anyhow::Result<()> {
        let status = calculate_status(dc, rcs, update_observed, extra);
        if status == dc["status"] {
            return Ok(());
        }
        let mut body = dc.clone();
        body["status"] = status;
        self.api.update_status(&Self::dc_path(dc), &body).await?;
        Ok(())
    }

    /// A trigger fired: the next version, with its cause.
    async fn bump(&self, dc: &Value, details: Value) -> anyhow::Result<()> {
        let mut body = dc.clone();
        let v = dc["status"]["latestVersion"].as_i64().unwrap_or(0) + 1;
        body["status"]["latestVersion"] = json!(v);
        let why = s(&details, "message").to_string();
        body["status"]["details"] = details;
        self.api.update_status(&Self::dc_path(dc), &body).await?;
        info!("DeploymentConfig {}/{}: version {v} ({why})", ns_of(dc), name_of(dc));
        Ok(())
    }

    async fn cleanup(&self, dc: &Value, rcs: &[Value]) {
        let Some(limit) = dc["spec"]["revisionHistoryLimit"].as_i64() else { return };
        let prunable = for_cleanup(dc, rcs);
        let excess = prunable.len().saturating_sub(limit.max(0) as usize);
        for rc in prunable.into_iter().take(excess) {
            if spec_replicas(rc) != 0 {
                continue;
            }
            let path = format!("/api/v1/namespaces/{}/replicationcontrollers/{}", ns_of(rc), name_of(rc));
            if let Err(e) = self.api.delete(&path).await {
                warn!("DeploymentConfig {}/{}: deleting old version {}: {e}", ns_of(dc), name_of(dc), name_of(rc));
            }
        }
    }

    async fn scale(&self, rc: &Value, replicas: i64) -> anyhow::Result<Value> {
        let mut body = rc.clone();
        body["spec"]["replicas"] = json!(replicas);
        let path = format!("/api/v1/namespaces/{}/replicationcontrollers/{}", ns_of(rc), name_of(rc));
        self.api.update(&path, &body).await
    }
}

#[async_trait::async_trait]
impl Controller for DeploymentConfigController {
    fn name(&self) -> &'static str {
        "deploymentconfig"
    }
    fn primary(&self) -> &str {
        DC_PATH
    }
    fn children(&self) -> Option<&'static str> {
        Some(RC_PATH)
    }
    async fn reconcile(&self, dc: &Value, children: &[Value], _deps: &Deps) -> anyhow::Result<()> {
        let rcs: Vec<Value> = children.to_vec();
        let name = name_of(dc);
        let latest = dc["status"]["latestVersion"].as_i64().unwrap_or(0);
        let triggers = dc["spec"]["triggers"].as_array().cloned().unwrap_or_default();
        if latest == 0 && triggers.is_empty() {
            return self.update_status(dc, &rcs, true, None).await;
        }
        if !dc["metadata"]["deletionTimestamp"].is_null() {
            return self.update_status(dc, &rcs, true, None).await;
        }
        if dc["spec"]["paused"] == true {
            self.cleanup(dc, &rcs).await;
            return self.update_status(dc, &rcs, true, None).await;
        }
        let latest_rc = rcs.iter().find(|r| name_of(r) == rc_name(&name, latest)).cloned();
        if latest_rc.is_none() {
            // An older rollout still running is cancelled first.
            let mut waiting = false;
            for rc in rcs.iter().filter(|r| !is_terminated(r)) {
                waiting = true;
                if is_cancelled(rc) {
                    continue;
                }
                let mut body = rc.clone();
                body["metadata"]["annotations"][CANCELLED_ANNOTATION] = json!("true");
                body["metadata"]["annotations"][STATUS_REASON_ANNOTATION] = json!(CANCELLED_BY_NEWER);
                let path = format!("/api/v1/namespaces/{}/replicationcontrollers/{}", ns_of(rc), name_of(rc));
                self.api.update(&path, &body).await?;
            }
            if waiting {
                apimachinery::reactor::requeue_after(Duration::from_secs(1));
                return Ok(());
            }
        }
        let empty_image = dc["spec"]["template"]["spec"]["containers"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|c| s(c, "image").trim().is_empty());
        if empty_image {
            return self.update_status(dc, &rcs, true, None).await;
        }
        if let Some(details) = trigger_activated(dc, latest_rc.as_ref()) {
            return self.bump(dc, details).await;
        }
        if let Some(rc) = latest_rc {
            if !is_terminated(&rc) {
                return self.update_status(dc, &rcs, false, None).await;
            }
            // reconcileDeployments: the active version runs, every other is 0.
            let active_name = active(&rcs).map(name_of);
            let mut updated = Vec::new();
            for rc in &rcs {
                let is_active = active_name.as_deref() == Some(name_of(rc).as_str());
                let want = if is_active && dc["spec"]["test"] != true { dc["spec"]["replicas"].as_i64().unwrap_or(0) } else { 0 };
                if want != spec_replicas(rc) {
                    updated.push(self.scale(rc, want).await?);
                } else {
                    updated.push(rc.clone());
                }
            }
            self.cleanup(dc, &updated).await;
            return self.update_status(dc, &updated, true, None).await;
        }
        let deployment = make_deployment(dc);
        let path = format!("/api/v1/namespaces/{}/replicationcontrollers", ns_of(dc));
        match self.api.create(&path, &deployment).await {
            Ok(created) => {
                let msg = format!("Created new replication controller {:?} for version {latest}", name_of(&created));
                info!("DeploymentConfig {}/{name}: {msg}", ns_of(dc));
                let mut all = rcs.clone();
                all.push(created);
                self.cleanup(dc, &all).await;
                let cond = condition("Progressing", "True", "NewReplicationControllerCreated", &msg);
                self.update_status(dc, &all, true, Some(cond)).await
            }
            Err(e) => {
                let cond = condition("Progressing", "False", "ReplicationControllerCreateError", &e.to_string());
                let _ = self.update_status(dc, &rcs, true, Some(cond)).await;
                Err(e.context(format!("couldn't create deployment for deployment config {}/{name}", ns_of(dc))))
            }
        }
    }
}

// --- the deployer ---------------------------------------------------------------------

/// Running rollouts, by ReplicationController uid. Dropping it (the
/// controller's term ends: leadership lost) stops them all; the next term
/// starts each again from its record, as a new deployer pod would.
#[derive(Default)]
struct Rollouts(Mutex<HashMap<String, tokio::task::JoinHandle<()>>>);

impl Drop for Rollouts {
    fn drop(&mut self) {
        for (_, h) in self.0.lock().unwrap().drain() {
            h.abort();
        }
    }
}

pub struct Deployer {
    api: Arc<ApiClient>,
    rollouts: Rollouts,
}

impl Deployer {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self { api, rollouts: Rollouts::default() }
    }

    pub async fn run(&self) {
        owned::run(&self.api, self).await;
    }

    fn stop(&self, uid: &str) {
        if let Some(h) = self.rollouts.0.lock().unwrap().remove(uid) {
            h.abort();
        }
    }
}

#[async_trait::async_trait]
impl Controller for Deployer {
    fn name(&self) -> &'static str {
        "deployer"
    }
    fn primary(&self) -> &str {
        RC_PATH
    }
    async fn deleted(&self, key: &apimachinery::informer::Key, _children: &[Value], _deps: &Deps) -> anyhow::Result<()> {
        self.stop(&key.uid);
        Ok(())
    }
    async fn reconcile(&self, rc: &Value, _children: &[Value], _deps: &Deps) -> anyhow::Result<()> {
        if ann(rc, CONFIG_ANNOTATION).is_empty() || ann(rc, ENCODED_CONFIG_ANNOTATION).is_empty() {
            return Ok(());
        }
        let uid = s(&rc["metadata"], "uid").to_string();
        if is_terminated(rc) || !rc["metadata"]["deletionTimestamp"].is_null() {
            self.stop(&uid);
            return Ok(());
        }
        let rollout = Rollout::new(self.api.clone(), rc);
        if is_cancelled(rc) {
            self.stop(&uid);
            rollout.cancelled(rc).await?;
            return Ok(());
        }
        let mut running = self.rollouts.0.lock().unwrap();
        if running.get(&uid).is_some_and(|h| !h.is_finished()) {
            return Ok(());
        }
        let fresh = matches!(phase_of(rc), "New" | "Pending" | "");
        let text = if fresh { String::new() } else { ann(rc, LOG_ANNOTATION).to_string() };
        running.insert(uid, tokio::spawn(async move { rollout.run(text).await }));
        Ok(())
    }
}

/// One version's rollout: OpenShift's deployer, against the API.
struct Rollout {
    api: Arc<ApiClient>,
    ns: String,
    name: String,
    log: tokio::sync::Mutex<String>,
    started: tokio::time::Instant,
}

impl Rollout {
    fn new(api: Arc<ApiClient>, rc: &Value) -> Self {
        Self {
            api,
            ns: ns_of(rc),
            name: name_of(rc),
            log: tokio::sync::Mutex::new(String::new()),
            started: tokio::time::Instant::now(),
        }
    }

    fn rc_path(&self, name: &str) -> String {
        format!("/api/v1/namespaces/{}/replicationcontrollers/{name}", self.ns)
    }

    async fn get(&self, path: &str) -> anyhow::Result<Option<Value>> {
        let resp = self.api.get(path).await?;
        if resp.status().as_u16() == 404 {
            return Ok(None);
        }
        anyhow::ensure!(resp.status().is_success(), "GET {path}: {}", resp.status());
        Ok(Some(resp.json().await?))
    }

    async fn rc(&self, name: &str) -> anyhow::Result<Value> {
        self.get(&self.rc_path(name)).await?.ok_or_else(|| anyhow::anyhow!("replicationcontrollers {name:?} not found"))
    }

    /// Annotations on this version's ReplicationController.
    async fn annotate(&self, annotations: Value) -> anyhow::Result<()> {
        self.api.patch_merge(&self.rc_path(&self.name), &json!({"metadata": {"annotations": annotations}})).await?;
        Ok(())
    }

    /// A line of the deployer's log, recorded at once.
    async fn say(&self, line: &str) {
        let text = {
            let mut log = self.log.lock().await;
            log.push_str(line);
            log.push('\n');
            if log.len() > LOG_LIMIT {
                let cut = log.len() - LOG_LIMIT;
                let cut = (cut..log.len()).find(|i| log.is_char_boundary(*i)).unwrap_or(log.len());
                log.drain(..cut);
            }
            log.clone()
        };
        if let Err(e) = self.annotate(json!({LOG_ANNOTATION: text})).await {
            warn!("rollout {}/{}: recording its log: {e}", self.ns, self.name);
        }
    }

    /// Cancelled (`oc rollout cancel`, or a newer version): Failed, its hook
    /// pods gone, as OpenShift's deployer controller does with the deployer
    /// pod's.
    async fn cancelled(&self, rc: &Value) -> anyhow::Result<()> {
        self.delete_hook_pods().await;
        let reason = match ann(rc, STATUS_REASON_ANNOTATION) {
            "" => CANCELLED_BY_USER,
            r => r,
        };
        *self.log.lock().await = ann(rc, LOG_ANNOTATION).to_string();
        self.say(&format!("--> Rollout cancelled: {reason}")).await;
        self.annotate(json!({PHASE_ANNOTATION: "Failed", STATUS_REASON_ANNOTATION: reason})).await
    }

    async fn delete_hook_pods(&self) {
        let path = format!("/api/v1/namespaces/{}/pods?labelSelector={}%3D{}", self.ns, DEPLOYER_POD_FOR_LABEL, self.name);
        let Ok(list) = self.api.list(&path).await else { return };
        for p in list["items"].as_array().into_iter().flatten() {
            let _ = self.api.delete(&format!("/api/v1/namespaces/{}/pods/{}", self.ns, name_of(p))).await;
        }
    }

    async fn run(self, text: String) {
        *self.log.lock().await = text;
        let current = self.rc(&self.name).await.ok();
        if let Some((rc, dc)) = current.as_ref().and_then(|rc| decode(rc).map(|dc| (rc, dc))) {
            if dc["spec"]["strategy"]["type"] == "Custom" {
                return self.custom(rc, &dc).await;
            }
        }
        if let Err(e) = self.annotate(json!({PHASE_ANNOTATION: "Running"})).await {
            warn!("rollout {}/{}: marking it Running: {e}", self.ns, self.name);
            return;
        }
        let deadline = match self.rc(&self.name).await.ok().as_ref().and_then(decode) {
            Some(dc) => dc["spec"]["strategy"]["activeDeadlineSeconds"].as_i64().unwrap_or(MAX_DEPLOYMENT_DURATION),
            None => MAX_DEPLOYMENT_DURATION,
        };
        let result = match tokio::time::timeout(Duration::from_secs(deadline.max(1) as u64), self.deploy()).await {
            Ok(r) => r,
            Err(_) => Err(anyhow::anyhow!("the rollout exceeded its active deadline of {deadline}s")),
        };
        let phase = match result {
            Ok(()) => {
                self.say("--> Success").await;
                "Complete"
            }
            Err(e) => {
                self.say(&format!("error: {e:#}")).await;
                "Failed"
            }
        };
        info!("rollout {}/{}: {phase}", self.ns, self.name);
        if let Err(e) = self.annotate(json!({PHASE_ANNOTATION: phase})).await {
            warn!("rollout {}/{}: marking it {phase}: {e}", self.ns, self.name);
        }
    }

    /// A Custom version (#392): OpenShift's deployer controller with the
    /// deployer pod `<rc>-deploy` from the user's image. The pod does the
    /// rollout; the version's phase follows it (Pending, Running, then
    /// Complete or Failed), as `deploymentStatusFor` maps a pod's phase.
    async fn custom(&self, rc: &Value, dc: &Value) {
        let pod = deployer_pod(rc, dc);
        let pod_name = name_of(&pod);
        let path = format!("/api/v1/namespaces/{}/pods/{pod_name}", self.ns);
        let deadline = dc["spec"]["strategy"]["activeDeadlineSeconds"].as_i64().unwrap_or(MAX_DEPLOYMENT_DURATION);
        // The pod's own deadline ends it once it has started; this one also
        // covers a pod that never starts (an image that cannot be pulled).
        let until = self.started + Duration::from_secs(deadline.max(1) as u64 + 30);
        let mut phase = phase_of(rc).to_string();
        let result: anyhow::Result<(&str, Value)> = async {
            if self.get(&path).await?.is_none() {
                self.say(&format!("--> Running deployer pod {pod_name} ({})", s(&pod["spec"]["containers"][0], "image"))).await;
                let pods = format!("/api/v1/namespaces/{}/pods", self.ns);
                if let Err(e) = self.api.create(&pods, &pod).await {
                    if self.get(&path).await?.is_none() {
                        anyhow::bail!("couldn't create deployer pod for {}/{}: {e}", self.ns, self.name);
                    }
                }
            }
            loop {
                let Some(p) = self.get(&path).await? else {
                    anyhow::bail!(DEPLOYER_POD_GONE);
                };
                let next = match p["status"]["phase"].as_str().unwrap_or("") {
                    "Succeeded" => return Ok(("Complete", p)),
                    "Failed" => return Ok(("Failed", p)),
                    "Running" => "Running",
                    _ => "Pending",
                };
                if next != phase {
                    self.annotate(json!({PHASE_ANNOTATION: next})).await?;
                    phase = next.to_string();
                }
                if tokio::time::Instant::now() >= until {
                    let _ = self.api.delete(&path).await;
                    anyhow::bail!("the rollout exceeded its active deadline of {deadline}s");
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
        .await;
        let (phase, reason) = match result {
            Ok((phase, p)) => {
                // The image's own output, kept with the version once the pod
                // has ended: `oc logs dc/…` reads the pod while it is there.
                match self.api.get(&format!("{path}/log")).await {
                    Ok(r) if r.status().is_success() => {
                        let text = r.text().await.unwrap_or_default();
                        for line in text.lines() {
                            self.say(line).await;
                        }
                    }
                    Ok(r) => self.say(&format!("warning: Unable to retrieve deployer logs from {pod_name}: {}", r.status())).await,
                    Err(e) => self.say(&format!("warning: Unable to retrieve deployer logs from {pod_name}: {e}")).await,
                }
                let reason = if phase == "Failed" {
                    let why = p["status"]["message"].as_str().filter(|m| !m.is_empty())
                        .or(p["status"]["containerStatuses"][0]["state"]["terminated"]["reason"].as_str())
                        .unwrap_or("the deployer pod failed");
                    self.say(&format!("error: deployer pod {pod_name} failed: {why}")).await;
                    why.to_string()
                } else {
                    String::new()
                };
                (phase, reason)
            }
            Err(e) => {
                self.say(&format!("error: {e:#}")).await;
                ("Failed", format!("{e:#}"))
            }
        };
        info!("rollout {}/{}: {phase} (Custom)", self.ns, self.name);
        let mut a = json!({PHASE_ANNOTATION: phase});
        if !reason.is_empty() {
            a[STATUS_REASON_ANNOTATION] = json!(reason);
        }
        if let Err(e) = self.annotate(a).await {
            warn!("rollout {}/{}: marking it {phase}: {e}", self.ns, self.name);
        }
    }

    /// `Deployer.Deploy`: the version to roll from, older ones down, the
    /// strategy.
    async fn deploy(&self) -> anyhow::Result<()> {
        let to = self.rc(&self.name).await?;
        let dc = decode(&to).ok_or_else(|| anyhow::anyhow!("couldn't decode deployment config from deployment {}", self.name))?;
        let desired: i64 = ann(&to, DESIRED_REPLICAS_ANNOTATION)
            .parse()
            .map_err(|_| anyhow::anyhow!("deployment {} has already run to completion", self.name))?;
        let config = ann(&to, CONFIG_ANNOTATION).to_string();
        let list = self
            .api
            .list(&format!("/api/v1/namespaces/{}/replicationcontrollers?labelSelector={}%3D{}", self.ns,
                           CONFIG_ANNOTATION.replace('/', "%2F"), config))
            .await?;
        let mut rcs: Vec<Value> = list["items"].as_array().cloned().unwrap_or_default();
        rcs.sort_by_key(|r| std::cmp::Reverse(version_of(r)));
        let from = rcs.iter().find(|r| name_of(r) != self.name && phase_of(r) == "Complete").cloned();
        if let Some(f) = &from {
            anyhow::ensure!(version_of(&to) >= version_of(f), "deployment {} is older than {}", self.name, name_of(f));
        }
        for c in &rcs {
            let n = name_of(c);
            if n == self.name || from.as_ref().is_some_and(|f| name_of(f) == n) || spec_replicas(c) == 0 {
                continue;
            }
            match self.scale_and_wait(&n, 0, 120).await {
                Ok(_) => self.say(&format!("--> Scaled older deployment {n} down")).await,
                Err(e) => self.say(&format!("error: Couldn't scale down prior deployment {}/{n}: {e}", self.ns)).await,
            }
        }
        let st = &dc["spec"]["strategy"];
        match s(st, "type") {
            "Recreate" => {
                let p = &st["recreateParams"];
                let timeout = p["timeoutSeconds"].as_i64().unwrap_or(DEFAULT_TIMEOUT);
                self.recreate(from.as_ref(), desired, timeout, &p["pre"], &p["mid"], &p["post"], &dc).await
            }
            "Rolling" => self.rolling(from.as_ref(), desired, &dc).await,
            other => anyhow::bail!("unsupported strategy type {other:?}"),
        }
    }

    /// `RecreateDeploymentStrategy.DeployWithAcceptor`.
    #[allow(clippy::too_many_arguments)]
    async fn recreate(&self, from: Option<&Value>, desired: i64, timeout: i64, pre: &Value, mid: &Value, post: &Value,
                      dc: &Value) -> anyhow::Result<()> {
        let min_ready = dc["spec"]["minReadySeconds"].as_i64().unwrap_or(0);
        self.hook(pre, "hook-pre", "pre", dc).await.map_err(|e| anyhow::anyhow!("pre hook failed: {e}"))?;
        if let Some(f) = from {
            let n = name_of(f);
            self.say(&format!("--> Scaling {n} down to zero")).await;
            self.scale_and_wait(&n, 0, timeout).await.map_err(|e| anyhow::anyhow!("couldn't scale {n} to 0: {e}"))?;
            self.wait_terminated(&n, timeout).await;
        }
        self.hook(mid, "hook-mid", "mid", dc).await.map_err(|e| anyhow::anyhow!("mid hook failed: {e}"))?;
        if desired > 0 {
            let mut accepted = false;
            let mut current = self.rc(&self.name).await?;
            if from.is_some() {
                self.say(&format!("--> Scaling {} to 1 before performing acceptance check", self.name)).await;
                current = self.scale_and_wait(&self.name, 1, timeout).await
                    .map_err(|e| anyhow::anyhow!("couldn't scale {} to 1: {e}", self.name))?;
                self.accept(timeout, min_ready).await
                    .map_err(|e| anyhow::anyhow!("update acceptor rejected {}: {e}", self.name))?;
                accepted = true;
            }
            if spec_replicas(&current) != desired {
                self.say(&format!("--> Scaling {} to {desired}", self.name)).await;
                self.scale_and_wait(&self.name, desired, timeout).await
                    .map_err(|e| anyhow::anyhow!("couldn't scale {} to {desired}: {e}", self.name))?;
            }
            if !accepted {
                self.accept(timeout, min_ready).await
                    .map_err(|e| anyhow::anyhow!("update acceptor rejected {}: {e}", self.name))?;
            }
        }
        self.hook(post, "hook-post", "post", dc).await.map_err(|e| anyhow::anyhow!("post hook failed: {e}"))
    }

    /// `RollingDeploymentStrategy.Deploy` and kubectl's rolling updater.
    async fn rolling(&self, from: Option<&Value>, desired: i64, dc: &Value) -> anyhow::Result<()> {
        let p = &dc["spec"]["strategy"]["rollingParams"];
        let timeout = p["timeoutSeconds"].as_i64().unwrap_or(DEFAULT_TIMEOUT);
        let Some(from) = from else {
            // The first version: Recreate's, with Rolling's hooks around it.
            self.hook(&p["pre"], "hook-pre", "pre", dc).await.map_err(|e| anyhow::anyhow!("pre hook failed: {e}"))?;
            self.recreate(None, desired, timeout, &Value::Null, &Value::Null, &Value::Null, dc).await?;
            return self.hook(&p["post"], "hook-post", "post", dc).await.map_err(|e| anyhow::anyhow!("post hook failed: {e}"));
        };
        self.hook(&p["pre"], "hook-pre", "pre", dc).await.map_err(|e| anyhow::anyhow!("pre hook failed: {e}"))?;
        let interval = p["intervalSeconds"].as_i64().unwrap_or(1).max(1) as u64;
        let period = p["updatePeriodSeconds"].as_i64().unwrap_or(1).max(1) as u64;
        let old_name = name_of(from);
        let mut old = spec_replicas(&self.rc(&old_name).await?);
        let mut new = spec_replicas(&self.rc(&self.name).await?);
        let (surge, mut unavailable) = fenceposts(&p["maxSurge"], &p["maxUnavailable"], desired);
        anyhow::ensure!(!(desired > 0 && unavailable == 0 && surge == 0), "one of maxSurge or maxUnavailable must be specified");
        let mut min_available = (desired - unavailable).max(0);
        if desired == 0 {
            // Everything old may go at once.
            unavailable = old;
            min_available = 0;
        }
        tracing::debug!("rollout {}/{}: maxUnavailable {unavailable}", self.ns, self.name);
        let mut first = true;
        let mut say = |line: String| {
            let prefix = if first { "-->" } else { "   " };
            first = false;
            format!("{prefix} {line}")
        };
        self.say(&say(format!(
            "Scaling up {} from {new} to {desired}, scaling down {old_name} from {old} to 0 (keep {min_available} pods available, don't exceed {} pods)",
            self.name, desired + surge
        ))).await;
        let mut deadline = tokio::time::Instant::now() + Duration::from_secs(timeout as u64);
        while new != desired || old != 0 {
            let (new_before, old_before) = (new, old);
            // scaleUp
            if new != desired {
                let mut increment = (desired + surge) - (old + new);
                if old == 0 {
                    increment = desired - new;
                }
                if increment > 0 {
                    new = (new + increment).min(desired);
                    self.say(&say(format!("Scaling {} up to {new}", self.name))).await;
                    self.scale_and_wait(&self.name, new, timeout).await?;
                }
            }
            tokio::time::sleep(Duration::from_secs(period)).await;
            // scaleDown
            if old != 0 {
                let new_available = status_count(&self.rc(&self.name).await?, "availableReplicas");
                let decrement = (old + new) - min_available - (new - new_available);
                if decrement > 0 {
                    old = (old - decrement).max(0);
                    if new == desired && new_available == desired {
                        old = 0;
                    }
                    self.say(&say(format!("Scaling {old_name} down to {old}"))).await;
                    self.scale_and_wait(&old_name, old, timeout).await?;
                }
            }
            if new != new_before || old != old_before {
                deadline = tokio::time::Instant::now() + Duration::from_secs(timeout as u64);
            } else if tokio::time::Instant::now() > deadline {
                anyhow::bail!("timed out waiting for any update progress to be made");
            } else {
                tokio::time::sleep(Duration::from_secs(interval)).await;
            }
        }
        self.hook(&p["post"], "hook-post", "post", dc).await.map_err(|e| anyhow::anyhow!("post hook failed: {e}"))
    }

    /// Scale a version and wait until its status reports that many.
    async fn scale_and_wait(&self, name: &str, replicas: i64, timeout: i64) -> anyhow::Result<Value> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout.max(1) as u64);
        loop {
            let mut rc = self.rc(name).await?;
            if spec_replicas(&rc) == replicas {
                break;
            }
            rc["spec"]["replicas"] = json!(replicas);
            match self.api.update(&self.rc_path(name), &rc).await {
                Ok(_) => break,
                Err(_) if tokio::time::Instant::now() < deadline => tokio::time::sleep(Duration::from_secs(1)).await,
                Err(e) => return Err(e),
            }
        }
        loop {
            let rc = self.rc(name).await?;
            if status_count(&rc, "replicas") == replicas {
                return Ok(rc);
            }
            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!("timed out waiting for the condition");
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    /// The update acceptor: every replica of this version available.
    async fn accept(&self, timeout: i64, _min_ready: i64) -> anyhow::Result<()> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout.max(1) as u64);
        loop {
            let rc = self.rc(&self.name).await?;
            if status_count(&rc, "availableReplicas") == spec_replicas(&rc) {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!("pods for rc '{}/{}' took longer than {timeout} seconds to become available", self.ns, self.name);
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    /// A version's pods gone, after it was scaled to zero.
    async fn wait_terminated(&self, rc: &str, timeout: i64) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout.max(1) as u64);
        let path = format!("/api/v1/namespaces/{}/pods?labelSelector={DEPLOYMENT_LABEL}%3D{rc}", self.ns);
        while tokio::time::Instant::now() < deadline {
            let Ok(list) = self.api.list(&path).await else { return };
            let running = list["items"].as_array().into_iter().flatten()
                .any(|p| !matches!(p["status"]["phase"].as_str(), Some("Succeeded" | "Failed")));
            if !running {
                return;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    /// A lifecycle hook, with its failure policy applied.
    async fn hook(&self, hook: &Value, suffix: &str, label: &str, dc: &Value) -> anyhow::Result<()> {
        let result = if hook["tagImages"].as_array().is_some_and(|t| !t.is_empty()) {
            self.tag_images(hook, label).await
        } else if hook["execNewPod"].is_object() {
            self.exec_new_pod(hook, suffix, label, dc).await
        } else {
            return Ok(());
        };
        match (result, s(hook, "failurePolicy")) {
            (Ok(()), _) => Ok(()),
            (Err(e), "Ignore") => {
                warn!("rollout {}/{}: the {label}-hook failed: {e} (ignore)", self.ns, self.name);
                Ok(())
            }
            (Err(e), _) => anyhow::bail!("the {label} hook failed: {e}, aborting rollout of {}/{}", self.ns, self.name),
        }
    }

    /// `tagImages` (#386): each named container's image, as this version
    /// runs it, tagged into an ImageStreamTag — a PUT of the tag, which the
    /// registry takes as an upsert into an existing stream, else a create.
    async fn tag_images(&self, hook: &Value, label: &str) -> anyhow::Result<()> {
        let rc = self.rc(&self.name).await?;
        let mut errs = Vec::new();
        for action in hook["tagImages"].as_array().into_iter().flatten() {
            let container = s(action, "containerName");
            let image = rc["spec"]["template"]["spec"]["containers"].as_array().into_iter().flatten()
                .find(|c| c["name"] == container)
                .map(|c| s(c, "image").to_string());
            let Some(image) = image else {
                errs.push(format!("unable to find image for container {container:?}, container could not be found"));
                continue;
            };
            let to = &action["to"];
            let ns = match s(to, "namespace") {
                "" => self.ns.clone(),
                n => n.to_string(),
            };
            let name = s(to, "name");
            let body = json!({"kind": "ImageStreamTag", "apiVersion": "image.openshift.io/v1",
                              "metadata": {"name": name, "namespace": ns},
                              "tag": {"name": imagestream_tag(name), "from": {"kind": "DockerImage", "name": image}}});
            let base = format!("/apis/image.openshift.io/v1/namespaces/{ns}/imagestreamtags");
            let result = match self.api.update(&format!("{base}/{name}"), &body).await {
                Err(e) if apimachinery::api_status::status_of(&e).is_some_and(|s| s.as_u16() == 404) => {
                    self.api.create(&base, &body).await
                }
                r => r,
            };
            match result {
                Ok(_) => self.say(&format!("--> {label}: Tagged {image:?} into {ns}/{name}")).await,
                Err(e) => errs.push(format!("{e:#}")),
            }
        }
        if errs.is_empty() { Ok(()) } else { anyhow::bail!("{}", errs.join("; ")) }
    }

    async fn exec_new_pod(&self, hook: &Value, suffix: &str, label: &str, dc: &Value) -> anyhow::Result<()> {
        let rc = self.rc(&self.name).await?;
        let elapsed = self.started.elapsed().as_secs() as i64;
        let deadline = dc["spec"]["strategy"]["activeDeadlineSeconds"].as_i64().unwrap_or(MAX_DEPLOYMENT_DURATION) - elapsed;
        let pod = hook_pod(hook, &rc, &dc["spec"]["strategy"], suffix, deadline.max(1))?;
        let pod_name = name_of(&pod);
        let pods = format!("/api/v1/namespaces/{}/pods", self.ns);
        let mut existed = false;
        if let Err(e) = self.api.create(&pods, &pod).await {
            if self.get(&format!("{pods}/{pod_name}")).await?.is_none() {
                anyhow::bail!("couldn't create lifecycle pod for {}: {e}", self.name);
            }
            existed = true;
        } else {
            self.say(&format!("--> {label}: Running hook pod ...")).await;
        }
        let mut restarts = 0;
        let pod = loop {
            let Some(p) = self.get(&format!("{pods}/{pod_name}")).await? else {
                let e = format!("{label}: pod/{pod_name}[{}] unexpectedly deleted", self.ns);
                self.say(&e).await;
                anyhow::bail!(e);
            };
            let r = p["status"]["containerStatuses"][0]["restartCount"].as_i64().unwrap_or(0);
            if r > restarts {
                restarts = r;
                self.say(&format!("--> {label}: Retrying hook pod (retry #{r})")).await;
            }
            match p["status"]["phase"].as_str() {
                Some("Succeeded" | "Failed") => break p,
                _ => tokio::time::sleep(Duration::from_secs(1)).await,
            }
        };
        if existed && pod["status"]["phase"] == "Succeeded" {
            self.say(&format!("--> {label}: Hook pod already succeeded")).await;
            return Ok(());
        }
        // The hook pod's own output, as the deployer copies it.
        match self.api.get(&format!("{pods}/{pod_name}/log")).await {
            Ok(r) if r.status().is_success() => {
                let text = r.text().await.unwrap_or_default();
                for line in text.lines() {
                    self.say(line).await;
                }
            }
            Ok(r) => self.say(&format!("warning: Unable to retrieve hook logs from {pod_name}: {}", r.status())).await,
            Err(e) => self.say(&format!("warning: Unable to retrieve hook logs from {pod_name}: {e}")).await,
        }
        if pod["status"]["phase"] == "Failed" {
            self.say(&format!("--> {label}: Failed")).await;
            let msg = pod["status"]["message"].as_str().filter(|m| !m.is_empty())
                .or(pod["status"]["containerStatuses"][0]["state"]["terminated"]["reason"].as_str())
                .unwrap_or("the hook pod failed");
            anyhow::bail!("{msg}");
        }
        self.say(&format!("--> {label}: Success")).await;
        Ok(())
    }
}

/// The tag half of `<stream>:<tag>`.
fn imagestream_tag(name: &str) -> &str {
    apimachinery::imagestream::split_tag(name).1
}

/// library-go's `naming.GetPodName`: `base-suffix`, shortened with an FNV
/// hash past 63.
fn pod_name(base: &str, suffix: &str) -> String {
    let name = format!("{base}-{suffix}");
    if name.len() <= 63 {
        return name;
    }
    let mut h: u32 = 0x811c9dc5;
    for b in base.bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x01000193);
    }
    let keep = 63usize.saturating_sub(10 + suffix.len()).min(base.len());
    format!("{}-{h:08x}-{suffix}", &base[..keep])
}

/// `makeDeployerPod` with `makeDeployerContainer` (#392): the Custom
/// strategy's image, command and environment with the version's name and
/// namespace, run once as the namespace's `deployer`, owned by the version so
/// it goes when the version does. Scheduling (node selector, tolerations,
/// pull secrets, DNS) follows the version's pods.
pub fn deployer_pod(rc: &Value, dc: &Value) -> Value {
    let st = &dc["spec"]["strategy"];
    let p = &st["customParams"];
    let rc_name = name_of(rc);
    let mut env: Vec<Value> = p["environment"].as_array().cloned().unwrap_or_default();
    env.push(json!({"name": "OPENSHIFT_DEPLOYMENT_NAME", "value": rc_name}));
    env.push(json!({"name": "OPENSHIFT_DEPLOYMENT_NAMESPACE", "value": ns_of(rc)}));
    let mut container = json!({
        "name": "deployment",
        "image": s(p, "image").trim(),
        "env": env,
        "imagePullPolicy": "IfNotPresent",
    });
    if p["command"].as_array().is_some_and(|c| !c.is_empty()) {
        container["command"] = p["command"].clone();
    }
    if st["resources"].is_object() {
        container["resources"] = st["resources"].clone();
    }
    let mut labels = serde_json::Map::new();
    labels.insert(DEPLOYER_POD_FOR_LABEL.into(), json!(rc_name));
    for (k, v) in st["labels"].as_object().into_iter().flatten() {
        labels.entry(k.clone()).or_insert_with(|| v.clone());
    }
    let mut annotations = serde_json::Map::new();
    annotations.insert(DEPLOYMENT_ANNOTATION.into(), json!(rc_name));
    annotations.insert(CONFIG_ANNOTATION.into(), json!(ann(rc, CONFIG_ANNOTATION)));
    for (k, v) in st["annotations"].as_object().into_iter().flatten() {
        annotations.entry(k.clone()).or_insert_with(|| v.clone());
    }
    let tspec = &rc["spec"]["template"]["spec"];
    let mut spec = json!({
        "containers": [container],
        "activeDeadlineSeconds": st["activeDeadlineSeconds"].as_i64().unwrap_or(MAX_DEPLOYMENT_DURATION),
        "restartPolicy": "Never",
        "serviceAccountName": DEPLOYER_SERVICE_ACCOUNT,
        "terminationGracePeriodSeconds": 10,
        "shareProcessNamespace": false,
    });
    for k in ["dnsPolicy", "imagePullSecrets", "tolerations", "nodeSelector"] {
        if !tspec[k].is_null() {
            spec[k] = tspec[k].clone();
        }
    }
    json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": pod_name(&rc_name, "deploy"),
            "namespace": ns_of(rc),
            "labels": labels,
            "annotations": annotations,
            "ownerReferences": [{"apiVersion": "v1", "kind": "ReplicationController", "name": rc_name,
                                 "uid": rc["metadata"]["uid"]}],
        },
        "spec": spec,
    })
}

/// `createHookPodManifest`: the named container's image, the hook's
/// command, the container's env with the hook's over it, the volumes the
/// hook names.
pub fn hook_pod(hook: &Value, rc: &Value, strategy: &Value, suffix: &str, deadline: i64) -> anyhow::Result<Value> {
    let exec = &hook["execNewPod"];
    let container_name = s(exec, "containerName");
    let tspec = &rc["spec"]["template"]["spec"];
    let base = tspec["containers"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|c| s(c, "name") == container_name)
        .ok_or_else(|| anyhow::anyhow!("no container named '{container_name}' found in rc template"))?;
    let mut env: Vec<Value> = base["env"].as_array().cloned().unwrap_or_default();
    for e in exec["env"].as_array().into_iter().flatten() {
        env.retain(|x| x["name"] != e["name"]);
        env.push(e.clone());
    }
    let rc_name = name_of(rc);
    env.push(json!({"name": "OPENSHIFT_DEPLOYMENT_NAME", "value": rc_name}));
    env.push(json!({"name": "OPENSHIFT_DEPLOYMENT_NAMESPACE", "value": ns_of(rc)}));
    let wanted: Vec<&str> = exec["volumes"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let volumes: Vec<Value> = tspec["volumes"].as_array().into_iter().flatten()
        .filter(|v| wanted.contains(&s(v, "name"))).cloned().collect();
    let mounts: Vec<Value> = base["volumeMounts"].as_array().into_iter().flatten()
        .filter(|m| wanted.contains(&s(m, "name"))).cloned().collect();
    let restart = if hook["failurePolicy"] == "Retry" { "OnFailure" } else { "Never" };
    let mut labels = serde_json::Map::new();
    labels.insert(DEPLOYER_POD_FOR_LABEL.into(), json!(rc_name));
    labels.insert(DEPLOYER_POD_TYPE_LABEL.into(), json!(suffix));
    for (k, v) in strategy["labels"].as_object().into_iter().flatten() {
        labels.entry(k.clone()).or_insert_with(|| v.clone());
    }
    let mut annotations = serde_json::Map::new();
    annotations.insert(DEPLOYMENT_ANNOTATION.into(), json!(rc_name));
    for (k, v) in strategy["annotations"].as_object().into_iter().flatten() {
        annotations.entry(k.clone()).or_insert_with(|| v.clone());
    }
    let mut container = json!({
        "name": "lifecycle",
        "image": base["image"],
        "command": exec["command"],
        "env": env,
        "volumeMounts": mounts,
    });
    for k in ["imagePullPolicy", "workingDir", "resources", "securityContext"] {
        if !base[k].is_null() {
            container[k] = base[k].clone();
        }
    }
    let mut spec = json!({
        "containers": [container],
        "volumes": volumes,
        "activeDeadlineSeconds": deadline,
        "restartPolicy": restart,
        "terminationGracePeriodSeconds": 10,
    });
    for k in ["securityContext", "nodeSelector", "imagePullSecrets"] {
        if !tspec[k].is_null() {
            spec[k] = tspec[k].clone();
        }
    }
    Ok(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": pod_name(&rc_name, suffix), "namespace": ns_of(rc), "labels": labels, "annotations": annotations},
        "spec": spec,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dc() -> Value {
        json!({"apiVersion": "apps.openshift.io/v1", "kind": "DeploymentConfig",
               "metadata": {"name": "web", "namespace": "demo", "uid": "u1", "generation": 3, "labels": {"team": "a"}},
               "spec": {"replicas": 3, "selector": {"app": "web"},
                        "strategy": {"type": "Rolling", "rollingParams": {"maxSurge": "25%", "maxUnavailable": "25%"}},
                        "template": {"metadata": {"labels": {"app": "web"}},
                                     "spec": {"containers": [{"name": "web", "image": " nginx:1.25 ",
                                                              "env": [{"name": "A", "value": "1"}],
                                                              "volumeMounts": [{"name": "data", "mountPath": "/d"}]}],
                                              "volumes": [{"name": "data", "emptyDir": {}}, {"name": "other", "emptyDir": {}}]}}},
               "status": {"latestVersion": 2, "details": {"message": "config change", "causes": [{"type": "ConfigChange"}]}}})
    }

    fn rc(version: i64, phase: &str, replicas: i64, available: i64) -> Value {
        json!({"metadata": {"name": format!("web-{version}"), "namespace": "demo",
                            "annotations": {VERSION_ANNOTATION: version.to_string(), PHASE_ANNOTATION: phase}},
               "spec": {"replicas": replicas},
               "status": {"replicas": replicas, "availableReplicas": available, "readyReplicas": available}})
    }

    #[test]
    fn the_version_is_a_replication_controller_as_openshift_makes_it() {
        let d = dc();
        let r = make_deployment(&d);
        assert_eq!(r["metadata"]["name"], "web-2");
        assert_eq!(r["metadata"]["labels"], json!({"team": "a", CONFIG_ANNOTATION: "web"}));
        let a = &r["metadata"]["annotations"];
        assert_eq!(a[PHASE_ANNOTATION], "New");
        assert_eq!(a[VERSION_ANNOTATION], "2");
        assert_eq!(a[DESIRED_REPLICAS_ANNOTATION], "3");
        assert_eq!(a[STATUS_REASON_ANNOTATION], "config change");
        assert_eq!(decode(&r).unwrap()["spec"]["replicas"], 3);
        assert_eq!(r["spec"]["replicas"], 0);
        assert_eq!(r["spec"]["selector"], json!({"app": "web", "deploymentconfig": "web", "deployment": "web-2"}));
        assert_eq!(r["spec"]["template"]["metadata"]["labels"]["deployment"], "web-2");
        assert_eq!(r["spec"]["template"]["metadata"]["annotations"][DEPLOYMENT_ANNOTATION], "web-2");
        assert_eq!(r["spec"]["template"]["spec"]["containers"][0]["image"], "nginx:1.25", "trimmed");
        assert_eq!(r["metadata"]["ownerReferences"][0]["controller"], true);
    }

    #[test]
    fn status_and_conditions_follow_the_versions() {
        let d = dc();
        let rcs = vec![rc(1, "Complete", 0, 0), rc(2, "Complete", 3, 3)];
        let st = calculate_status(&d, &rcs, true, None);
        assert_eq!((st["replicas"].clone(), st["updatedReplicas"].clone(), st["availableReplicas"].clone()),
                   (json!(3), json!(3), json!(3)));
        assert_eq!(st["observedGeneration"], 3);
        let cond = |t: &str| st["conditions"].as_array().unwrap().iter().find(|c| c["type"] == t).cloned().unwrap();
        assert_eq!(cond("Available")["status"], "True");
        assert_eq!(cond("Progressing")["reason"], "NewReplicationControllerAvailable");
        let mut cancelled = rc(2, "Failed", 0, 0);
        cancelled["metadata"]["annotations"][CANCELLED_ANNOTATION] = json!("true");
        let st = calculate_status(&d, &[rc(1, "Complete", 3, 3), cancelled], false, None);
        let p = st["conditions"].as_array().unwrap().iter().find(|c| c["type"] == "Progressing").cloned().unwrap();
        assert_eq!(p["reason"], "RolloutCancelled");
        assert_eq!(st["observedGeneration"], 0, "not observed on this pass");
    }

    #[test]
    fn cleanup_keeps_the_history_limit_behind_the_active_version() {
        let mut d = dc();
        d["status"]["latestVersion"] = json!(5);
        let rcs: Vec<Value> = (1..=5).map(|v| rc(v, "Complete", if v == 5 { 3 } else { 0 }, 0)).collect();
        let prunable: Vec<String> = for_cleanup(&d, &rcs).iter().map(|r| name_of(r)).collect();
        assert_eq!(prunable, ["web-1", "web-2", "web-3", "web-4"]);
        let mut failed = rcs.clone();
        failed[4] = rc(5, "Failed", 0, 0);
        let prunable: Vec<String> = for_cleanup(&d, &failed).iter().map(|r| name_of(r)).collect();
        assert_eq!(prunable, ["web-1", "web-2", "web-3"], "behind the active web-4");
    }

    #[test]
    fn a_hook_pod_is_the_named_container_with_the_hooks_command() {
        let d = dc();
        let r = make_deployment(&d);
        let hook = json!({"failurePolicy": "Retry", "execNewPod": {"containerName": "web", "command": ["/bin/true"],
                          "env": [{"name": "A", "value": "2"}], "volumes": ["data"]}});
        let p = hook_pod(&hook, &r, &d["spec"]["strategy"], "hook-pre", 100).unwrap();
        assert_eq!(p["metadata"]["name"], "web-2-hook-pre");
        assert_eq!(p["metadata"]["labels"][DEPLOYER_POD_FOR_LABEL], "web-2");
        assert_eq!(p["metadata"]["labels"][DEPLOYER_POD_TYPE_LABEL], "hook-pre");
        let c = &p["spec"]["containers"][0];
        assert_eq!((c["name"].clone(), c["image"].clone(), c["command"].clone()),
                   (json!("lifecycle"), json!("nginx:1.25"), json!(["/bin/true"])));
        let env = c["env"].as_array().unwrap();
        assert!(env.contains(&json!({"name": "A", "value": "2"})) && !env.contains(&json!({"name": "A", "value": "1"})));
        assert!(env.contains(&json!({"name": "OPENSHIFT_DEPLOYMENT_NAME", "value": "web-2"})));
        assert_eq!(p["spec"]["volumes"], json!([{"name": "data", "emptyDir": {}}]));
        assert_eq!(c["volumeMounts"], json!([{"name": "data", "mountPath": "/d"}]));
        assert_eq!(p["spec"]["restartPolicy"], "OnFailure", "Retry restarts the hook");
        assert_eq!(p["spec"]["activeDeadlineSeconds"], 100);
        let abort = json!({"failurePolicy": "Abort", "execNewPod": {"containerName": "nope", "command": ["x"]}});
        assert!(hook_pod(&abort, &r, &d["spec"]["strategy"], "hook-mid", 1).is_err());
    }

    #[test]
    fn a_custom_version_runs_the_users_image_as_the_deployer() {
        let mut d = dc();
        d["spec"]["strategy"] = json!({"type": "Custom", "activeDeadlineSeconds": 300,
            "labels": {"x": "y", DEPLOYER_POD_FOR_LABEL: "not-this"}, "annotations": {"a": "b"},
            "resources": {"limits": {"cpu": "100m"}},
            "customParams": {"image": " quay.io/me/deployer:1 ", "command": ["/deploy", "--fast"],
                             "environment": [{"name": "MODE", "value": "blue"}]}});
        d["spec"]["template"]["spec"]["nodeSelector"] = json!({"zone": "a"});
        let mut r = make_deployment(&d);
        r["metadata"]["uid"] = json!("rc-uid");
        let p = deployer_pod(&r, &decode(&r).unwrap());
        assert_eq!(p["metadata"]["name"], "web-2-deploy");
        assert_eq!(p["metadata"]["labels"][DEPLOYER_POD_FOR_LABEL], "web-2", "the strategy's labels never overwrite");
        assert_eq!(p["metadata"]["labels"]["x"], "y");
        assert_eq!(p["metadata"]["annotations"][DEPLOYMENT_ANNOTATION], "web-2");
        assert_eq!(p["metadata"]["annotations"][CONFIG_ANNOTATION], "web");
        assert_eq!(p["metadata"]["annotations"]["a"], "b");
        assert_eq!(p["metadata"]["ownerReferences"][0]["uid"], "rc-uid");
        assert_eq!(p["metadata"]["ownerReferences"][0]["kind"], "ReplicationController");
        let sp = &p["spec"];
        assert_eq!(sp["serviceAccountName"], "deployer");
        assert_eq!(sp["restartPolicy"], "Never");
        assert_eq!(sp["activeDeadlineSeconds"], 300);
        assert_eq!(sp["nodeSelector"], json!({"zone": "a"}));
        let c = &sp["containers"][0];
        assert_eq!((c["name"].clone(), c["image"].clone(), c["command"].clone()),
                   (json!("deployment"), json!("quay.io/me/deployer:1"), json!(["/deploy", "--fast"])));
        assert_eq!(c["env"], json!([{"name": "MODE", "value": "blue"},
                                    {"name": "OPENSHIFT_DEPLOYMENT_NAME", "value": "web-2"},
                                    {"name": "OPENSHIFT_DEPLOYMENT_NAMESPACE", "value": "demo"}]));
        assert_eq!(c["resources"], json!({"limits": {"cpu": "100m"}}));
        assert!(sp["volumes"].is_null() && c["securityContext"].is_null(), "nothing of the app's pod but scheduling");
        d["spec"]["strategy"]["customParams"]["command"] = json!([]);
        let p = deployer_pod(&r, &d);
        assert!(p["spec"]["containers"][0]["command"].is_null(), "no command: the image's entrypoint");
    }

    #[test]
    fn fenceposts_and_pod_names() {
        assert_eq!(fenceposts(&json!("25%"), &json!("25%"), 4), (1, 1));
        assert_eq!(fenceposts(&json!(0), &json!(0), 3), (0, 1));
        assert_eq!(pod_name("web-2", "hook-pre"), "web-2-hook-pre");
        assert!(pod_name(&"a".repeat(70), "hook-post").len() <= 63);
    }

    #[test]
    fn image_triggers_activate_as_upstreams_trigger_activated() {
        const IMG: &str = "node1.lo:5000/demo/web@sha256:1111";
        let mut d = dc();
        d["status"]["latestVersion"] = json!(0);
        d["spec"]["triggers"] = json!([{"type": "ImageChange", "imageChangeParams": {"automatic": true, "containerNames": ["web"],
                                        "from": {"kind": "ImageStreamTag", "name": "web:latest"}}}]);
        assert!(trigger_activated(&d, None).is_none(), "the first version waits for the image");
        d["spec"]["triggers"][0]["imageChangeParams"]["lastTriggeredImage"] = json!(IMG);
        assert_eq!(trigger_activated(&d, None).unwrap()["causes"][0]["type"], "ConfigChange");
        // Version 1 runs the image: nothing; a new image: ImageChange.
        d["status"]["latestVersion"] = json!(1);
        d["spec"]["template"]["spec"]["containers"][0]["image"] = json!(IMG);
        let mut v1 = make_deployment(&d);
        assert!(trigger_activated(&d, Some(&v1)).is_none());
        let img2 = "node1.lo:5000/demo/web@sha256:2222";
        d["spec"]["triggers"][0]["imageChangeParams"]["lastTriggeredImage"] = json!(img2);
        d["spec"]["template"]["spec"]["containers"][0]["image"] = json!(img2);
        let details = trigger_activated(&d, Some(&v1)).unwrap();
        assert_eq!(details["message"], "image change");
        assert_eq!(details["causes"][0], json!({"type": "ImageChange", "imageTrigger": {"from": {"kind": "DockerImage", "name": img2}}}));
        // A manual trigger never fires by itself; no ConfigChange: no template rollout.
        d["spec"]["triggers"][0]["imageChangeParams"]["automatic"] = json!(false);
        assert!(trigger_activated(&d, Some(&v1)).is_none());
        d["spec"]["triggers"] = json!([{"type": "ConfigChange"}]);
        assert_eq!(trigger_activated(&d, Some(&v1)).unwrap()["message"], "config change");
        v1["metadata"]["annotations"][ENCODED_CONFIG_ANNOTATION] = json!(d.to_string());
        assert!(trigger_activated(&d, Some(&v1)).is_none());
        d["spec"]["paused"] = json!(true);
        d["spec"]["template"]["spec"]["containers"][0]["image"] = json!("x");
        assert!(trigger_activated(&d, Some(&v1)).is_none(), "paused");
    }
}
