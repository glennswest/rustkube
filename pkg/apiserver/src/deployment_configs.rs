//! `apps.openshift.io/v1` DeploymentConfigs (#375, sc#151, sc#162): the API
//! half — what openshift-apiserver's `pkg/apps` does. The rollouts are the
//! controller-manager's (`deploymentconfig` there): each version is a
//! ReplicationController, rolled out by the controller itself rather than a
//! deployer pod (the choice recorded on #375).
//!
//! Ported from openshift/openshift-apiserver (read 2026-11-23):
//! `apis/apps/v1/defaults.go` ([`normalize`]), `apis/apps/validation`
//! ([`validate`]), `registry/deployconfig` (status kept on a spec write but
//! for a `latestVersion` one ahead; spec kept on a status write; generation),
//! `registry/instantiate` ([`instantiate`]) and `registry/rollback`
//! ([`rollback`]).
//!
//! **What this cluster cannot run is refused, not stored** (no stubs): a
//! Custom strategy (it runs a user's image as the deployer, under a
//! `deployer` account this cluster does not provision), `tagImages` hooks
//! and ImageChange triggers (no ImageStreams are served) are each a field
//! error.

use crate::error::ApiError;
use crate::storage::ResourceStorage;
use serde_json::{json, Value};

pub const GROUP: &str = "apps.openshift.io";
pub const API_VERSION: &str = "apps.openshift.io/v1";
pub const RESOURCE: &str = "deploymentconfigs";

pub const CONFIG_ANNOTATION: &str = "openshift.io/deployment-config.name";
pub const VERSION_ANNOTATION: &str = "openshift.io/deployment-config.latest-version";
pub const PHASE_ANNOTATION: &str = "openshift.io/deployment.phase";
pub const ENCODED_CONFIG_ANNOTATION: &str = "openshift.io/encoded-deployment-config";
pub const DESIRED_REPLICAS_ANNOTATION: &str = "kubectl.kubernetes.io/desired-replicas";
/// The rollout's log: what OpenShift's deployer pod prints, recorded on its
/// ReplicationController by the controller that runs it (#375, option A).
pub const LOG_ANNOTATION: &str = "deploy.rustkube.io/log";

const DEFAULT_ROLLING_INTERVAL: i64 = 1;
const DEFAULT_ROLLING_UPDATE_PERIOD: i64 = 1;
const DEFAULT_TIMEOUT: i64 = 600;
const MAX_DEPLOYMENT_DURATION: i64 = 21600;
const DEFAULT_REVISION_HISTORY_LIMIT: i64 = 10;
/// The strategies this cluster runs.
const STRATEGIES: [&str; 2] = ["Rolling", "Recreate"];

// --- field errors -----------------------------------------------------------------

fn q(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}
fn invalid(path: &str, value: &str, detail: &str) -> String {
    format!("{path}: Invalid value: {value}: {detail}")
}
fn required(path: &str, detail: &str) -> String {
    if detail.is_empty() { format!("{path}: Required value") } else { format!("{path}: Required value: {detail}") }
}
fn not_supported(path: &str, value: &str, valid: &[&str]) -> String {
    let quoted: Vec<String> = valid.iter().map(|v| q(v)).collect();
    format!("{path}: Unsupported value: {}: supported values: {}", q(value), quoted.join(", "))
}
fn refuse(kind: &str, name: &str, errs: Vec<String>) -> Result<(), ApiError> {
    if errs.is_empty() {
        return Ok(());
    }
    let what = if errs.len() == 1 { errs[0].clone() } else { format!("[{}]", errs.join(", ")) };
    Err(ApiError::invalid(&format!("{kind}.{GROUP} {name:?} is invalid: {what}")))
}
fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("")
}

// --- defaults -------------------------------------------------------------------------

fn default_hook_container(hook: &mut Value, container: &str) {
    if let Some(exec) = hook.get_mut("execNewPod").filter(|e| e.is_object()) {
        if s(exec, "containerName").is_empty() {
            exec["containerName"] = json!(container);
        }
    }
}

