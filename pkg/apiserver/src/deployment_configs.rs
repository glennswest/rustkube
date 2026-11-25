//! `apps.openshift.io/v1` DeploymentConfigs (#375, sc#151, sc#162): the API
//! half — what openshift-apiserver's `pkg/apps` does. The rollouts are the
//! controller-manager's (`deploymentconfig` there): each version is a
//! ReplicationController, rolled out by the controller itself rather than a
//! deployer pod (the choice recorded on #375). A Custom strategy is the
//! exception (#392): its deployer pod `<rc>-deploy` runs the user's image
//! under the namespace's `deployer` account, which `system:deployers` binds
//! to `system:deployer` ([`deployers_binding`], kept by `default_rolebindings`).
//!
//! Ported from openshift/openshift-apiserver (read 2026-11-23):
//! `apis/apps/v1/defaults.go` ([`normalize`]), `apis/apps/validation`
//! ([`validate`]), `registry/deployconfig` (status kept on a spec write but
//! for a `latestVersion` one ahead; spec kept on a status write; generation),
//! `registry/instantiate` ([`instantiate`]) and `registry/rollback`
//! ([`rollback`]).
//!
//! **What this cluster cannot run is refused, not stored** (no stubs):
//! `customParams` on a Rolling or Recreate strategy (it swaps the deployer
//! image, and those rollouts have none here) and a Custom strategy without
//! an image (there is no default deployer image) are each a field error.
//!
//! **ImageChange triggers** (#386): validated as upstream's (an
//! `ImageStreamTag` `<stream>:<tag>`, container names in the template; a
//! container an image trigger names may have no image yet). The
//! controller-manager's `imagetrigger` controller resolves an automatic
//! trigger into its containers and `lastTriggeredImage`;
//! [`instantiate`] does it here for `oc rollout latest` (`processTriggers`),
//! reading the stream through the aggregator — ImageStreams are the
//! registry's (#385). `tagImages` hooks are run by the deployer.

use crate::error::ApiError;
use crate::image_policy::Fetched;
use crate::storage::ResourceStorage;
use serde_json::{json, Value};
use std::future::Future;

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
const STRATEGIES: [&str; 3] = ["Rolling", "Recreate", "Custom"];

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
    // SetDefaults_DeploymentTriggerPolicy: an image trigger's `from` is an
    // ImageStreamTag unless it says otherwise.
    for t in spec["triggers"].as_array_mut().into_iter().flatten() {
        if t["type"] == "ImageChange" && t["imageChangeParams"].is_object() {
            let from = &mut t["imageChangeParams"]["from"];
            if !from.is_object() {
                *from = json!({});
            }
            if s(from, "kind").is_empty() {
                from["kind"] = json!("ImageStreamTag");
            }
        }
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

// --- the deployer's own access ------------------------------------------------------

/// The account a Custom deployer pod runs as, made in every namespace by the
/// controller-manager's serviceaccounts pass.
pub const DEPLOYER_SERVICE_ACCOUNT: &str = "deployer";
/// OpenShift's `system:deployer` ClusterRole (bootstrappolicy): what a
/// deployer pod does, scale and watch the versions and read their pods.
pub const DEPLOYER_ROLE: &str = "system:deployer";
/// The RoleBinding OpenShift puts in every namespace for its `deployer`.
pub const DEPLOYERS_BINDING: &str = "system:deployers";

pub fn deployer_role() -> Value {
    json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "ClusterRole",
        "metadata": {"name": DEPLOYER_ROLE,
                     "annotations": {"openshift.io/description": "Grants the right to deploy within a project.  Used primarily with service accounts for automated deployments."}},
        "rules": [
            // `delete`: kept by upstream for older deployer images.
            {"apiGroups": [""], "resources": ["replicationcontrollers"], "verbs": ["delete"]},
            {"apiGroups": [""], "resources": ["replicationcontrollers"], "verbs": ["get", "list", "watch", "update"]},
            {"apiGroups": [""], "resources": ["replicationcontrollers/scale"], "verbs": ["get", "update"]},
            {"apiGroups": [""], "resources": ["pods"], "verbs": ["get", "list", "watch", "create"]},
            {"apiGroups": [""], "resources": ["pods/log"], "verbs": ["get"]},
            {"apiGroups": [""], "resources": ["events"], "verbs": ["create", "list"]},
            {"apiGroups": ["image.openshift.io", ""], "resources": ["imagestreams"], "verbs": ["get"]},
            {"apiGroups": ["image.openshift.io", ""], "resources": ["imagestreamtags", "imagestreammappings"], "verbs": ["create"]},
            {"apiGroups": ["image.openshift.io", ""], "resources": ["imagestreamtags"], "verbs": ["update"]},
        ],
    })
}

