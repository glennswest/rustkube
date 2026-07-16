//! Generic resource CRUD handlers.
//!
//! These handlers work for any K8s resource type. The resource type
//! and namespace are extracted from the URL path.

use crate::error::ApiError;
use crate::handlers::AppState;
use crate::selector;
use crate::storage::ResourceStorage;
use crate::watch::{self, WatchParams};
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

/// GET a single cluster-scoped resource.
pub async fn get_cluster_resource(
    State(state): State<AppState>,
    Path((resource, name)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    let key = ResourceStorage::cluster_key(&resource, &name);
    let obj = state.storage.get(&key).await?;
    Ok(Json(obj))
}

/// GET a single namespace-scoped resource.
pub async fn get_namespaced_resource(
    State(state): State<AppState>,
    Path((namespace, resource, name)): Path<(String, String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    let key = ResourceStorage::namespaced_key(&resource, &namespace, &name);
    let obj = state.storage.get(&key).await?;
    Ok(Json(obj))
}

/// LIST/WATCH cluster-scoped resources.
pub async fn list_cluster_resources(
    State(state): State<AppState>,
    Path(resource): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Response, ApiError> {
    let params = WatchParams::from_query(query.as_deref().unwrap_or(""));
    let prefix = ResourceStorage::cluster_prefix(&resource);

    if params.watch {
        let start_rev = params.resource_version.unwrap_or(0);
        let rx = state.storage.watch(&prefix, start_rev).await?;
        return Ok(watch::watch_response(
            rx,
            params.label_selector,
            params.field_selector,
        ));
    }

    let limit = params.limit.unwrap_or(500);
    let (items, continue_token, revision) = state
        .storage
        .list(&prefix, limit, params.continue_token.as_deref())
        .await?;

    let items = selector::filter_objects(items, &params.label_selector, &params.field_selector);

    let kind = resource_to_list_kind(&resource);
    let mut list = json!({
        "apiVersion": "v1",
        "kind": kind,
        "metadata": {
            "resourceVersion": revision.to_string()
        },
        "items": items
    });

    if let Some(token) = continue_token {
        list["metadata"]["continue"] = Value::String(token);
    }

    Ok(Json(list).into_response())
}

/// LIST/WATCH namespace-scoped resources in a single namespace.
pub async fn list_namespaced_resources(
    State(state): State<AppState>,
    Path((namespace, resource)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> Result<Response, ApiError> {
    let params = WatchParams::from_query(query.as_deref().unwrap_or(""));
    let prefix = ResourceStorage::namespace_prefix(&resource, &namespace);

    if params.watch {
        let start_rev = params.resource_version.unwrap_or(0);
        let rx = state.storage.watch(&prefix, start_rev).await?;
        return Ok(watch::watch_response(
            rx,
            params.label_selector,
            params.field_selector,
        ));
    }

    let limit = params.limit.unwrap_or(500);
    let (items, continue_token, revision) = state
        .storage
        .list(&prefix, limit, params.continue_token.as_deref())
        .await?;

    let items = selector::filter_objects(items, &params.label_selector, &params.field_selector);

    let kind = resource_to_list_kind(&resource);
    let mut list = json!({
        "apiVersion": "v1",
        "kind": kind,
        "metadata": {
            "resourceVersion": revision.to_string()
        },
        "items": items
    });

    if let Some(token) = continue_token {
        list["metadata"]["continue"] = Value::String(token);
    }

    Ok(Json(list).into_response())
}

/// LIST namespace-scoped resources across all namespaces.
pub async fn list_all_namespaces_resources(
    State(state): State<AppState>,
    Path(resource): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Response, ApiError> {
    let params = WatchParams::from_query(query.as_deref().unwrap_or(""));
    let prefix = ResourceStorage::all_namespaces_prefix(&resource);

    if params.watch {
        let start_rev = params.resource_version.unwrap_or(0);
        let rx = state.storage.watch(&prefix, start_rev).await?;
        return Ok(watch::watch_response(
            rx,
            params.label_selector,
            params.field_selector,
        ));
    }

    let limit = params.limit.unwrap_or(500);
    let (items, continue_token, revision) = state
        .storage
        .list(&prefix, limit, params.continue_token.as_deref())
        .await?;

    let items = selector::filter_objects(items, &params.label_selector, &params.field_selector);

    let kind = resource_to_list_kind(&resource);
    let mut list = json!({
        "apiVersion": "v1",
        "kind": kind,
        "metadata": {
            "resourceVersion": revision.to_string()
        },
        "items": items
    });

    if let Some(token) = continue_token {
        list["metadata"]["continue"] = Value::String(token);
    }

    Ok(Json(list).into_response())
}

/// POST — create a cluster-scoped resource.
pub async fn create_cluster_resource(
    State(state): State<AppState>,
    Path(resource): Path<String>,
    Json(mut body): Json<Value>,
) -> Result<impl IntoResponse, ApiError> {
    let name = body["metadata"]["name"]
        .as_str()
        .ok_or_else(|| ApiError::invalid("metadata.name is required"))?
        .to_string();

    ensure_metadata(&mut body, &name, None);

    crate::builtin_admission::admit_create(&state.storage, &resource, None, &mut body).await?;

    let key = ResourceStorage::cluster_key(&resource, &name);
    let obj = state.storage.create(&key, body).await?;
    Ok((StatusCode::CREATED, Json(obj)))
}

/// POST — create a namespace-scoped resource.
pub async fn create_namespaced_resource(
    State(state): State<AppState>,
    Path((namespace, resource)): Path<(String, String)>,
    Json(mut body): Json<Value>,
) -> Result<impl IntoResponse, ApiError> {
    let name = body["metadata"]["name"]
        .as_str()
        .ok_or_else(|| ApiError::invalid("metadata.name is required"))?
        .to_string();

    ensure_metadata(&mut body, &name, Some(&namespace));

    // Built-in admission (NamespaceLifecycle, ServiceAccount, DefaultTolerationSeconds).
    crate::builtin_admission::admit_create(&state.storage, &resource, Some(&namespace), &mut body)
        .await?;

    let key = ResourceStorage::namespaced_key(&resource, &namespace, &name);
    let obj = state.storage.create(&key, body).await?;
    Ok((StatusCode::CREATED, Json(obj)))
}

/// PUT — update a cluster-scoped resource.
pub async fn update_cluster_resource(
    State(state): State<AppState>,
    Path((resource, name)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<impl IntoResponse, ApiError> {
    let prev_rev = body["metadata"]["resourceVersion"]
        .as_str()
        .and_then(|rv| rv.parse::<u64>().ok());

    let key = ResourceStorage::cluster_key(&resource, &name);
    let obj = state.storage.update(&key, body, prev_rev).await?;
    Ok(Json(obj))
}

/// PUT — update a namespace-scoped resource.
pub async fn update_namespaced_resource(
    State(state): State<AppState>,
    Path((namespace, resource, name)): Path<(String, String, String)>,
    Json(body): Json<Value>,
) -> Result<impl IntoResponse, ApiError> {
    let prev_rev = body["metadata"]["resourceVersion"]
        .as_str()
        .and_then(|rv| rv.parse::<u64>().ok());

    let key = ResourceStorage::namespaced_key(&resource, &namespace, &name);
    let obj = state.storage.update(&key, body, prev_rev).await?;
    Ok(Json(obj))
}

/// DELETE a cluster-scoped resource.
pub async fn delete_cluster_resource(
    State(state): State<AppState>,
    Path((resource, name)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    let key = ResourceStorage::cluster_key(&resource, &name);
    // Get the object first so we can return it
    let _obj = state.storage.get(&key).await?;
    state.storage.delete(&key, None).await?;

    let status = json!({
        "apiVersion": "v1",
        "kind": "Status",
        "metadata": {},
        "status": "Success",
        "details": {
            "name": name,
            "kind": resource
        }
    });
    Ok(Json(status))
}

/// DELETE a namespace-scoped resource.
pub async fn delete_namespaced_resource(
    State(state): State<AppState>,
    Path((namespace, resource, name)): Path<(String, String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    let key = ResourceStorage::namespaced_key(&resource, &namespace, &name);
    state.storage.delete(&key, None).await?;

    let status = json!({
        "apiVersion": "v1",
        "kind": "Status",
        "metadata": {},
        "status": "Success",
        "details": {
            "name": name,
            "namespace": namespace,
            "kind": resource
        }
    });
    Ok(Json(status))
}

// --- Status subresource handlers ---

/// GET status for a cluster-scoped resource.
pub async fn get_cluster_status(
    State(state): State<AppState>,
    Path((resource, name)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    let key = ResourceStorage::cluster_key(&resource, &name);
    let obj = state.storage.get(&key).await?;
    Ok(Json(obj))
}

/// PUT status for a cluster-scoped resource.
pub async fn update_cluster_status(
    State(state): State<AppState>,
    Path((resource, name)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<impl IntoResponse, ApiError> {
    let key = ResourceStorage::cluster_key(&resource, &name);
    let mut existing = state.storage.get(&key).await?;

    // Only update the status field, preserve everything else
    if let Some(status) = body.get("status") {
        existing["status"] = status.clone();
    }

    let prev_rev = existing["metadata"]["resourceVersion"]
        .as_str()
        .and_then(|rv| rv.parse::<u64>().ok());
    let obj = state.storage.update(&key, existing, prev_rev).await?;
    Ok(Json(obj))
}

/// PATCH status for a cluster-scoped resource.
pub async fn patch_cluster_status(
    State(state): State<AppState>,
    Path((resource, name)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<impl IntoResponse, ApiError> {
    let key = ResourceStorage::cluster_key(&resource, &name);
    let mut existing = state.storage.get(&key).await?;

    // Merge the patch into the status field
    if let Some(status_patch) = body.get("status") {
        merge_json(&mut existing["status"], status_patch);
    }

    let prev_rev = existing["metadata"]["resourceVersion"]
        .as_str()
        .and_then(|rv| rv.parse::<u64>().ok());
    let obj = state.storage.update(&key, existing, prev_rev).await?;
    Ok(Json(obj))
}

/// GET status for a namespace-scoped resource.
pub async fn get_namespaced_status(
    State(state): State<AppState>,
    Path((namespace, resource, name)): Path<(String, String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    let key = ResourceStorage::namespaced_key(&resource, &namespace, &name);
    let obj = state.storage.get(&key).await?;
    Ok(Json(obj))
}

/// PUT status for a namespace-scoped resource.
pub async fn update_namespaced_status(
    State(state): State<AppState>,
    Path((namespace, resource, name)): Path<(String, String, String)>,
    Json(body): Json<Value>,
) -> Result<impl IntoResponse, ApiError> {
    let key = ResourceStorage::namespaced_key(&resource, &namespace, &name);
    let mut existing = state.storage.get(&key).await?;

    if let Some(status) = body.get("status") {
        existing["status"] = status.clone();
    }

    let prev_rev = existing["metadata"]["resourceVersion"]
        .as_str()
        .and_then(|rv| rv.parse::<u64>().ok());
    let obj = state.storage.update(&key, existing, prev_rev).await?;
    Ok(Json(obj))
}

/// PATCH status for a namespace-scoped resource.
pub async fn patch_namespaced_status(
    State(state): State<AppState>,
    Path((namespace, resource, name)): Path<(String, String, String)>,
    Json(body): Json<Value>,
) -> Result<impl IntoResponse, ApiError> {
    let key = ResourceStorage::namespaced_key(&resource, &namespace, &name);
    let mut existing = state.storage.get(&key).await?;

    if let Some(status_patch) = body.get("status") {
        merge_json(&mut existing["status"], status_patch);
    }

    let prev_rev = existing["metadata"]["resourceVersion"]
        .as_str()
        .and_then(|rv| rv.parse::<u64>().ok());
    let obj = state.storage.update(&key, existing, prev_rev).await?;
    Ok(Json(obj))
}

/// Recursively merge src JSON into dst.
fn merge_json(dst: &mut Value, src: &Value) {
    match (dst, src) {
        (Value::Object(dst_map), Value::Object(src_map)) => {
            for (key, value) in src_map {
                merge_json(dst_map.entry(key.clone()).or_insert(Value::Null), value);
            }
        }
        (dst, src) => {
            *dst = src.clone();
        }
    }
}

/// Public version of ensure_metadata for use by other modules (e.g. CRD handlers).
pub fn ensure_metadata_pub(obj: &mut Value, name: &str, namespace: Option<&str>) {
    ensure_metadata(obj, name, namespace);
}

/// Ensure metadata fields are set. Defensive: a body or `metadata` that isn't a
/// JSON object must never panic the apiserver (a client request could send any
/// shape) — see rustkube#9.
fn ensure_metadata(obj: &mut Value, name: &str, namespace: Option<&str>) {
    let Some(root) = obj.as_object_mut() else {
        return;
    };
    let meta_val = root.entry("metadata").or_insert_with(|| json!({}));
    if !meta_val.is_object() {
        *meta_val = json!({});
    }
    let Some(meta) = meta_val.as_object_mut() else {
        return;
    };

    meta.entry("name").or_insert_with(|| Value::String(name.to_string()));

    if let Some(ns) = namespace {
        meta.entry("namespace")
            .or_insert_with(|| Value::String(ns.to_string()));
    }

    if !meta.contains_key("uid") {
        meta.insert("uid".into(), Value::String(uuid::Uuid::new_v4().to_string()));
    }

    if !meta.contains_key("creationTimestamp") {
        meta.insert(
            "creationTimestamp".into(),
            Value::String(chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()),
        );
    }
}

/// Convert resource name to list kind (e.g., "nodes" → "NodeList").
fn resource_to_list_kind(resource: &str) -> String {
    let singular = match resource {
        "namespaces" => "Namespace",
        "nodes" => "Node",
        "pods" => "Pod",
        "services" => "Service",
        "endpoints" => "Endpoints",
        "configmaps" => "ConfigMap",
        "secrets" => "Secret",
        "serviceaccounts" => "ServiceAccount",
        "events" => "Event",
        "persistentvolumeclaims" => "PersistentVolumeClaim",
        "persistentvolumes" => "PersistentVolume",
        "deployments" => "Deployment",
        "replicasets" => "ReplicaSet",
        "statefulsets" => "StatefulSet",
        "daemonsets" => "DaemonSet",
        "jobs" => "Job",
        "cronjobs" => "CronJob",
        "leases" => "Lease",
        "customresourcedefinitions" => "CustomResourceDefinition",
        "clusterroles" => "ClusterRole",
        "clusterrolebindings" => "ClusterRoleBinding",
        "roles" => "Role",
        "rolebindings" => "RoleBinding",
        "horizontalpodautoscalers" => "HorizontalPodAutoscaler",
        "networkpolicies" => "NetworkPolicy",
        "ingresses" => "Ingress",
        "ingressclasses" => "IngressClass",
        "mutatingwebhookconfigurations" => "MutatingWebhookConfiguration",
        "validatingwebhookconfigurations" => "ValidatingWebhookConfiguration",
        "gatewayclasses" => "GatewayClass",
        "gateways" => "Gateway",
        "httproutes" => "HTTPRoute",
        "apiservices" => "APIService",
        "podmigrations" => "PodMigration",
        other => other,
    };
    format!("{singular}List")
}

#[cfg(test)]
mod tests {
    use super::*;

    // rustkube#9: a client request with an unexpected shape must never panic.
    #[test]
    fn ensure_metadata_never_panics_on_bad_shapes() {
        // Top-level body not an object (array / scalar / null).
        for mut v in [json!([1, 2, 3]), json!("nope"), json!(42), json!(null)] {
            ensure_metadata(&mut v, "x", Some("default")); // must not panic
        }
        // metadata present but not an object → coerced, then populated.
        let mut v = json!({"metadata": "not-an-object", "status": {"phase": "Failed"}});
        ensure_metadata(&mut v, "pod1", Some("default"));
        assert_eq!(v["metadata"]["name"], "pod1");
        assert_eq!(v["metadata"]["namespace"], "default");

        // metadata as an array → coerced to object.
        let mut v = json!({"metadata": []});
        ensure_metadata(&mut v, "pod2", None);
        assert!(v["metadata"].is_object());
        assert_eq!(v["metadata"]["name"], "pod2");
    }

    // A kubelet-shaped pod-status PUT (the exact write that took the cluster
    // down) must be handled without panicking.
    #[test]
    fn kubelet_pod_status_put_shape_is_safe() {
        let mut pod = json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {"name": "test", "namespace": "default"},
            "spec": {"nodeName": "rknode1"},
            "status": {
                "phase": "Failed",
                "conditions": [{"type": "Ready", "status": "False"}],
                "containerStatuses": [{
                    "name": "c", "ready": false, "restartCount": 0,
                    "state": {"terminated": {"exitCode": 1, "reason": "Error"}}
                }]
            }
        });
        ensure_metadata(&mut pod, "test", Some("default")); // must not panic
        assert_eq!(pod["status"]["phase"], "Failed");
    }
}