/// `SetDefaults_*` of apps.openshift.io/v1, and the group's layered default
/// `revisionHistoryLimit` (10).
pub fn normalize(dc: &mut Value) {
    if !dc["spec"].is_object() {
        dc["spec"] = json!({});
    }
    let spec = &mut dc["spec"];
    if spec.get("triggers").is_none_or(Value::is_null) {
        spec["triggers"] = json!([{"type": "ConfigChange"}]);
    }
    if spec["selector"].as_object().is_none_or(|m| m.is_empty()) && spec["template"]["metadata"]["labels"].is_object() {
        spec["selector"] = spec["template"]["metadata"]["labels"].clone();
    }
    if spec["replicas"].is_null() {
        spec["replicas"] = json!(0);
    }
    if spec["revisionHistoryLimit"].is_null() {
        spec["revisionHistoryLimit"] = json!(DEFAULT_REVISION_HISTORY_LIMIT);
    }
    if !spec["strategy"].is_object() {
        spec["strategy"] = json!({});
    }
    let st = &mut spec["strategy"];
    if s(st, "type").is_empty() {
        st["type"] = json!("Rolling");
    }
    if st["type"] == "Rolling" && st["rollingParams"].is_null() {
        st["rollingParams"] = json!({});
    }
    if st["type"] == "Recreate" && st["recreateParams"].is_null() {
        st["recreateParams"] = json!({});
    }
    if st["activeDeadlineSeconds"].is_null() {
        st["activeDeadlineSeconds"] = json!(MAX_DEPLOYMENT_DURATION);
    }
    if let Some(p) = st.get_mut("recreateParams").filter(|p| p.is_object()) {
        if p["timeoutSeconds"].is_null() {
            p["timeoutSeconds"] = json!(DEFAULT_TIMEOUT);
        }
    }
    if let Some(p) = st.get_mut("rollingParams").filter(|p| p.is_object()) {
        for (k, v) in [("intervalSeconds", DEFAULT_ROLLING_INTERVAL), ("updatePeriodSeconds", DEFAULT_ROLLING_UPDATE_PERIOD),
                       ("timeoutSeconds", DEFAULT_TIMEOUT)] {
            if p[k].is_null() {
                p[k] = json!(v);
            }
        }
        let zero = |v: &Value| v == &json!(0) || v == &json!("0%");
        match (p["maxUnavailable"].is_null(), p["maxSurge"].is_null()) {
            (true, true) => {
                p["maxUnavailable"] = json!("25%");
                p["maxSurge"] = json!("25%");
            }
            (true, false) if zero(&p["maxSurge"]) => p["maxUnavailable"] = json!("25%"),
            (false, true) if zero(&p["maxUnavailable"]) => p["maxSurge"] = json!("25%"),
            _ => {}
        }
    }
    // A lone container is every hook's default container.
    let containers = spec["template"]["spec"]["containers"].as_array().cloned().unwrap_or_default();
    if containers.len() == 1 {
        let name = s(&containers[0], "name").to_string();
        for (params, hooks) in [("recreateParams", &["pre", "mid", "post"][..]), ("rollingParams", &["pre", "post"][..])] {
            for h in hooks {
                if let Some(hook) = spec["strategy"].get_mut(params).and_then(|p| p.get_mut(*h)).filter(|v| v.is_object()) {
                    default_hook_container(hook, &name);
                }
            }
        }
    }
}

// --- validation ---------------------------------------------------------------------

/// An IntOrString that is an int, or a percentage (`25%`).
fn int_or_percent(v: &Value) -> Option<(i64, bool)> {
    match v {
        Value::Number(n) => n.as_i64().map(|n| (n, false)),
        Value::String(s) => s.strip_suffix('%').and_then(|p| p.parse::<i64>().ok()).map(|p| (p, true)),
        _ => None,
    }
}

/// `ResolveFenceposts`: surge rounded up, unavailable down, never both 0.
pub fn fenceposts(max_surge: &Value, max_unavailable: &Value, desired: i64) -> (i64, i64) {
    let resolve = |v: &Value, up: bool| match int_or_percent(v) {
        Some((n, false)) => n,
        Some((p, true)) => {
            let raw = p * desired;
            if up { (raw + 99) / 100 } else { raw / 100 }
        }
        None => 0,
    };
    let surge = resolve(max_surge, true);
    let mut unavailable = resolve(max_unavailable, false);
    if surge == 0 && unavailable == 0 {
        unavailable = 1;
    }
    (surge, unavailable)
}

