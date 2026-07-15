//! Built-in admission plugins — the always-on chain upstream enables by default,
//! run before persistence on creates. Implements a first, high-value subset:
//!
//! - **NamespaceLifecycle** (validating): reject writes into a namespace that is
//!   missing or being terminated.
//! - **ServiceAccount** (mutating): default a Pod's `serviceAccountName` to
//!   `default`.
//! - **DefaultTolerationSeconds** (mutating): add the not-ready/unreachable
//!   NoExecute tolerations (300s) to Pods that lack them.

use crate::error::ApiError;
use crate::storage::ResourceStorage;
use serde_json::{json, Value};

/// Run built-in admission for a create. Mutates `obj` in place; an `Err`
/// rejects the request.
pub async fn admit_create(
    storage: &ResourceStorage,
    resource: &str,
    namespace: Option<&str>,
    obj: &mut Value,
) -> Result<(), ApiError> {
    // NamespaceLifecycle — namespaced resources (other than Namespaces) require
    // an existing, non-terminating namespace.
    let ns_obj = if let Some(ns) = namespace {
        if resource != "namespaces" {
            Some(namespace_lifecycle(storage, ns).await?)
        } else {
            None
        }
    } else {
        None
    };

    if resource == "pods" {
        service_account_default(obj);
        default_toleration_seconds(obj);
        // PodSecurity — validate against the namespace's enforce level.
        if let Some(ns_obj) = &ns_obj {
            let level = ns_obj["metadata"]["labels"]
                ["pod-security.kubernetes.io/enforce"]
                .as_str()
                .unwrap_or("");
            pod_security(level, obj)?;
        }
    }
    Ok(())
}

async fn namespace_lifecycle(
    storage: &ResourceStorage,
    ns: &str,
) -> Result<serde_json::Value, ApiError> {
    let key = ResourceStorage::cluster_key("namespaces", ns);
    match storage.get(&key).await {
        Ok(nsobj) => {
            if nsobj["status"]["phase"].as_str() == Some("Terminating") {
                return Err(ApiError::forbidden(&format!(
                    "unable to create new content in namespace {ns} because it is being terminated"
                )));
            }
            Ok(nsobj)
        }
        Err(_) => Err(ApiError::forbidden(&format!("namespace {ns} not found"))),
    }
}

/// PodSecurity admission — a subset of the baseline/restricted checks, keyed on
/// the namespace's `pod-security.kubernetes.io/enforce` level.
fn pod_security(level: &str, obj: &Value) -> Result<(), ApiError> {
    if level.is_empty() || level == "privileged" {
        return Ok(());
    }
    let spec = &obj["spec"];

    // baseline + restricted: no host namespaces, no hostPath volumes.
    for key in ["hostNetwork", "hostPID", "hostIPC"] {
        if spec[key].as_bool() == Some(true) {
            return Err(ApiError::forbidden(&format!(
                "pod security \"{level}\": {key} is not allowed"
            )));
        }
    }
    if let Some(vols) = spec["volumes"].as_array() {
        if vols.iter().any(|v| !v["hostPath"].is_null()) {
            return Err(ApiError::forbidden(&format!(
                "pod security \"{level}\": hostPath volumes are not allowed"
            )));
        }
    }

    let empty = vec![];
    let containers = spec["containers"].as_array().unwrap_or(&empty);
    let init = spec["initContainers"].as_array().unwrap_or(&empty);
    for c in containers.iter().chain(init.iter()) {
        let sc = &c["securityContext"];
        if sc["privileged"].as_bool() == Some(true) {
            return Err(ApiError::forbidden(&format!(
                "pod security \"{level}\": privileged containers are not allowed"
            )));
        }
        if level == "restricted" {
            if sc["allowPrivilegeEscalation"].as_bool() != Some(false) {
                return Err(ApiError::forbidden(
                    "pod security \"restricted\": allowPrivilegeEscalation must be false",
                ));
            }
            let drops_all = sc["capabilities"]["drop"]
                .as_array()
                .map(|d| d.iter().any(|x| x.as_str() == Some("ALL")))
                .unwrap_or(false);
            if !drops_all {
                return Err(ApiError::forbidden(
                    "pod security \"restricted\": containers must drop ALL capabilities",
                ));
            }
        }
    }

    if level == "restricted" {
        let pod_nonroot = spec["securityContext"]["runAsNonRoot"].as_bool() == Some(true);
        for c in containers.iter().chain(init.iter()) {
            let c_nonroot = c["securityContext"]["runAsNonRoot"].as_bool() == Some(true);
            if !pod_nonroot && !c_nonroot {
                return Err(ApiError::forbidden(
                    "pod security \"restricted\": runAsNonRoot must be true",
                ));
            }
        }
    }
    Ok(())
}

fn service_account_default(obj: &mut Value) {
    if let Some(spec) = obj.get_mut("spec").and_then(|s| s.as_object_mut()) {
        let unset = spec
            .get("serviceAccountName")
            .and_then(|v| v.as_str())
            .map(|s| s.is_empty())
            .unwrap_or(true);
        if unset {
            spec.insert("serviceAccountName".into(), json!("default"));
        }
        // Mirror to the deprecated `serviceAccount` field for compatibility.
        let name = spec
            .get("serviceAccountName")
            .cloned()
            .unwrap_or_else(|| json!("default"));
        spec.insert("serviceAccount".into(), name);
    }
}

fn default_toleration_seconds(obj: &mut Value) {
    let Some(spec) = obj.get_mut("spec").and_then(|s| s.as_object_mut()) else {
        return;
    };
    let tols = spec.entry("tolerations").or_insert_with(|| json!([]));
    let Some(arr) = tols.as_array_mut() else { return };
    for key in [
        "node.kubernetes.io/not-ready",
        "node.kubernetes.io/unreachable",
    ] {
        let present = arr.iter().any(|t| t["key"].as_str() == Some(key));
        if !present {
            arr.push(json!({
                "key": key,
                "operator": "Exists",
                "effect": "NoExecute",
                "tolerationSeconds": 300
            }));
        }
    }
}
