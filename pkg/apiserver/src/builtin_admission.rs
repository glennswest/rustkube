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
    if let Some(ns) = namespace {
        if resource != "namespaces" {
            namespace_lifecycle(storage, ns).await?;
        }
    }

    if resource == "pods" {
        service_account_default(obj);
        default_toleration_seconds(obj);
    }
    Ok(())
}

async fn namespace_lifecycle(storage: &ResourceStorage, ns: &str) -> Result<(), ApiError> {
    let key = ResourceStorage::cluster_key("namespaces", ns);
    match storage.get(&key).await {
        Ok(nsobj) => {
            if nsobj["status"]["phase"].as_str() == Some("Terminating") {
                return Err(ApiError::forbidden(&format!(
                    "unable to create new content in namespace {ns} because it is being terminated"
                )));
            }
            Ok(())
        }
        Err(_) => Err(ApiError::forbidden(&format!(
            "namespace {ns} not found"
        ))),
    }
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