fn validate_hook(hook: &Value, path: &str, containers: &[String], errs: &mut Vec<String>) {
    if !hook.is_object() {
        return;
    }
    match s(hook, "failurePolicy") {
        "" => errs.push(required(&format!("{path}.failurePolicy"), "")),
        "Abort" | "Retry" | "Ignore" => {}
        other => errs.push(not_supported(&format!("{path}.failurePolicy"), other, &["Abort", "Retry", "Ignore"])),
    }
    if hook["tagImages"].as_array().is_some_and(|t| !t.is_empty()) {
        // Tagging needs ImageStreams, which are not served.
        errs.push(format!("{path}.tagImages: Forbidden: tagImages hooks need ImageStreams, which this cluster does not serve"));
        return;
    }
    let exec = &hook["execNewPod"];
    if !exec.is_object() {
        errs.push(invalid(path, "\"<empty>\"", "One of execNewPod or tagImages must be specified"));
        return;
    }
    if exec["command"].as_array().is_none_or(|c| c.is_empty()) {
        errs.push(required(&format!("{path}.execNewPod.command"), ""));
    }
    let c = s(exec, "containerName");
    if c.is_empty() {
        errs.push(required(&format!("{path}.execNewPod.containerName"), ""));
    } else if !containers.iter().any(|n| n == c) {
        errs.push(invalid(&format!("{path}.execNewPod.containerName"), &q(c), &format!("no container {c:?} found")));
    }
    for (i, v) in exec["volumes"].as_array().into_iter().flatten().enumerate() {
        if v.as_str().is_none_or(str::is_empty) {
            errs.push(invalid(&format!("{path}.execNewPod.volumes[{i}]"), "\"\"", "must not be empty"));
        }
    }
    for (i, e) in exec["env"].as_array().into_iter().flatten().enumerate() {
        if s(e, "name").is_empty() {
            errs.push(required(&format!("{path}.execNewPod.env[{i}]"), ""));
        }
    }
}

