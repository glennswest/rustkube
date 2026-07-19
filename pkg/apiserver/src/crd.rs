//! CRD (CustomResourceDefinition) support.
//!
//! Dynamic resource registration: when a CRD is created, its custom resources
//! become available as API endpoints.

use crate::error::ApiError;
use crate::handlers::AppState;
use crate::storage::ResourceStorage;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use std::collections::HashMap;
use tokio::sync::RwLock;

/// Scope of a CRD — determines whether instances are namespaced or cluster-scoped.
#[derive(Debug, Clone, PartialEq)]
pub enum CrdScope {
    Namespaced,
    Cluster,
}

/// A registered CRD definition.
#[derive(Debug, Clone)]
pub struct CrdDefinition {
    pub group: String,
    pub version: String,
    pub kind: String,
    pub plural: String,
    pub singular: String,
    pub short_names: Vec<String>,
    pub scope: CrdScope,
}

/// Registry of all active CRDs, keyed by group → version → plural.
#[derive(Default)]
pub struct CrdRegistry {
    #[allow(clippy::type_complexity)]
    crds: RwLock<HashMap<String, HashMap<String, HashMap<String, CrdDefinition>>>>,
}

impl CrdRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a CRD from its JSON spec.
    pub async fn register(&self, crd: &Value) {
        let group = crd["spec"]["group"].as_str().unwrap_or("").to_string();
        let names = &crd["spec"]["names"];
        let plural = names["plural"].as_str().unwrap_or("").to_string();
        let singular = names["singular"].as_str().unwrap_or("").to_string();
        let kind = names["kind"].as_str().unwrap_or("").to_string();
        let short_names: Vec<String> = names["shortNames"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let scope = match crd["spec"]["scope"].as_str().unwrap_or("Namespaced") {
            "Cluster" => CrdScope::Cluster,
            _ => CrdScope::Namespaced,
        };

        // Get version from the first entry in spec.versions
        let version = crd["spec"]["versions"]
            .as_array()
            .and_then(|vs| vs.first())
            .and_then(|v| v["name"].as_str())
            .unwrap_or("v1")
            .to_string();

        if plural.is_empty() || group.is_empty() {
            return;
        }

        let def = CrdDefinition {
            group: group.clone(),
            version: version.clone(),
            kind,
            plural: plural.clone(),
            singular,
            short_names,
            scope,
        };

        let mut crds = self.crds.write().await;
        crds.entry(group)
            .or_default()
            .entry(version)
            .or_default()
            .insert(plural, def);
    }

    /// Unregister a CRD by name (e.g. "foos.example.com").
    pub async fn unregister(&self, crd_name: &str) {
        // CRD name format is "{plural}.{group}"
        let parts: Vec<&str> = crd_name.splitn(2, '.').collect();
        if parts.len() != 2 {
            return;
        }
        let plural = parts[0];
        let group = parts[1];

        let mut crds = self.crds.write().await;
        if let Some(versions) = crds.get_mut(group) {
            for resources in versions.values_mut() {
                resources.remove(plural);
            }
            // Clean up empty maps
            versions.retain(|_, v| !v.is_empty());
            if versions.is_empty() {
                crds.remove(group);
            }
        }
    }

    /// Look up a CRD definition by group, version, and resource plural.
    pub async fn lookup(&self, group: &str, version: &str, resource: &str) -> Option<CrdDefinition> {
        let crds = self.crds.read().await;
        crds.get(group)?.get(version)?.get(resource).cloned()
    }

    /// Get all registered API groups and their resources for discovery.
    pub async fn api_groups(&self) -> Vec<Value> {
        let crds = self.crds.read().await;
        let mut groups = Vec::new();
        for (group, versions) in crds.iter() {
            let version_list: Vec<Value> = versions
                .keys()
                .map(|v| {
                    json!({
                        "groupVersion": format!("{group}/{v}"),
                        "version": v
                    })
                })
                .collect();
            if let Some(preferred) = version_list.first() {
                groups.push(json!({
                    "name": group,
                    "versions": version_list,
                    "preferredVersion": preferred
                }));
            }
        }
        groups
    }

    /// Get API resources for a specific group/version.
    pub async fn api_resources(&self, group: &str, version: &str) -> Vec<Value> {
        let crds = self.crds.read().await;
        let resources = match crds.get(group).and_then(|v| v.get(version)) {
            Some(r) => r,
            None => return Vec::new(),
        };
        resources
            .values()
            .map(|def| {
                let mut res = json!({
                    "name": def.plural,
                    "singularName": def.singular,
                    "namespaced": def.scope == CrdScope::Namespaced,
                    "kind": def.kind,
                    "verbs": ["create", "delete", "get", "list", "patch", "update", "watch"]
                });
                if !def.short_names.is_empty() {
                    res["shortNames"] = json!(def.short_names);
                }
                res
            })
            .collect()
    }
}