pub fn deployers_binding(namespace: &str) -> Value {
    json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "RoleBinding",
        "metadata": {"name": DEPLOYERS_BINDING, "namespace": namespace,
                     "annotations": {"openshift.io/description": "Allows deploymentconfigs in this namespace to rollout pods in this namespace. Configured by the apiserver (#392)."}},
        "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole", "name": DEPLOYER_ROLE},
        "subjects": [{"kind": "ServiceAccount", "name": DEPLOYER_SERVICE_ACCOUNT, "namespace": namespace}],
    })
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
    let tags = hook["tagImages"].as_array().filter(|t| !t.is_empty());
    let exec = &hook["execNewPod"];
    if let Some(tags) = tags {
        if exec.is_object() {
            errs.push(invalid(path, "\"<hook>\"", "only one of execNewPod or tagImages may be specified"));
            return;
        }
        for (i, image) in tags.iter().enumerate() {
            let at = format!("{path}.tagImages[{i}]");
            let c = s(image, "containerName");
            if c.is_empty() {
                errs.push(required(&format!("{at}.containerName"), "a containerName is required"));
            } else if !containers.iter().any(|n| n == c) {
                errs.push(invalid(&format!("{at}.containerName"), &q(c), &format!("no container {c:?} found")));
            }
            let kind = s(&image["to"], "kind");
            if kind != "ImageStreamTag" {
                errs.push(invalid(&format!("{at}.to.kind"), &q(kind), "Must be 'ImageStreamTag'"));
            }
            if s(&image["to"], "name").is_empty() {
                errs.push(required(&format!("{at}.to.name"), "a destination tag name is required"));
            }
        }
        return;
    }
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
        // #392: a deployer pod from the user's image.
        "Custom" => {
            let p = &st["customParams"];
            if !p.is_object() {
                errs.push(required("spec.strategy.customParams", ""));
            } else if s(p, "image").trim().is_empty() {
                errs.push(required("spec.strategy.customParams.image",
                                   "this cluster has no default deployer image; a Custom strategy names its own"));
            }
            for (k, hooks) in [("recreateParams", &["pre", "mid", "post"][..]), ("rollingParams", &["pre", "post"][..])] {
                for h in hooks {
                    validate_hook(&st[k][h], &format!("spec.strategy.{k}.{h}"), &containers, errs);
                }
            }
        }
        other => errs.push(not_supported("spec.strategy.type", other, &STRATEGIES)),
    }
    // `validateCustomParams`: the environment's names.
    for (i, e) in st["customParams"]["environment"].as_array().into_iter().flatten().enumerate() {
        if s(e, "name").is_empty() {
            errs.push(required(&format!("spec.strategy.customParams.environment[{i}].name"), ""));
        }
    }
    if st["customParams"].is_object() && ty != "Custom" && !ty.is_empty() {
        errs.push(format!("spec.strategy.customParams: Forbidden: customParams with a {ty} strategy replaces the deployer image, \
                           and this cluster rolls {ty} out itself; use type Custom to run an image"));
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

/// `validateImageChangeParams`, and the container names it lists against
/// the template.
fn validate_image_change_params(p: &Value, path: &str, containers: &[&str], errs: &mut Vec<String>) {
    let from = &p["from"];
    let name = s(from, "name");
    if name.is_empty() {
        errs.push(required(&format!("{path}.from"), ""));
    } else {
        if s(from, "kind") != "ImageStreamTag" {
            errs.push(invalid(&format!("{path}.from.kind"), &q(s(from, "kind")), "kind must be one of: ImageStreamTag"));
        }
        if !apimachinery::imagestream::valid_tag_name(name) {
            errs.push(invalid(&format!("{path}.from.name"), &q(name), "must be of the form <stream_name>:<tag>"));
        }
        let ns = s(from, "namespace");
        if !ns.is_empty() && !crate::user_groups::is_dns_subdomain(ns) {
            errs.push(invalid(&format!("{path}.from.namespace"), &q(ns), "namespace must be a valid subdomain"));
        }
    }
    let names = p["containerNames"].as_array().cloned().unwrap_or_default();
    if names.is_empty() {
        errs.push(required(&format!("{path}.containerNames"), ""));
    }
    for (j, n) in names.iter().enumerate() {
        let n = n.as_str().unwrap_or("");
        if !containers.contains(&n) {
            errs.push(invalid(&format!("{path}.containerNames[{j}]"), &q(n), &format!("container {n:?} not found in pod template")));
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
    let template_containers: Vec<&str> = ["containers", "initContainers"]
        .iter()
        .flat_map(|l| spec["template"]["spec"][*l].as_array().into_iter().flatten())
        .map(|c| s(c, "name"))
        .collect();
    // Containers an image trigger fills in: their image may be empty.
    let mut triggered: Vec<&str> = Vec::new();
    for (i, t) in spec["triggers"].as_array().into_iter().flatten().enumerate() {
        let at = format!("spec.triggers[{i}]");
        match s(t, "type") {
            "" => errs.push(required(&format!("{at}.type"), "trigger type must be present")),
            "ConfigChange" => {
                if !t["imageChangeParams"].is_null() {
                    errs.push(invalid(&format!("{at}.imageChangeParams"), "{}",
                                      "imageChangeParams must not be present when the type is ConfigChange"));
                }
            }
            "ImageChange" => {
                let p = &t["imageChangeParams"];
                if !p.is_object() {
                    errs.push(required(&format!("{at}.imageChangeParams"), ""));
                    continue;
                }
                validate_image_change_params(p, &format!("{at}.imageChangeParams"), &template_containers, &mut errs);
                triggered.extend(p["containerNames"].as_array().into_iter().flatten().filter_map(Value::as_str));
            }
            other => errs.push(not_supported(&format!("{at}.type"), other, &["ConfigChange", "ImageChange"])),
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
            if s(c, "image").trim().is_empty() && !triggered.contains(&s(c, "name")) {
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

/// The ImageStream a trigger's `from` names, as `processTriggers` reads it.
async fn get_stream<F, Fut>(get: &F, ns: &str, stream: &str) -> Result<Option<Value>, String>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Fetched>,
{
    if stream.is_empty() || stream.contains(['/', '?', '#', '%']) || ns.contains(['/', '?', '#', '%']) {
        return Ok(None);
    }
    match get(format!("/apis/image.openshift.io/v1/namespaces/{ns}/imagestreams/{stream}")).await {
        Fetched::Found(v) => Ok(Some(v)),
        Fetched::NotFound => Ok(None),
        Fetched::Failed(e) => Err(e),
    }
}

/// `processTriggers`: each ImageChange trigger's tag, as the stream has it
/// now, into the containers it names and its `lastTriggeredImage` — every
/// trigger when forced, else the automatic ones of an unpaused config. A
/// stream that is not there is skipped; one that cannot be read fails the
/// request.
pub async fn process_triggers<F, Fut>(get: &F, dc: &mut Value, force: bool) -> Result<(), ApiError>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Fetched>,
{
    let ns = s(&dc["metadata"], "namespace").to_string();
    let paused = dc["spec"]["paused"] == true;
    let n = dc["spec"]["triggers"].as_array().map_or(0, Vec::len);
    let mut errs = Vec::new();
    for i in 0..n {
        let t = dc["spec"]["triggers"][i].clone();
        let p = &t["imageChangeParams"];
        if t["type"] != "ImageChange" || !p.is_object() || (!force && (paused || p["automatic"] != true)) {
            continue;
        }
        let (stream, tag, _) = apimachinery::imagestream::split_tag(s(&p["from"], "name"));
        let from_ns = match s(&p["from"], "namespace") {
            "" => ns.as_str(),
            other => other,
        };
        let found = match get_stream(get, from_ns, stream).await {
            Ok(Some(v)) => v,
            Ok(None) => continue,
            Err(e) => {
                errs.push(e);
                continue;
            }
        };
        let Some(latest) = apimachinery::imagestream::resolve_latest_tagged_image(&found, tag) else { continue };
        let last = s(p, "lastTriggeredImage").to_string();
        if latest == last {
            continue;
        }
        let names: Vec<String> = p["containerNames"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(String::from)).collect();
        let mut set_last = false;
        for list in ["containers", "initContainers"] {
            for c in dc["spec"]["template"]["spec"][list].as_array_mut().into_iter().flatten() {
                if names.iter().any(|n| c["name"] == n.as_str()) && (c["image"] != latest.as_str() || last != latest) {
                    c["image"] = json!(latest);
                    set_last = true;
                }
            }
        }
        if set_last {
            dc["spec"]["triggers"][i]["imageChangeParams"]["lastTriggeredImage"] = json!(latest);
        }
    }
    if errs.is_empty() {
        Ok(())
    } else {
        Err(ApiError::internal(&errs.join("; ")))
    }
}

fn same_from(a: &Value, b: &Value) -> bool {
    a["imageChangeParams"]["from"]["name"] == b["imageChangeParams"]["from"]["name"]
        && s(&a["imageChangeParams"]["from"], "namespace") == s(&b["imageChangeParams"]["from"], "namespace")
}

fn image_triggers(dc: &Value) -> Vec<&Value> {
    dc["spec"]["triggers"].as_array().into_iter().flatten().filter(|t| t["type"] == "ImageChange").collect()
}

/// `canTrigger`: the causes of a new version, or `None`. Every image
/// trigger must have resolved first (a 400 otherwise, forced or not).
fn can_trigger(dc: &Value, decoded: Option<&Value>, force: bool) -> Result<Option<Value>, ApiError> {
    let version = dc["status"]["latestVersion"].as_i64().unwrap_or(0);
    let name = s(&dc["metadata"], "name");
    let (mut count, mut resolved) = (0, 0);
    let mut causes = Vec::new();
    let previous = decoded.map(image_triggers).unwrap_or_default();
    for t in image_triggers(dc) {
        count += 1;
        let p = &t["imageChangeParams"];
        if s(p, "lastTriggeredImage").is_empty() {
            continue;
        }
        resolved += 1;
        if p["automatic"] != true {
            continue;
        }
        let updated_triggers = !previous.iter().any(|pt| same_from(t, pt));
        let different_image = previous
            .iter()
            .find(|pt| same_from(t, pt))
            .is_some_and(|pt| pt["imageChangeParams"]["lastTriggeredImage"] != p["lastTriggeredImage"]);
        if version == 0 || updated_triggers || different_image {
            causes.push(json!({"type": "ImageChange", "imageTrigger": {"from": {
                "kind": "ImageStreamTag", "name": p["from"]["name"], "namespace": p["from"]["namespace"]}}}));
        }
    }
    if count != resolved {
        return Err(ApiError::bad_request(&format!(
            "cannot trigger a deployment for {name:?} because it contains unresolved images"
        )));
    }
    if force {
        return Ok(Some(json!([{"type": "Manual"}])));
    }
    let config_trigger = dc["spec"]["triggers"].as_array().into_iter().flatten().any(|t| t["type"] == "ConfigChange");
    let template_changed = version == 0 || decoded.is_none_or(|d| d["spec"]["template"] != dc["spec"]["template"]);
    if config_trigger && template_changed {
        causes.push(json!({"type": "ConfigChange"}));
    }
    Ok((!causes.is_empty()).then(|| json!(causes)))
}

/// `deploymentconfigs/instantiate` (`oc rollout latest`): image triggers
/// resolved first (`processTriggers`, through `get`, unless the request
/// excludes ImageChange), then a new version when forced, when there is
/// none yet, on a trigger's new image, or when the template differs from
/// the latest version's; `None` (a 204 Status) when nothing would change.
pub async fn instantiate<F, Fut>(storage: &ResourceStorage, get: &F, ns: &str, name: &str, req: &Value) -> Result<Option<Value>, ApiError>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Fetched>,
{
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
        let excluded = req["excludeTriggers"].as_array().into_iter().flatten().any(|t| t == "ImageChange");
        if !excluded {
            process_triggers(get, &mut dc, force).await?;
        }
        let decoded = if version == 0 {
            None
        } else {
            let rc = storage
                .get(&ResourceStorage::namespaced_key("replicationcontrollers", ns, &rc_name(name, version)))
                .await?;
            decode(&rc)
        };
        let Some(causes) = can_trigger(&dc, decoded.as_ref(), force)? else { return Ok(None) };
        let message = match causes[0]["type"].as_str() {
            Some("Manual") => "manual change",
            Some("ImageChange") => "image change",
            _ => "config change",
        };
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
    // Image triggers are turned off, so the rollback is not rolled forward
    // again at once (as upstream's rollback generator does).
    for t in out["spec"]["triggers"].as_array_mut().into_iter().flatten() {
        if t["type"] == "ImageChange" && t["imageChangeParams"].is_object() {
            t["imageChangeParams"]["automatic"] = json!(false);
        }
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
            ("spec.strategy.customParams: Required value", json!({"type": "Custom"})),
            ("spec.strategy.customParams.image: Required value", json!({"type": "Custom", "customParams": {"command": ["x"]}})),
            ("spec.strategy.customParams: Forbidden", json!({"type": "Recreate", "customParams": {"image": "x"}})),
            ("spec.strategy.customParams.environment[0].name: Required value",
             json!({"type": "Custom", "customParams": {"image": "x", "environment": [{"value": "v"}]}})),
            ("spec.strategy.type: Unsupported value: \"Blue\"", json!({"type": "Blue"})),
        ];
        for (want, strategy) in cases {
            let mut d = dc();
            d["spec"]["strategy"] = strategy;
            let e = admit_create(&mut d).unwrap_err();
            assert!(e.message.contains(want), "{}", e.message);
        }
    }

    #[test]
    fn image_change_triggers_and_tag_images_hooks_validate_as_upstreams() {
        let mut d = dc();
        d["spec"]["template"]["spec"]["containers"][0]["image"] = json!("");
        d["spec"]["triggers"] = json!([{"type": "ImageChange", "imageChangeParams": {"automatic": true, "containerNames": ["web"],
                                        "from": {"name": "web:latest"}}}]);
        admit_create(&mut d).unwrap();
        assert_eq!(d["spec"]["triggers"][0]["imageChangeParams"]["from"]["kind"], "ImageStreamTag", "defaulted");
        let cases: Vec<(&str, Value)> = vec![
            ("spec.triggers[0].imageChangeParams: Required value", json!({"type": "ImageChange"})),
            ("spec.triggers[0].imageChangeParams.from.name: Invalid value: \"web\"",
             json!({"type": "ImageChange", "imageChangeParams": {"containerNames": ["web"], "from": {"name": "web"}}})),
            ("spec.triggers[0].imageChangeParams.from.kind: Invalid value: \"DockerImage\"",
             json!({"type": "ImageChange", "imageChangeParams": {"containerNames": ["web"], "from": {"kind": "DockerImage", "name": "a:b"}}})),
            ("spec.triggers[0].imageChangeParams.containerNames: Required value",
             json!({"type": "ImageChange", "imageChangeParams": {"from": {"name": "a:b"}}})),
            ("spec.triggers[0].imageChangeParams.containerNames[0]: Invalid value: \"nope\"",
             json!({"type": "ImageChange", "imageChangeParams": {"containerNames": ["nope"], "from": {"name": "a:b"}}})),
            ("spec.triggers[0].type: Unsupported value: \"Webhook\"", json!({"type": "Webhook"})),
        ];
        for (want, trigger) in cases {
            let mut d = dc();
            d["spec"]["triggers"] = json!([trigger]);
            let e = admit_create(&mut d).unwrap_err();
            assert!(e.message.contains(want), "{want}: {}", e.message);
        }
        // An empty image no trigger fills is still required.
        let mut d = dc();
        d["spec"]["template"]["spec"]["containers"][0]["image"] = json!("");
        assert!(admit_create(&mut d).unwrap_err().message.contains("spec.template.spec.containers[0].image: Required value"));
        // tagImages: admitted, and checked.
        let mut d = dc();
        d["spec"]["strategy"] = json!({"type": "Recreate", "recreateParams": {"pre": {"failurePolicy": "Abort",
            "tagImages": [{"containerName": "web", "to": {"kind": "ImageStreamTag", "name": "a:b"}}]}}});
        admit_create(&mut d).unwrap();
        let mut d = dc();
        d["spec"]["strategy"] = json!({"type": "Recreate", "recreateParams": {"pre": {"failurePolicy": "Abort",
            "tagImages": [{"containerName": "nope", "to": {"kind": "DockerImage"}}]}}});
        let e = admit_create(&mut d).unwrap_err().message;
        for want in ["tagImages[0].containerName: Invalid value: \"nope\"", "tagImages[0].to.kind: Invalid value",
                     "tagImages[0].to.name: Required value"] {
            assert!(e.contains(want), "{want}: {e}");
        }
    }

    const IMG: &str = "node1.lo:5000/demo/web@sha256:1111";

    fn registry(path: String) -> std::future::Ready<Fetched> {
        std::future::ready(match path.as_str() {
            "/apis/image.openshift.io/v1/namespaces/demo/imagestreams/web" => Fetched::Found(json!({
                "metadata": {"name": "web"},
                "status": {"tags": [{"tag": "latest", "items": [{"dockerImageReference": IMG, "image": "sha256:1111"}]}]}})),
            "/apis/image.openshift.io/v1/namespaces/down/imagestreams/web" => Fetched::Failed("connection refused".into()),
            _ => Fetched::NotFound,
        })
    }

    fn triggered(automatic: bool) -> Value {
        let mut d = dc();
        d["spec"]["triggers"] = json!([{"type": "ImageChange", "imageChangeParams": {"automatic": automatic,
            "containerNames": ["web"], "from": {"kind": "ImageStreamTag", "name": "web:latest"}}}]);
        d
    }

    #[tokio::test]
    async fn process_triggers_resolves_through_the_stream() {
        let mut d = triggered(false);
        process_triggers(&registry, &mut d, false).await.unwrap();
        assert_eq!(d["spec"]["template"]["spec"]["containers"][0]["image"], "nginx:1.25", "manual, not forced");
        process_triggers(&registry, &mut d, true).await.unwrap();
        assert_eq!(d["spec"]["template"]["spec"]["containers"][0]["image"], IMG);
        assert_eq!(d["spec"]["triggers"][0]["imageChangeParams"]["lastTriggeredImage"], IMG);
        let mut gone = triggered(true);
        gone["spec"]["triggers"][0]["imageChangeParams"]["from"]["name"] = json!("other:latest");
        process_triggers(&registry, &mut gone, true).await.unwrap();
        assert!(gone["spec"]["triggers"][0]["imageChangeParams"]["lastTriggeredImage"].is_null(), "no such stream: skipped");
        let mut down = triggered(true);
        down["spec"]["triggers"][0]["imageChangeParams"]["from"]["namespace"] = json!("down");
        assert_eq!(process_triggers(&registry, &mut down, true).await.unwrap_err().status, axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn can_trigger_as_upstreams() {
        let mut d = triggered(true);
        admit_create(&mut d).unwrap();
        assert!(can_trigger(&d, None, true).unwrap_err().message.contains("contains unresolved images"));
        d["spec"]["triggers"][0]["imageChangeParams"]["lastTriggeredImage"] = json!(IMG);
        assert_eq!(can_trigger(&d, None, false).unwrap().unwrap()[0]["type"], "ImageChange", "initial version");
        d["status"]["latestVersion"] = json!(1);
        let previous = d.clone();
        assert!(can_trigger(&d, Some(&previous), false).unwrap().is_none(), "same image");
        d["spec"]["triggers"][0]["imageChangeParams"]["lastTriggeredImage"] = json!("other@sha256:2");
        assert_eq!(can_trigger(&d, Some(&previous), false).unwrap().unwrap()[0]["type"], "ImageChange");
        assert_eq!(can_trigger(&d, Some(&previous), true).unwrap().unwrap()[0]["type"], "Manual");
    }

    #[test]
    fn a_custom_strategy_is_admitted_with_its_image() {
        let mut d = dc();
        d["spec"]["strategy"] = json!({"type": "Custom", "customParams": {"image": "quay.io/me/deployer:1",
            "command": ["/deploy"], "environment": [{"name": "MODE", "value": "blue"}]},
            "rollingParams": {"pre": {"failurePolicy": "Abort", "execNewPod": {"containerName": "web", "command": ["true"]}}}});
        admit_create(&mut d).unwrap();
        assert_eq!(d["spec"]["strategy"]["type"], "Custom");
        assert_eq!(d["spec"]["strategy"]["activeDeadlineSeconds"], MAX_DEPLOYMENT_DURATION, "defaulted as for any strategy");
        assert!(d["spec"]["strategy"]["rollingParams"]["timeoutSeconds"].is_number(), "params a Custom image may read are defaulted");
        let mut bad = d.clone();
        bad["spec"]["strategy"]["rollingParams"]["pre"]["execNewPod"]["containerName"] = json!("nope");
        assert!(admit_create(&mut bad).unwrap_err().message.contains("spec.strategy.rollingParams.pre.execNewPod.containerName"));
    }

    #[test]
    fn the_deployer_role_and_its_binding_are_openshifts() {
        let r = deployer_role();
        assert_eq!(r["metadata"]["name"], "system:deployer");
        let grants = |res: &str, verb: &str| r["rules"].as_array().unwrap().iter().any(|x| {
            x["apiGroups"].as_array().unwrap().contains(&json!(""))
                && x["resources"].as_array().unwrap().contains(&json!(res))
                && x["verbs"].as_array().unwrap().contains(&json!(verb))
        });
        for (res, verb) in [("replicationcontrollers", "update"), ("replicationcontrollers", "watch"),
                            ("replicationcontrollers/scale", "update"), ("pods", "list"), ("pods/log", "get"), ("events", "create")] {
            assert!(grants(res, verb), "{verb} {res}");
        }
        assert!(!grants("secrets", "get") && !grants("pods", "delete"));
        let b = deployers_binding("demo");
        assert_eq!((b["metadata"]["name"].clone(), b["metadata"]["namespace"].clone()), (json!("system:deployers"), json!("demo")));
        assert_eq!(b["roleRef"]["name"], "system:deployer");
        assert_eq!(b["subjects"], json!([{"kind": "ServiceAccount", "name": "deployer", "namespace": "demo"}]));
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
