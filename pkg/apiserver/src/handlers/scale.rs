//! The `scale` subresource, `autoscaling/v1` Scale (#86): what `kubectl
//! scale`, `oc scale` and upstream-shaped autoscalers read and write.
//!
//! Served for Deployments, ReplicaSets and StatefulSets, and for custom
//! resources whose CRD version declares `subresources.scale`. A Scale is a
//! view of its object: `spec.replicas` from the object's replicas path,
//! `status.replicas` from its status path, `status.selector` as a label
//! selector string, and the object's name, namespace, uid and
//! resourceVersion. A write changes only the replicas, through the object's
//! guaranteed update — conditional on the Scale's `resourceVersion` when it
//! carries one, admitted as an update of the object.

use super::resource::{apply_patch_body, guaranteed_update};
use super::AppState;
use crate::error::ApiError;
use crate::storage::ResourceStorage;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::Json;
use serde_json::{json, Value};

/// Where a scalable object keeps what a Scale shows, as JSON pointers.
#[derive(Clone, Debug, PartialEq)]
pub struct ScalePaths {
    pub spec_replicas: String,
    pub status_replicas: String,
    /// A string field holding the selector (custom resources), or None for
    /// the built-ins' `spec.selector` LabelSelector.
    pub label_selector: Option<String>,
}

impl ScalePaths {
    fn builtin() -> Self {
        Self { spec_replicas: "/spec/replicas".into(), status_replicas: "/status/replicas".into(), label_selector: None }
    }

    /// From a CRD's `subresources.scale` (`.spec.replicas`-style paths).
    pub fn from_crd(scale: &Value) -> Option<Self> {
        let pointer = |p: &str| format!("/{}", p.trim_start_matches('.').replace('.', "/"));
        Some(Self {
            spec_replicas: pointer(scale["specReplicasPath"].as_str()?),
            status_replicas: pointer(scale["statusReplicasPath"].as_str()?),
            label_selector: scale["labelSelectorPath"].as_str().map(pointer),
        })
    }
}

/// A LabelSelector as the string `kubectl` and `status.selector` use.
pub fn selector_string(sel: &Value) -> String {
    // A ReplicationController's selector is a plain label map (#125).
    if sel.get("matchLabels").is_none() && sel.get("matchExpressions").is_none() {
        if let Some(m) = sel.as_object() {
            return m.iter().map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or(""))).collect::<Vec<_>>().join(",");
        }
    }
    let mut parts: Vec<String> = sel["matchLabels"]
        .as_object()
        .map(|m| m.iter().map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or(""))).collect())
        .unwrap_or_default();
    for e in sel["matchExpressions"].as_array().into_iter().flatten() {
        let key = e["key"].as_str().unwrap_or("");
        let values: Vec<&str> = e["values"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        parts.push(match e["operator"].as_str().unwrap_or("") {
            "In" => format!("{key} in ({})", values.join(",")),
            "NotIn" => format!("{key} notin ({})", values.join(",")),
            "Exists" => key.to_string(),
            "DoesNotExist" => format!("!{key}"),
            _ => continue,
        });
    }
    parts.join(",")
}

/// The Scale view of `obj`.
pub fn to_scale(obj: &Value, paths: &ScalePaths) -> Value {
    let meta = &obj["metadata"];
    let selector = match &paths.label_selector {
        Some(p) => obj.pointer(p).and_then(Value::as_str).unwrap_or("").to_string(),
        None => selector_string(&obj["spec"]["selector"]),
    };
    let mut scale = json!({
        "apiVersion": "autoscaling/v1",
        "kind": "Scale",
        "metadata": {"name": meta["name"], "namespace": meta["namespace"], "uid": meta["uid"],
                     "resourceVersion": meta["resourceVersion"], "creationTimestamp": meta["creationTimestamp"]},
        "spec": {"replicas": obj.pointer(&paths.spec_replicas).and_then(Value::as_i64).unwrap_or(0)},
        "status": {"replicas": obj.pointer(&paths.status_replicas).and_then(Value::as_i64).unwrap_or(0)},
    });
    if !selector.is_empty() {
        scale["status"]["selector"] = json!(selector);
    }
    if meta["namespace"].is_null() {
        scale["metadata"].as_object_mut().unwrap().remove("namespace");
    }
    scale
}

fn replicas_of(scale: &Value) -> Result<i64, ApiError> {
    match scale["spec"]["replicas"].as_i64() {
        Some(r) if r >= 0 => Ok(r),
        Some(r) => Err(ApiError::invalid(&format!("spec.replicas: Invalid value: {r}: must be greater than or equal to 0"))),
        None => Err(ApiError::invalid("spec.replicas: Required value")),
    }
}