/// Load all existing CRDs from storage into the registry.
pub async fn load_existing_crds(storage: &ResourceStorage, registry: &CrdRegistry) {
    let prefix = ResourceStorage::cluster_prefix("customresourcedefinitions");
    if let Ok((crds, _, _)) = storage.list(&prefix, 1000, None).await {
        for crd in &crds {
            registry.register(crd).await;
        }
    }
}

// --- CRD API handlers ---

/// GET /apis/{group}/{version} — list CRD resources for this group/version.
pub async fn crd_api_resources(
    State(state): State<AppState>,
    Path((group, version)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    let resources = state.crd_registry.api_resources(&group, &version).await;
    Ok(Json(json!({
        "kind": "APIResourceList",
        "groupVersion": format!("{group}/{version}"),
        "resources": resources
    })))
}

/// LIST namespaced CRD instances.
pub async fn crd_list_ns(
    State(state): State<AppState>,
    Path((group, version, namespace, resource)): Path<(String, String, String, String)>,
    RawQuery(query): RawQuery,
) -> Result<Response, ApiError> {
    validate_crd(&state, &group, &version, &resource).await?;
    let params = crate::watch::WatchParams::from_query(query.as_deref().unwrap_or(""));
    let prefix = ResourceStorage::namespace_prefix(&resource, &namespace);

    if params.watch {
        let start_rev = params.resource_version.unwrap_or(0);
        let rx = state.storage.watch(&prefix, start_rev).await?;
        return Ok(crate::watch::watch_response(rx, params.label_selector, params.field_selector,
            format!("{group}/{version}"),
            crate::handlers::resource::resource_to_kind(&resource)));
    }

    let limit = params.limit.unwrap_or(500);
    let (items, continue_token, revision) = state
        .storage
        .list(&prefix, limit, params.continue_token.as_deref())
        .await?;
    let items = crate::selector::filter_objects(items, &params.label_selector, &params.field_selector);

    let mut list = json!({
        "apiVersion": format!("{group}/{version}"),
        "kind": format!("{}List", resource),
        "metadata": { "resourceVersion": revision.to_string() },
        "items": items
    });
    if let Some(token) = continue_token {
        list["metadata"]["continue"] = Value::String(token);
    }
    Ok(Json(list).into_response())
}

/// POST — create namespaced CRD instance.
pub async fn crd_create_ns(
    State(state): State<AppState>,
    Path((group, version, namespace, resource)): Path<(String, String, String, String)>,
    Json(mut body): Json<Value>,
) -> Result<impl IntoResponse, ApiError> {
    validate_crd(&state, &group, &version, &resource).await?;
    let name = body["metadata"]["name"]
        .as_str()
        .ok_or_else(|| ApiError::invalid("metadata.name is required"))?
        .to_string();
    crate::handlers::resource::ensure_metadata_pub(&mut body, &name, Some(&namespace));
    let key = ResourceStorage::namespaced_key(&resource, &namespace, &name);
    let obj = state.storage.create(&key, body).await?;
    Ok((StatusCode::CREATED, Json(obj)))
}

/// GET a single namespaced CRD instance.
pub async fn crd_get_ns(
    State(state): State<AppState>,
    Path((group, version, namespace, resource, name)): Path<(String, String, String, String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    validate_crd(&state, &group, &version, &resource).await?;
    let key = ResourceStorage::namespaced_key(&resource, &namespace, &name);
    let obj = state.storage.get(&key).await?;
    Ok(Json(obj))
}

/// PUT — update a namespaced CRD instance.
pub async fn crd_update_ns(
    State(state): State<AppState>,
    Path((group, version, namespace, resource, name)): Path<(String, String, String, String, String)>,
    Json(body): Json<Value>,
) -> Result<impl IntoResponse, ApiError> {
    validate_crd(&state, &group, &version, &resource).await?;
    let prev_rev = body["metadata"]["resourceVersion"]
        .as_str()
        .and_then(|rv| rv.parse::<u64>().ok());
    let key = ResourceStorage::namespaced_key(&resource, &namespace, &name);
    let obj = state.storage.update(&key, body, prev_rev).await?;
    Ok(Json(obj))
}

/// PATCH a namespaced CRD instance (merge-patch / JSON Patch / apply) — #23.
pub async fn crd_patch_ns(
    State(state): State<AppState>,
    Path((group, version, namespace, resource, name)): Path<(String, String, String, String, String)>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, ApiError> {
    validate_crd(&state, &group, &version, &resource).await?;
    let key = ResourceStorage::namespaced_key(&resource, &namespace, &name);
    let obj = patch_cr(&state, &key, &headers, &body).await?;
    Ok(Json(obj))
}

/// GET the /status subresource of a namespaced CR.
pub async fn crd_get_status_ns(
    State(state): State<AppState>,
    Path((group, version, namespace, resource, name)): Path<(String, String, String, String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    validate_crd(&state, &group, &version, &resource).await?;
    let key = ResourceStorage::namespaced_key(&resource, &namespace, &name);
    let obj = state.storage.get(&key).await?;
    Ok(Json(obj))
}

/// PUT the /status subresource of a namespaced CR (status only).
pub async fn crd_update_status_ns(
    State(state): State<AppState>,
    Path((group, version, namespace, resource, name)): Path<(String, String, String, String, String)>,
    Json(body): Json<Value>,
) -> Result<impl IntoResponse, ApiError> {
    validate_crd(&state, &group, &version, &resource).await?;
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

/// PATCH the /status subresource of a namespaced CR.
pub async fn crd_patch_status_ns(
    State(state): State<AppState>,
    Path((group, version, namespace, resource, name)): Path<(String, String, String, String, String)>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, ApiError> {
    validate_crd(&state, &group, &version, &resource).await?;
    let key = ResourceStorage::namespaced_key(&resource, &namespace, &name);
    let obj = patch_cr_status(&state, &key, &headers, &body).await?;
    Ok(Json(obj))
}

/// DELETE a namespaced CRD instance.
pub async fn crd_delete_ns(
    State(state): State<AppState>,
    Path((group, version, namespace, resource, name)): Path<(String, String, String, String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    validate_crd(&state, &group, &version, &resource).await?;
    let key = ResourceStorage::namespaced_key(&resource, &namespace, &name);
    state.storage.delete(&key, None).await?;
    Ok(Json(json!({
        "apiVersion": "v1",
        "kind": "Status",
        "metadata": {},
        "status": "Success",
        "details": { "name": name, "namespace": namespace, "kind": resource }
    })))
}

/// Read-modify-write a CR through the shared patch dispatcher.
async fn patch_cr(
    state: &AppState,
    key: &str,
    headers: &axum::http::HeaderMap,
    body: &[u8],
) -> Result<Value, ApiError> {
    let mut existing = state.storage.get(key).await?;
    let ct = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    crate::handlers::resource::apply_patch_body(&mut existing, ct, body)?;
    let prev_rev = existing["metadata"]["resourceVersion"]
        .as_str()
        .and_then(|rv| rv.parse::<u64>().ok());
    state.storage.update(key, existing, prev_rev).await
}

/// Patch only the `status` stanza of a CR, leaving spec/metadata untouched —
/// what `patch_status` in kube-rs/client-go expects from the subresource.
async fn patch_cr_status(
    state: &AppState,
    key: &str,
    headers: &axum::http::HeaderMap,
    body: &[u8],
) -> Result<Value, ApiError> {
    let mut existing = state.storage.get(key).await?;
    let ct = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    // Apply to a copy, then take only its status back — so a patch body that
    // touches other fields can't sneak spec changes through /status.
    let mut scratch = existing.clone();
    crate::handlers::resource::apply_patch_body(&mut scratch, ct, body)?;
    existing["status"] = scratch["status"].clone();
    let prev_rev = existing["metadata"]["resourceVersion"]
        .as_str()
        .and_then(|rv| rv.parse::<u64>().ok());
    state.storage.update(key, existing, prev_rev).await
}

/// PATCH a cluster-scoped CRD instance.
pub async fn crd_patch_cluster(
    State(state): State<AppState>,
    Path((group, version, resource, name)): Path<(String, String, String, String)>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, ApiError> {
    validate_crd(&state, &group, &version, &resource).await?;
    let key = ResourceStorage::cluster_key(&resource, &name);
    let obj = patch_cr(&state, &key, &headers, &body).await?;
    Ok(Json(obj))
}

/// GET the /status subresource of a cluster-scoped CR.
pub async fn crd_get_status_cluster(
    State(state): State<AppState>,
    Path((group, version, resource, name)): Path<(String, String, String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    validate_crd(&state, &group, &version, &resource).await?;
    let key = ResourceStorage::cluster_key(&resource, &name);
    let obj = state.storage.get(&key).await?;
    Ok(Json(obj))
}

/// PUT the /status subresource of a cluster-scoped CR.
pub async fn crd_update_status_cluster(
    State(state): State<AppState>,
    Path((group, version, resource, name)): Path<(String, String, String, String)>,
    Json(body): Json<Value>,
) -> Result<impl IntoResponse, ApiError> {
    validate_crd(&state, &group, &version, &resource).await?;
    let key = ResourceStorage::cluster_key(&resource, &name);
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

/// PATCH the /status subresource of a cluster-scoped CR.
pub async fn crd_patch_status_cluster(
    State(state): State<AppState>,
    Path((group, version, resource, name)): Path<(String, String, String, String)>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, ApiError> {
    validate_crd(&state, &group, &version, &resource).await?;
    let key = ResourceStorage::cluster_key(&resource, &name);
    let obj = patch_cr_status(&state, &key, &headers, &body).await?;
    Ok(Json(obj))
}

/// LIST cluster-scoped CRD instances.
pub async fn crd_list_cluster(
    State(state): State<AppState>,
    Path((group, version, resource)): Path<(String, String, String)>,
    RawQuery(query): RawQuery,
) -> Result<Response, ApiError> {
    validate_crd(&state, &group, &version, &resource).await?;
    let params = crate::watch::WatchParams::from_query(query.as_deref().unwrap_or(""));
    let prefix = ResourceStorage::cluster_prefix(&resource);

    if params.watch {
        let start_rev = params.resource_version.unwrap_or(0);
        let rx = state.storage.watch(&prefix, start_rev).await?;
        return Ok(crate::watch::watch_response(rx, params.label_selector, params.field_selector,
            format!("{group}/{version}"),
            crate::handlers::resource::resource_to_kind(&resource)));
    }

    let limit = params.limit.unwrap_or(500);
    let (items, continue_token, revision) = state
        .storage
        .list(&prefix, limit, params.continue_token.as_deref())
        .await?;
    let items = crate::selector::filter_objects(items, &params.label_selector, &params.field_selector);

    let mut list = json!({
        "apiVersion": format!("{group}/{version}"),
        "kind": format!("{}List", resource),
        "metadata": { "resourceVersion": revision.to_string() },
        "items": items
    });
    if let Some(token) = continue_token {
        list["metadata"]["continue"] = Value::String(token);
    }
    Ok(Json(list).into_response())
}

/// POST — create cluster-scoped CRD instance.
pub async fn crd_create_cluster(
    State(state): State<AppState>,
    Path((group, version, resource)): Path<(String, String, String)>,
    Json(mut body): Json<Value>,
) -> Result<impl IntoResponse, ApiError> {
    validate_crd(&state, &group, &version, &resource).await?;
    let name = body["metadata"]["name"]
        .as_str()
        .ok_or_else(|| ApiError::invalid("metadata.name is required"))?
        .to_string();
    crate::handlers::resource::ensure_metadata_pub(&mut body, &name, None);
    let key = ResourceStorage::cluster_key(&resource, &name);

    // Special handling: if this is a CRD being created, register it
    if resource == "customresourcedefinitions" {
        let obj = state.storage.create(&key, body).await?;
        state.crd_registry.register(&obj).await;
        return Ok((StatusCode::CREATED, Json(obj)));
    }

    let obj = state.storage.create(&key, body).await?;
    Ok((StatusCode::CREATED, Json(obj)))
}

/// GET a single cluster-scoped CRD instance.
pub async fn crd_get_cluster(
    State(state): State<AppState>,
    Path((group, version, resource, name)): Path<(String, String, String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    validate_crd(&state, &group, &version, &resource).await?;
    let key = ResourceStorage::cluster_key(&resource, &name);
    let obj = state.storage.get(&key).await?;
    Ok(Json(obj))
}

/// PUT — update a cluster-scoped CRD instance.
pub async fn crd_update_cluster(
    State(state): State<AppState>,
    Path((group, version, resource, name)): Path<(String, String, String, String)>,
    Json(body): Json<Value>,
) -> Result<impl IntoResponse, ApiError> {
    validate_crd(&state, &group, &version, &resource).await?;
    let prev_rev = body["metadata"]["resourceVersion"]
        .as_str()
        .and_then(|rv| rv.parse::<u64>().ok());
    let key = ResourceStorage::cluster_key(&resource, &name);
    let obj = state.storage.update(&key, body, prev_rev).await?;
    Ok(Json(obj))
}

/// DELETE a cluster-scoped CRD instance.
pub async fn crd_delete_cluster(
    State(state): State<AppState>,
    Path((group, version, resource, name)): Path<(String, String, String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    validate_crd(&state, &group, &version, &resource).await?;
    let key = ResourceStorage::cluster_key(&resource, &name);
    state.storage.delete(&key, None).await?;

    // If this is a CRD being deleted, unregister it
    if resource == "customresourcedefinitions" {
        state.crd_registry.unregister(&name).await;
    }

    Ok(Json(json!({
        "apiVersion": "v1",
        "kind": "Status",
        "metadata": {},
        "status": "Success",
        "details": { "name": name, "kind": resource }
    })))
}

/// Validate that the resource exists in the CRD registry or is a built-in CRD resource.
async fn validate_crd(
    state: &AppState,
    group: &str,
    version: &str,
    resource: &str,
) -> Result<(), ApiError> {
    // apiextensions.k8s.io/v1/customresourcedefinitions is always valid
    if group == "apiextensions.k8s.io" && resource == "customresourcedefinitions" {
        return Ok(());
    }
    // Check the CRD registry
    if state.crd_registry.lookup(group, version, resource).await.is_some() {
        return Ok(());
    }
    Err(ApiError::not_found("resource", resource))
}