fn validate_strategy(spec: &Value, errs: &mut Vec<String>) {
    let st = &spec["strategy"];
    let containers: Vec<String> = spec["template"]["spec"]["containers"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(spec["template"]["spec"]["initContainers"].as_array().into_iter().flatten())
        .map(|c| s(c, "name").to_string())
        .collect();
    let ty = s(st, "type");
    match ty {
        "" => errs.push(required("spec.strategy.type", "strategy type is required")),
        "Recreate" => {
            let p = &st["recreateParams"];
            if let Some(t) = p["timeoutSeconds"].as_i64().filter(|t| *t <= 0) {
                errs.push(invalid("spec.strategy.recreateParams.timeoutSeconds", &t.to_string(), "must be >0"));
            }
            for h in ["pre", "mid", "post"] {
                validate_hook(&p[h], &format!("spec.strategy.recreateParams.{h}"), &containers, errs);
            }
        }
        "Rolling" => {
            let p = &st["rollingParams"];
            if !p.is_object() {
                errs.push(required("spec.strategy.rollingParams", ""));
                return;
            }
            for k in ["intervalSeconds", "updatePeriodSeconds", "timeoutSeconds"] {
                if let Some(t) = p[k].as_i64().filter(|t| *t <= 0) {
                    errs.push(invalid(&format!("spec.strategy.rollingParams.{k}"), &t.to_string(), "must be >0"));
                }
            }
            for k in ["maxSurge", "maxUnavailable"] {
                if p[k].is_null() {
                    continue;
                }
                match int_or_percent(&p[k]) {
                    None => errs.push(invalid(&format!("spec.strategy.rollingParams.{k}"), &p[k].to_string(),
                                              "value should be int(5) or percentage(5%)")),
                    Some((n, _)) if n < 0 => errs.push(invalid(&format!("spec.strategy.rollingParams.{k}"), &p[k].to_string(),
                                                              "must be greater than or equal to 0")),
                    Some((n, true)) if n > 100 && k == "maxUnavailable" => errs.push(invalid(
                        &format!("spec.strategy.rollingParams.{k}"), &p[k].to_string(), "should not be more than 100%")),
                    _ => {}
                }
            }
            let is_zero = |v: &Value| int_or_percent(v).is_none_or(|(n, _)| n == 0);
            if is_zero(&p["maxSurge"]) && is_zero(&p["maxUnavailable"]) {
                errs.push(invalid("spec.strategy.rollingParams.maxUnavailable", &p["maxUnavailable"].to_string(),
                                  "cannot be 0 when maxSurge is 0 as well"));
            }
            for h in ["pre", "post"] {
                validate_hook(&p[h], &format!("spec.strategy.rollingParams.{h}"), &containers, errs);
            }
        }
        other => errs.push(not_supported("spec.strategy.type", other, &STRATEGIES)),
    }
    if let Some(ad) = st["activeDeadlineSeconds"].as_i64() {
        if ad < 0 {
            errs.push(invalid("spec.strategy.activeDeadlineSeconds", &ad.to_string(), "must be greater than or equal to 0"));
        }
        let timeout = st["rollingParams"]["timeoutSeconds"].as_i64().or(st["recreateParams"]["timeoutSeconds"].as_i64());
        if timeout.is_some_and(|t| ad <= t) {
            errs.push(invalid("spec.strategy.activeDeadlineSeconds", &ad.to_string(),
                              "activeDeadlineSeconds must be greater than timeoutSeconds"));
        }
    }
}

/// `ValidateDeploymentConfigSpec` and the object's name.
pub fn validate(dc: &Value) -> Result<(), ApiError> {
    let name = s(&dc["metadata"], "name");
    let mut errs = Vec::new();
    if name.is_empty() {
        errs.push(required("metadata.name", "name or generateName is required"));
    } else if !crate::user_groups::is_dns_subdomain(name) {
        errs.push(invalid("metadata.name", &q(name), "a lowercase RFC 1123 subdomain must consist of lower case alphanumeric characters, '-' or '.', and must start and end with an alphanumeric character"));
    }
    let spec = &dc["spec"];
    for (i, t) in spec["triggers"].as_array().into_iter().flatten().enumerate() {
        match s(t, "type") {
            "" => errs.push(required(&format!("spec.triggers[{i}].type"), "")),
            "ConfigChange" => {}
            // ImageChange needs ImageStreams, which are not served.
            other => errs.push(not_supported(&format!("spec.triggers[{i}].type"), other, &["ConfigChange"])),
        }
    }
    if spec["template"].is_object() {
        validate_strategy(spec, &mut errs);
    }
    if let Some(n) = spec["revisionHistoryLimit"].as_i64().filter(|n| *n < 0) {
        errs.push(invalid("spec.revisionHistoryLimit", &n.to_string(), "must be greater than or equal to 0"));
    }
    let min_ready = spec["minReadySeconds"].as_i64().unwrap_or(0);
    if min_ready < 0 {
        errs.push(invalid("spec.minReadySeconds", &min_ready.to_string(), "must be greater than or equal to 0"));
    }
    let timeout = spec["strategy"]["rollingParams"]["timeoutSeconds"]
        .as_i64()
        .or(spec["strategy"]["recreateParams"]["timeoutSeconds"].as_i64())
        .unwrap_or(DEFAULT_TIMEOUT);
    if timeout > 0 && min_ready >= timeout {
        errs.push(invalid("spec.minReadySeconds", &min_ready.to_string(),
                          &format!("must be less than the deployment timeout ({timeout}s)")));
    }
    if let Some(ad) = spec["strategy"]["activeDeadlineSeconds"].as_i64() {
        if min_ready >= ad {
            errs.push(invalid("spec.minReadySeconds", &min_ready.to_string(),
                              &format!("must be less than activeDeadlineSeconds ({ad}s - used by the deployer pod)")));
        }
    }
    let template = &spec["template"];
    if !template.is_object() {
        errs.push(required("spec.template", ""));
    } else {
        let containers = template["spec"]["containers"].as_array().cloned().unwrap_or_default();
        if containers.is_empty() {
            errs.push(required("spec.template.spec.containers", ""));
        }
        for (i, c) in containers.iter().enumerate() {
            if s(c, "name").is_empty() {
                errs.push(required(&format!("spec.template.spec.containers[{i}].name"), ""));
            }
            if s(c, "image").trim().is_empty() {
                errs.push(required(&format!("spec.template.spec.containers[{i}].image"), ""));
            }
        }
        let labels = &template["metadata"]["labels"];
        for (k, v) in spec["selector"].as_object().into_iter().flatten() {
            if &labels[k] != v {
                errs.push(invalid("spec.template.metadata.labels", &labels.to_string(),
                                  "`selector` does not match template `labels`"));
                break;
            }
        }
    }
    if let Some(r) = spec["replicas"].as_i64().filter(|r| *r < 0) {
        errs.push(invalid("spec.replicas", &r.to_string(), "replicas cannot be negative"));
    }
    if spec["selector"].as_object().is_none_or(|m| m.is_empty()) {
        errs.push(invalid("spec.selector", "map[]", "selector cannot be empty"));
    }
    refuse("DeploymentConfig", name, errs)
}

// --- the registry strategies -------------------------------------------------------

/// A create: defaults, status empty, generation 1, validation.
pub fn admit_create(dc: &mut Value) -> Result<(), ApiError> {
    normalize(dc);
    dc["status"] = json!({"latestVersion": 0, "observedGeneration": 0, "replicas": 0, "updatedReplicas": 0,
                          "availableReplicas": 0, "unavailableReplicas": 0});
    dc["metadata"]["generation"] = json!(1);
    validate(dc)
}

/// An update through the main resource keeps the stored status, but for a
/// `latestVersion` one ahead (`oc rollout latest` of old); through
/// `/status` it keeps the spec and labels, and nothing goes back.
pub fn admit_update(dc: &mut Value, old: &Value, status_write: bool) -> Result<(), ApiError> {
    if status_write {
        dc["spec"] = old["spec"].clone();
        dc["metadata"]["labels"] = old["metadata"]["labels"].clone();
        let mut errs = Vec::new();
        for k in ["latestVersion", "observedGeneration"] {
            let (now, before) = (dc["status"][k].as_i64().unwrap_or(0), old["status"][k].as_i64().unwrap_or(0));
            if now < before {
                errs.push(invalid(&format!("status.{k}"), &now.to_string(), &format!("{k} cannot be decremented")));
            }
        }
        return refuse("DeploymentConfig", s(&old["metadata"], "name"), errs);
    }
    normalize(dc);
    let (new_v, old_v) = (dc["status"]["latestVersion"].as_i64().unwrap_or(0), old["status"]["latestVersion"].as_i64().unwrap_or(0));
    dc["status"] = old["status"].clone();
    if new_v == old_v + 1 {
        dc["status"]["latestVersion"] = json!(new_v);
    }
    let gen = old["metadata"]["generation"].as_i64().unwrap_or(1);
    let changed = dc["spec"] != old["spec"] || dc["status"]["latestVersion"] != old["status"]["latestVersion"];
    dc["metadata"]["generation"] = json!(if changed { gen + 1 } else { gen });
    validate(dc)
}

// --- instantiate and rollback ----------------------------------------------------------

/// `<config>-<version>`, the version's ReplicationController.
pub fn rc_name(config: &str, version: i64) -> String {
    format!("{config}-{version}")
}

/// The DeploymentConfig a ReplicationController was made from
/// (`DecodeDeploymentConfig`).
pub fn decode(rc: &Value) -> Option<Value> {
    serde_json::from_str(rc["metadata"]["annotations"][ENCODED_CONFIG_ANNOTATION].as_str()?).ok()
}

fn not_found(name: &str) -> ApiError {
    ApiError::not_found("deploymentconfigs.apps.openshift.io", name)
}

/// `deploymentconfigs/instantiate` (`oc rollout latest`): a new version when
/// forced, when there is none yet, or when the template differs from the
/// latest version's; `None` (a 204 Status) when nothing would change.
pub async fn instantiate(storage: &ResourceStorage, ns: &str, name: &str, req: &Value) -> Result<Option<Value>, ApiError> {
    let key = ResourceStorage::namespaced_key(RESOURCE, ns, name);
    for _ in 0..5 {
        let mut dc = storage.get(&key).await.map_err(|e| if e.status == axum::http::StatusCode::NOT_FOUND { not_found(name) } else { e })?;
        let mut errs = Vec::new();
        if s(req, "name").is_empty() {
            errs.push(invalid("name", "\"\"", "name of the deployment config is missing"));
        }
        if dc["spec"]["paused"] == true {
            errs.push(invalid("spec.paused", "true",
                              &format!("deployment config {name:?} is paused - unpause to request a new deployment")));
        }
        refuse("DeploymentRequest", name, errs)?;
        let version = dc["status"]["latestVersion"].as_i64().unwrap_or(0);
        let force = req["force"] == true;
        let causes = if force {
            Some(json!([{"type": "Manual"}]))
        } else {
            let has_config_trigger = dc["spec"]["triggers"].as_array().into_iter().flatten().any(|t| t["type"] == "ConfigChange");
            let template_changed = if version == 0 {
                true
            } else {
                let rc = storage
                    .get(&ResourceStorage::namespaced_key("replicationcontrollers", ns, &rc_name(name, version)))
                    .await?;
                decode(&rc).is_none_or(|d| d["spec"]["template"] != dc["spec"]["template"])
            };
            (has_config_trigger && template_changed).then(|| json!([{"type": "ConfigChange"}]))
        };
        let Some(causes) = causes else { return Ok(None) };
        let message = if force { "manual change" } else { "config change" };
        dc["status"]["details"] = json!({"message": message, "causes": causes});
        dc["status"]["latestVersion"] = json!(version + 1);
        let g = dc["metadata"]["generation"].as_i64().unwrap_or(1);
        dc["metadata"]["generation"] = json!(g + 1);
        let rev = dc["metadata"]["resourceVersion"].as_str().and_then(|r| r.parse().ok());
        match storage.update(&key, dc, rev).await {
            Ok(v) => return Ok(Some(v)),
            Err(e) if e.reason == "Conflict" => continue,
            Err(e) => return Err(e),
        }
    }
    Err(ApiError::conflict(&format!(
        "Operation cannot be fulfilled on deploymentconfigs.{GROUP} {name:?}: the object has been modified; please apply your changes to the latest version and try again"
    )))
}

/// `deploymentconfigs/rollback` (`oc rollback`, `oc rollout undo`): the
/// config as it would be rolled back to `spec.revision` (the previous one by
/// default), not stored — the client updates with it, as OpenShift's does.
pub async fn rollback(storage: &ResourceStorage, ns: &str, name: &str, req: &Value) -> Result<Value, ApiError> {
    let fail = |reason: &str| {
        Err(ApiError::invalid(&format!(
            "DeploymentConfigRollback.{GROUP} {name:?} is invalid: name: Invalid value: {}: {reason}",
            q(name)
        )))
    };
    let mut errs = Vec::new();
    if s(req, "name").is_empty() {
        errs.push(required("name", "name of the deployment config is missing"));
    }
    if let Some(r) = req["spec"]["revision"].as_i64().filter(|r| *r < 0) {
        errs.push(invalid("spec.revision", &r.to_string(), "must be non-negative"));
    }
    refuse("DeploymentConfigRollback", name, errs)?;
    let Ok(from) = storage.get(&ResourceStorage::namespaced_key(RESOURCE, ns, name)).await else {
        return fail(&format!("cannot get deployment config {name:?}: deploymentconfigs.{GROUP} {name:?} not found"));
    };
    let latest = from["status"]["latestVersion"].as_i64().unwrap_or(0);
    let asked = req["spec"]["revision"].as_i64().unwrap_or(0);
    match latest {
        0 => return fail("cannot rollback an undeployed config"),
        1 => return fail(&format!("no previous deployment exists for \"{ns}/{name}\"")),
        v if v == asked => return fail(&format!("version {asked} is already the latest")),
        _ => {}
    }
    let revision = if asked > 0 { asked } else { latest - 1 };
    let target = rc_name(name, revision);
    let Ok(rc) = storage.get(&ResourceStorage::namespaced_key("replicationcontrollers", ns, &target)).await else {
        return fail(&format!("replicationcontrollers {target:?} not found"));
    };
    let Some(to) = decode(&rc) else {
        return fail("couldn't decode deployment config from deployment: no encoded config");
    };
    let mut out = from.clone();
    for (k, v) in req["updatedAnnotations"].as_object().into_iter().flatten() {
        out["metadata"]["annotations"][k] = v.clone();
    }
    let spec = &req["spec"];
    if spec["includeTemplate"] == true {
        out["spec"]["template"] = to["spec"]["template"].clone();
    }
    if spec["includeReplicationMeta"] == true {
        out["spec"]["replicas"] = to["spec"]["replicas"].clone();
        out["spec"]["selector"] = to["spec"]["selector"].clone();
    }
    if spec["includeTriggers"] == true {
        out["spec"]["triggers"] = to["spec"]["triggers"].clone();
    }
    if spec["includeStrategy"] == true {
        out["spec"]["strategy"] = to["spec"]["strategy"].clone();
    }
    out["status"]["latestVersion"] = json!(latest + 1);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dc() -> Value {
        json!({"apiVersion": API_VERSION, "kind": "DeploymentConfig", "metadata": {"name": "web", "namespace": "demo"},
               "spec": {"replicas": 2,
                        "template": {"metadata": {"labels": {"app": "web"}},
                                     "spec": {"containers": [{"name": "web", "image": "nginx:1.25"}]}}}})
    }

    #[test]
    fn defaults_as_apps_v1_writes_them() {
        let mut d = dc();
        admit_create(&mut d).unwrap();
        let spec = &d["spec"];
        assert_eq!(spec["triggers"], json!([{"type": "ConfigChange"}]));
        assert_eq!(spec["selector"], json!({"app": "web"}));
        assert_eq!(spec["revisionHistoryLimit"], 10);
        assert_eq!(spec["strategy"]["type"], "Rolling");
        assert_eq!(spec["strategy"]["activeDeadlineSeconds"], 21600);
        assert_eq!(spec["strategy"]["rollingParams"],
                   json!({"intervalSeconds": 1, "updatePeriodSeconds": 1, "timeoutSeconds": 600,
                          "maxUnavailable": "25%", "maxSurge": "25%"}));
        assert_eq!(d["status"]["latestVersion"], 0);
        // A hook's container defaults to the only one.
        let mut r = dc();
        r["spec"]["strategy"] = json!({"type": "Recreate",
            "recreateParams": {"pre": {"failurePolicy": "Abort", "execNewPod": {"command": ["/bin/true"]}}}});
        admit_create(&mut r).unwrap();
        assert_eq!(r["spec"]["strategy"]["recreateParams"]["pre"]["execNewPod"]["containerName"], "web");
        assert_eq!(r["spec"]["strategy"]["recreateParams"]["timeoutSeconds"], 600);
        // maxSurge 0 alone: unavailable 25%.
        let mut z = dc();
        z["spec"]["strategy"] = json!({"rollingParams": {"maxSurge": 0}});
        admit_create(&mut z).unwrap();
        assert_eq!(z["spec"]["strategy"]["rollingParams"]["maxUnavailable"], "25%");
    }

    #[test]
    fn what_this_cluster_cannot_run_is_refused() {
        let cases: Vec<(&str, Value)> = vec![
            ("spec.strategy.type: Unsupported value: \"Custom\"",
             json!({"type": "Custom", "customParams": {"image": "x"}})),
        ];
        for (want, strategy) in cases {
            let mut d = dc();
            d["spec"]["strategy"] = strategy;
            let e = admit_create(&mut d).unwrap_err();
            assert!(e.message.contains(want), "{}", e.message);
        }
        let mut d = dc();
        d["spec"]["triggers"] = json!([{"type": "ImageChange", "imageChangeParams": {"from": {"name": "x:1"}}}]);
        assert!(admit_create(&mut d).unwrap_err().message.contains("spec.triggers[0].type: Unsupported value: \"ImageChange\""));
        let mut d = dc();
        d["spec"]["strategy"] = json!({"type": "Recreate", "recreateParams": {"pre": {"failurePolicy": "Abort",
            "tagImages": [{"containerName": "web", "to": {"kind": "ImageStreamTag", "name": "a:b"}}]}}});
        assert!(admit_create(&mut d).unwrap_err().message.contains("tagImages: Forbidden"));
    }

    #[test]
    fn upstream_validation() {
        let refused: Vec<(&str, Value)> = vec![
            ("spec.replicas: Invalid value: -1: replicas cannot be negative", json!({"replicas": -1})),
            ("spec.template: Required value", json!({"template": null, "selector": {"a": "b"}})),
            ("spec.strategy.rollingParams.maxUnavailable: Invalid value: 0: cannot be 0 when maxSurge is 0 as well",
             json!({"strategy": {"rollingParams": {"maxSurge": 0, "maxUnavailable": 0}}})),
            ("spec.strategy.rollingParams.intervalSeconds: Invalid value: 0: must be >0",
             json!({"strategy": {"rollingParams": {"intervalSeconds": 0}}})),
            ("spec.minReadySeconds: Invalid value: 600: must be less than the deployment timeout (600s)",
             json!({"minReadySeconds": 600})),
            ("spec.strategy.recreateParams.pre.failurePolicy: Required value",
             json!({"strategy": {"type": "Recreate", "recreateParams": {"pre": {"execNewPod": {"command": ["x"]}}}}})),
            ("spec.strategy.recreateParams.mid.execNewPod.containerName: Invalid value: \"nope\"",
             json!({"strategy": {"type": "Recreate", "recreateParams": {"mid": {"failurePolicy": "Abort",
                    "execNewPod": {"command": ["x"], "containerName": "nope"}}}}})),
            ("spec.template.metadata.labels: Invalid value",
             json!({"selector": {"app": "other"}})),
            ("spec.strategy.activeDeadlineSeconds: Invalid value: 100: activeDeadlineSeconds must be greater than timeoutSeconds",
             json!({"strategy": {"activeDeadlineSeconds": 100}})),
        ];
        for (want, change) in refused {
            let mut d = dc();
            for (k, v) in change.as_object().unwrap() {
                if v.is_object() && d["spec"][k].is_object() {
                    for (k2, v2) in v.as_object().unwrap() {
                        d["spec"][k][k2] = v2.clone();
                    }
                } else {
                    d["spec"][k] = v.clone();
                }
            }
            let e = admit_create(&mut d).unwrap_err();
            assert!(e.message.contains(want), "want {want:?} in {}", e.message);
        }
    }

    #[test]
    fn status_and_spec_writes_keep_each_other() {
        let mut old = dc();
        admit_create(&mut old).unwrap();
        old["status"]["latestVersion"] = json!(3);
        old["status"]["replicas"] = json!(2);
        // A spec write keeps the status, but takes latestVersion one ahead.
        let mut put = old.clone();
        put["spec"]["replicas"] = json!(5);
        put["status"] = json!({"latestVersion": 4, "replicas": 99});
        admit_update(&mut put, &old, false).unwrap();
        assert_eq!(put["status"]["replicas"], 2);
        assert_eq!(put["status"]["latestVersion"], 4);
        assert_eq!(put["metadata"]["generation"], 2);
        let mut jump = old.clone();
        jump["status"]["latestVersion"] = json!(9);
        admit_update(&mut jump, &old, false).unwrap();
        assert_eq!(jump["status"]["latestVersion"], 3, "only one ahead is taken");
        // A status write keeps the spec; nothing goes back.
        let mut st = old.clone();
        st["spec"]["replicas"] = json!(7);
        st["status"]["replicas"] = json!(1);
        admit_update(&mut st, &old, true).unwrap();
        assert_eq!(st["spec"]["replicas"], 2);
        assert_eq!(st["status"]["replicas"], 1);
        let mut back = old.clone();
        back["status"]["latestVersion"] = json!(2);
        assert!(admit_update(&mut back, &old, true).unwrap_err().message.contains("latestVersion cannot be decremented"));
    }

    #[test]
    fn fenceposts_round_as_upstream() {
        assert_eq!(fenceposts(&json!("25%"), &json!("25%"), 4), (1, 1));
        assert_eq!(fenceposts(&json!("25%"), &json!("25%"), 3), (1, 0));
        assert_eq!(fenceposts(&json!(0), &json!(0), 3), (0, 1));
        assert_eq!(fenceposts(&json!(2), &json!("50%"), 5), (2, 2));
    }
}