/// Set the replicas at `pointer`, creating the parents a pointer needs.
fn set_replicas(obj: &mut Value, pointer: &str, replicas: i64) {
    let mut cur = obj;
    let parts: Vec<&str> = pointer.trim_start_matches('/').split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        if !cur.is_object() {
            *cur = json!({});
        }
        if i == parts.len() - 1 {
            cur[*part] = json!(replicas);
            return;
        }
        cur = &mut cur[*part];
    }
}

async fn get(state: &AppState, key: &str, paths: &ScalePaths) -> Result<Json<Value>, ApiError> {
    Ok(Json(to_scale(&state.storage.get(key).await?, paths)))
}

async fn put(state: &AppState, key: &str, paths: &ScalePaths, body: &Value) -> Result<Json<Value>, ApiError> {
    let replicas = replicas_of(body)?;
    let precondition = body["metadata"]["resourceVersion"].as_str().filter(|v| !v.is_empty()).map(str::to_string);
    let p = paths.spec_replicas.clone();
    let obj = guaranteed_update(state, key, precondition, |mut o| {
        set_replicas(&mut o, &p, replicas);
        Ok(o)
    })
    .await?;
    Ok(Json(to_scale(&obj, paths)))
}

async fn patch(state: &AppState, key: &str, paths: &ScalePaths, headers: &HeaderMap, body: &[u8]) -> Result<Json<Value>, ApiError> {
    let ct = headers.get(axum::http::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let p = paths.clone();
    let obj = guaranteed_update(state, key, None, |mut o| {
        let mut scale = to_scale(&o, &p);
        apply_patch_body(&mut scale, &ct, body)?;
        set_replicas(&mut o, &p.spec_replicas, replicas_of(&scale)?);
        Ok(o)
    })
    .await?;
    Ok(Json(to_scale(&obj, paths)))
}

fn apps_key(namespace: &str, resource: &str, name: &str) -> Result<String, ApiError> {
    match resource {
        "deployments" | "replicasets" | "statefulsets" => Ok(ResourceStorage::namespaced_key(resource, namespace, name)),
        _ => Err(ApiError::not_found(&format!("{resource}/scale"), name)),
    }
}

/// GET /apis/apps/v1/namespaces/{ns}/{resource}/{name}/scale
pub async fn get_apps(
    State(state): State<AppState>,
    Path((namespace, resource, name)): Path<(String, String, String)>,
) -> Result<Json<Value>, ApiError> {
    get(&state, &apps_key(&namespace, &resource, &name)?, &ScalePaths::builtin()).await
}

/// PUT …/scale
pub async fn put_apps(
    State(state): State<AppState>,
    Path((namespace, resource, name)): Path<(String, String, String)>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    put(&state, &apps_key(&namespace, &resource, &name)?, &ScalePaths::builtin(), &body).await
}

/// PATCH …/scale (merge, strategic, JSON patch, apply) on the Scale view.
pub async fn patch_apps(
    State(state): State<AppState>,
    Path((namespace, resource, name)): Path<(String, String, String)>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    patch(&state, &apps_key(&namespace, &resource, &name)?, &ScalePaths::builtin(), &headers, &body).await
}

fn rc_key(namespace: &str, name: &str) -> String {
    ResourceStorage::namespaced_key("replicationcontrollers", namespace, name)
}

/// GET /api/v1/namespaces/{ns}/replicationcontrollers/{name}/scale (#125)
pub async fn get_rc(
    State(state): State<AppState>,
    Path((namespace, name)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    get(&state, &rc_key(&namespace, &name), &ScalePaths::builtin()).await
}

pub async fn put_rc(
    State(state): State<AppState>,
    Path((namespace, name)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    put(&state, &rc_key(&namespace, &name), &ScalePaths::builtin(), &body).await
}

pub async fn patch_rc(
    State(state): State<AppState>,
    Path((namespace, name)): Path<(String, String)>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    patch(&state, &rc_key(&namespace, &name), &ScalePaths::builtin(), &headers, &body).await
}

/// A custom resource's key and scale paths, or 404 when its CRD version
/// declares no scale subresource.
async fn cr(state: &AppState, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str)
    -> Result<(String, ScalePaths), ApiError>
{
    let def = state.crd_registry.lookup(group, version, resource).await;
    let Some(paths) = def.and_then(|d| d.scale) else {
        return Err(ApiError::not_found(&format!("{resource}/scale"), name));
    };
    let res = ResourceStorage::custom_resource(group, resource);
    let key = match namespace {
        Some(ns) => ResourceStorage::namespaced_key(&res, ns, name),
        None => ResourceStorage::cluster_key(&res, name),
    };
    Ok((key, paths))
}

pub async fn get_cr_ns(
    State(state): State<AppState>,
    Path((group, version, namespace, resource, name)): Path<(String, String, String, String, String)>,
) -> Result<Json<Value>, ApiError> {
    let (key, paths) = cr(&state, &group, &version, &resource, Some(&namespace), &name).await?;
    get(&state, &key, &paths).await
}

pub async fn put_cr_ns(
    State(state): State<AppState>,
    Path((group, version, namespace, resource, name)): Path<(String, String, String, String, String)>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let (key, paths) = cr(&state, &group, &version, &resource, Some(&namespace), &name).await?;
    put(&state, &key, &paths, &body).await
}

pub async fn patch_cr_ns(
    State(state): State<AppState>,
    Path((group, version, namespace, resource, name)): Path<(String, String, String, String, String)>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    let (key, paths) = cr(&state, &group, &version, &resource, Some(&namespace), &name).await?;
    patch(&state, &key, &paths, &headers, &body).await
}

pub async fn get_cr_cluster(
    State(state): State<AppState>,
    Path((group, version, resource, name)): Path<(String, String, String, String)>,
) -> Result<Json<Value>, ApiError> {
    let (key, paths) = cr(&state, &group, &version, &resource, None, &name).await?;
    get(&state, &key, &paths).await
}

pub async fn put_cr_cluster(
    State(state): State<AppState>,
    Path((group, version, resource, name)): Path<(String, String, String, String)>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let (key, paths) = cr(&state, &group, &version, &resource, None, &name).await?;
    put(&state, &key, &paths, &body).await
}

pub async fn patch_cr_cluster(
    State(state): State<AppState>,
    Path((group, version, resource, name)): Path<(String, String, String, String)>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    let (key, paths) = cr(&state, &group, &version, &resource, None, &name).await?;
    patch(&state, &key, &paths, &headers, &body).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_replication_controller_s_map_selector_reads_as_a_string() {
        let rc = json!({"metadata": {"name": "rc", "namespace": "ns"},
            "spec": {"replicas": 2, "selector": {"app": "web", "tier": "a"}}, "status": {"replicas": 1}});
        let s = to_scale(&rc, &ScalePaths::builtin());
        assert_eq!(s["status"]["selector"], "app=web,tier=a");
        assert_eq!(s["spec"]["replicas"], 2);
    }

    #[test]
    fn a_deployment_reads_as_a_scale() {
        let d = json!({"metadata": {"name": "web", "namespace": "ns", "uid": "u1", "resourceVersion": "42"},
            "spec": {"replicas": 3, "selector": {"matchLabels": {"app": "web"},
                "matchExpressions": [{"key": "tier", "operator": "In", "values": ["a", "b"]}, {"key": "x", "operator": "DoesNotExist"}]}},
            "status": {"replicas": 2}});
        let s = to_scale(&d, &ScalePaths::builtin());
        assert_eq!((s["kind"].as_str(), s["apiVersion"].as_str()), (Some("Scale"), Some("autoscaling/v1")));
        assert_eq!((s["spec"]["replicas"].as_i64(), s["status"]["replicas"].as_i64()), (Some(3), Some(2)));
        assert_eq!(s["status"]["selector"], "app=web,tier in (a,b),!x");
        assert_eq!(s["metadata"]["resourceVersion"], "42");
    }

    #[test]
    fn a_custom_resource_scales_by_its_crds_paths() {
        let paths = ScalePaths::from_crd(&json!({"specReplicasPath": ".spec.size", "statusReplicasPath": ".status.size",
                                                "labelSelectorPath": ".status.selector"})).unwrap();
        assert_eq!(paths.spec_replicas, "/spec/size");
        let mut o = json!({"metadata": {"name": "c"}, "spec": {"size": 1}, "status": {"size": 1, "selector": "app=c"}});
        let s = to_scale(&o, &paths);
        assert_eq!((s["spec"]["replicas"].as_i64(), s["status"]["selector"].as_str()), (Some(1), Some("app=c")));
        assert!(s["metadata"].get("namespace").is_none(), "cluster-scoped: no namespace");
        set_replicas(&mut o, &paths.spec_replicas, 4);
        assert_eq!(o["spec"]["size"], 4);
        let mut bare = json!({"metadata": {}});
        set_replicas(&mut bare, "/spec/replicas", 2);
        assert_eq!(bare["spec"]["replicas"], 2);
        assert!(ScalePaths::from_crd(&json!({"specReplicasPath": ".spec.size"})).is_none(), "both replicas paths are required");
    }

    #[test]
    fn a_patch_of_the_scale_view_and_its_validation() {
        let mut s = to_scale(&json!({"metadata": {"name": "w"}, "spec": {"replicas": 1}}), &ScalePaths::builtin());
        apply_patch_body(&mut s, "application/merge-patch+json", br#"{"spec":{"replicas":5}}"#).unwrap();
        assert_eq!(replicas_of(&s).unwrap(), 5);
        assert_eq!(replicas_of(&json!({"spec": {"replicas": -1}})).unwrap_err().status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
        assert!(replicas_of(&json!({"spec": {}})).is_err());
    }
}
