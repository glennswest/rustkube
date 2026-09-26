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
#[derive(Debug, Clone, Copy, PartialEq)]
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
    /// `additionalPrinterColumns` for this version, verbatim from the CRD.
    ///
    /// What makes `oc get ciliumendpoints` print something other than NAME and
    /// AGE. The columns are the CRD author's, so honouring them covers every
    /// custom resource at once rather than one hand-written printer per kind.
    pub printer_columns: Vec<Value>,
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

        if plural.is_empty() || group.is_empty() {
            return;
        }
        // Never serve a group without a dot: its CRs would be stored under
        // `/registry/{group}/`, which is where a built-in named like the group
        // lives (#76). The write paths refuse such a CRD with a reason; this
        // catches one that is already stored or arrives from a manifest.
        if let Err(e) = validate_crd_group(&group) {
            tracing::warn!("CRD {plural}.{group} not served: {}", e.message);
            return;
        }

        // **Every served version, not just the first.** A CRD declares a list
        // of versions and clients pick one; registering only `versions[0]`
        // made every other version 404 — which for Cilium means its v2alpha1
        // kinds (CiliumEndpointSlice among them) do not exist as far as the
        // apiserver is concerned, while its v2 kinds do.
        //
        // `served: false` is a version that exists in storage and is not
        // offered over the API, so it is skipped rather than registered.
        let mut versions: Vec<(String, Vec<Value>)> = crd["spec"]["versions"]
            .as_array()
            .map(|vs| {
                vs.iter()
                    .filter(|v| v["served"].as_bool().unwrap_or(true))
                    .filter_map(|v| {
                        let name = v["name"].as_str()?.to_string();
                        let cols = v["additionalPrinterColumns"]
                            .as_array()
                            .cloned()
                            .unwrap_or_default();
                        Some((name, cols))
                    })
                    .collect()
            })
            .unwrap_or_default();
        if versions.is_empty() {
            versions.push(("v1".to_string(), Vec::new()));
        }

        let mut crds = self.crds.write().await;
        for (version, printer_columns) in versions {
            let def = CrdDefinition {
                group: group.clone(),
                version: version.clone(),
                kind: kind.clone(),
                plural: plural.clone(),
                singular: singular.clone(),
                short_names: short_names.clone(),
                scope,
                printer_columns,
            };
            crds.entry(group.clone())
                .or_default()
                .entry(version)
                .or_default()
                .insert(plural.clone(), def);
        }
    }

    /// `(plural, namespaced)` for a custom kind, so a manifest naming a
    /// custom resource can be resolved to a storage path the same way a
    /// built-in one is.
    pub async fn resource_for_kind(
        &self,
        group: &str,
        version: &str,
        kind: &str,
    ) -> Option<(String, bool)> {
        let crds = self.crds.read().await;
        crds.get(group)?.get(version)?.values().find(|d| d.kind == kind).map(|d| {
            (d.plural.clone(), matches!(d.scope, CrdScope::Namespaced))
        })
    }

    /// The printer columns a version declares, if any.
    pub async fn printer_columns(&self, group: &str, version: &str, plural: &str) -> Vec<Value> {
        let crds = self.crds.read().await;
        crds.get(group)
            .and_then(|vs| vs.get(version))
            .and_then(|ps| ps.get(plural))
            .map(|d| d.printer_columns.clone())
            .unwrap_or_default()
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

    /// One definition per `(group, plural)`, whichever version it came from.
    pub async fn definitions(&self) -> Vec<CrdDefinition> {
        let crds = self.crds.read().await;
        let mut seen = std::collections::HashSet::new();
        crds.values()
            .flat_map(|versions| versions.values())
            .flat_map(|resources| resources.values())
            .filter(|d| seen.insert((d.group.clone(), d.plural.clone())))
            .cloned()
            .collect()
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

/// Load all existing CRDs from storage into the registry, and backfill the
/// establishing status on any that predate #36.
pub async fn load_existing_crds(storage: &ResourceStorage, registry: &CrdRegistry) {
    let prefix = ResourceStorage::cluster_prefix("customresourcedefinitions");
    if let Ok((crds, _, _)) = storage.list(&prefix, 1000, None).await {
        for crd in &crds {
            registry.register(crd).await;
            // Establish any CRD whose status was never populated.
            if crd["status"]["conditions"].as_array().is_none() {
                let mut updated = crd.clone();
                establish_crd_status(&mut updated);
                let name = crd["metadata"]["name"].as_str().unwrap_or("");
                let key = ResourceStorage::cluster_key("customresourcedefinitions", name);
                let _ = storage.update(&key, updated, None).await;
            }
        }
    }
    migrate_legacy_cr_keys(storage, registry).await;
}

/// Move custom resources from the pre-#76 key `/registry/{plural}/...` to
/// `/registry/{group}/{plural}/...`.
///
/// The old keyspace was shared by every CRD with the same plural — and by a
/// built-in with it — so what moves is decided per object, by the group in its
/// own `apiVersion`, never by the prefix it was found under. Two CRDs that
/// collided therefore separate correctly, and a built-in `events` is never
/// swept into `events.example.io`.
///
/// Runs at every boot and is idempotent, which also makes it safe in HA: the
/// object is created at the new key before the old one is deleted, and a new
/// key already holding the *same* object (same `uid`) — another replica got
/// there first, or a previous boot died between the two steps — just has its
/// stale copy removed. A different object at the new key is a conflict that is
/// reported and left alone, because either choice would lose data. A replica
/// still running the old code writes old keys during a rolling upgrade; the
/// next boot of an upgraded replica moves them.
pub async fn migrate_legacy_cr_keys(storage: &ResourceStorage, registry: &CrdRegistry) {
    for def in registry.definitions().await {
        // Collect first: moving while paging through the same prefix would
        // shift the pages under the continue token.
        let old_prefix = ResourceStorage::cluster_prefix(&def.plural);
        let mut items = Vec::new();
        let mut cont: Option<String> = None;
        loop {
            match storage.list(&old_prefix, 500, cont.as_deref()).await {
                Ok((page, next, _)) => {
                    items.extend(page);
                    match next {
                        Some(t) => cont = Some(t),
                        None => break,
                    }
                }
                Err(e) => {
                    tracing::warn!("CR key migration: listing {old_prefix} failed: {}", e.message);
                    break;
                }
            }
        }

        let new_resource = ResourceStorage::custom_resource(&def.group, &def.plural);
        let mut moved = 0usize;
        for obj in items {
            let (group, _) =
                crate::manifests::group_version(obj["apiVersion"].as_str().unwrap_or(""));
            if group != def.group {
                continue;
            }
            let Some(name) = obj["metadata"]["name"].as_str() else { continue };
            let (old_key, new_key) = match (def.scope, obj["metadata"]["namespace"].as_str()) {
                (CrdScope::Namespaced, Some(ns)) => (
                    ResourceStorage::namespaced_key(&def.plural, ns, name),
                    ResourceStorage::namespaced_key(&new_resource, ns, name),
                ),
                (CrdScope::Cluster, _) => (
                    ResourceStorage::cluster_key(&def.plural, name),
                    ResourceStorage::cluster_key(&new_resource, name),
                ),
                (CrdScope::Namespaced, None) => {
                    tracing::warn!(
                        "CR key migration: {}.{} {name} has no namespace; left in place",
                        def.plural, def.group
                    );
                    continue;
                }
            };
            let placed = match storage.create(&new_key, obj.clone()).await {
                Ok(_) => true,
                Err(e) if e.is_already_exists() => {
                    match storage.get(&new_key).await {
                        Ok(there) if there["metadata"]["uid"] == obj["metadata"]["uid"] => true,
                        _ => {
                            tracing::warn!(
                                "CR key migration: {new_key} already holds a different object; \
                                 {old_key} left in place"
                            );
                            false
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("CR key migration: writing {new_key} failed: {}", e.message);
                    false
                }
            };
            if placed {
                match storage.delete(&old_key, None).await {
                    Ok(()) => moved += 1,
                    Err(e) if e.status == StatusCode::NOT_FOUND => moved += 1,
                    Err(e) => tracing::warn!(
                        "CR key migration: removing {old_key} failed: {}",
                        e.message
                    ),
                }
            }
        }
        if moved > 0 {
            tracing::info!(
                "CR key migration: moved {moved} {}.{} to {}",
                def.plural,
                def.group,
                ResourceStorage::cluster_prefix(&new_resource)
            );
        }
    }
}

/// Populate a CustomResourceDefinition's `status` the way the upstream
/// apiextensions naming/establishing controller does (#36): copy the accepted
/// names from spec, mark NamesAccepted + Established, and record the stored
/// version. Clients (cilium-operator, kube-rs) block on Established=True.
pub fn establish_crd_status(crd: &mut Value) {
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let names = crd["spec"]["names"].clone();
    let kind = names["kind"].as_str().unwrap_or("").to_string();

    // acceptedNames mirror spec.names, filling list/singular defaults like the
    // controller does.
    let mut accepted = names.clone();
    if accepted["listKind"].as_str().unwrap_or("").is_empty() && !kind.is_empty() {
        accepted["listKind"] = json!(format!("{kind}List"));
    }
    if accepted["singular"].as_str().unwrap_or("").is_empty() && !kind.is_empty() {
        accepted["singular"] = json!(kind.to_lowercase());
    }

    // storedVersions = the storage version (else all served version names).
    let stored: Vec<Value> = crd["spec"]["versions"]
        .as_array()
        .map(|vs| {
            let storage: Vec<Value> = vs
                .iter()
                .filter(|v| v["storage"].as_bool() == Some(true))
                .filter_map(|v| v["name"].as_str().map(|s| json!(s)))
                .collect();
            if storage.is_empty() {
                vs.iter().filter_map(|v| v["name"].as_str().map(|s| json!(s))).collect()
            } else {
                storage
            }
        })
        .unwrap_or_default();

    crd["status"] = json!({
        "acceptedNames": accepted,
        "storedVersions": stored,
        "conditions": [
            {
                "type": "NamesAccepted",
                "status": "True",
                "reason": "NoConflicts",
                "message": "no conflicts found",
                "lastTransitionTime": now,
            },
            {
                "type": "Established",
                "status": "True",
                "reason": "InitialNamesAccepted",
                "message": "the initial names have been accepted",
                "lastTransitionTime": now,
            }
        ]
    });
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
    headers: axum::http::HeaderMap,
    RawQuery(query): RawQuery,
) -> Result<Response, ApiError> {
    validate_crd(&state, &group, &version, &resource).await?;
    let params = crate::watch::WatchParams::from_query(query.as_deref().unwrap_or(""));
    let metadata_only = crate::handlers::resource::accept_partial_metadata(&headers);
    let (item_kind, list_kind) = crd_kinds(&state, &group, &version, &resource).await;
    let prefix = ResourceStorage::namespace_prefix(&storage_resource(&group, &resource), &namespace);

    if params.watch {
        return crate::handlers::resource::watch_prefix(
            &state.storage,
            &prefix,
            &params,
            format!("{group}/{version}"),
            item_kind,
            metadata_only,
        )
        .await;
    }

    let limit = params.limit.unwrap_or(500);
    let (items, continue_token, revision) = state
        .storage
        .list(&prefix, limit, params.continue_token.as_deref())
        .await?;
    let items = crate::selector::filter_objects(items, &params.label_selector, &params.field_selector);

    let mut list = json!({
        "apiVersion": format!("{group}/{version}"),
        "kind": list_kind,
        "metadata": { "resourceVersion": revision.to_string() },
        "items": items
    });
    if let Some(token) = continue_token {
        list["metadata"]["continue"] = Value::String(token);
    }
    let body = crate::handlers::resource::project_list(list, metadata_only);
    // A custom resource gets a Table too, built from the columns its own CRD
    // declares. Without this every `oc get <anything custom>` printed NAME and
    // AGE and nothing else, however much the CRD had to say.
    if crate::table::wants_table(&headers) {
        let cols = state.crd_registry.printer_columns(&group, &version, &resource).await;
        return Ok(Json(crate::table::to_table_crd(body, &cols)).into_response());
    }
    Ok(Json(body).into_response())
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
    let key = ResourceStorage::namespaced_key(&storage_resource(&group, &resource), &namespace, &name);
    let obj = state.storage.create(&key, body).await?;
    Ok((StatusCode::CREATED, Json(obj)))
}

/// GET a single namespaced CRD instance.
pub async fn crd_get_ns(
    State(state): State<AppState>,
    Path((group, version, namespace, resource, name)): Path<(String, String, String, String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    validate_crd(&state, &group, &version, &resource).await?;
    let key = ResourceStorage::namespaced_key(&storage_resource(&group, &resource), &namespace, &name);
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
    let key = ResourceStorage::namespaced_key(&storage_resource(&group, &resource), &namespace, &name);
    let obj = crate::handlers::resource::put_object(&state, &key, &name, Some(&namespace), body).await?;
    let obj = register_if_crd(&state, &resource, &obj).await;
    Ok(Json(obj))
}

/// PATCH a namespaced CRD instance (merge-patch / JSON Patch / apply) — #23.
pub async fn crd_patch_ns(
    State(state): State<AppState>,
    Path((group, version, namespace, resource, name)): Path<(String, String, String, String, String)>,
    headers: axum::http::HeaderMap,
    RawQuery(query): RawQuery,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, ApiError> {
    validate_crd(&state, &group, &version, &resource).await?;
    let key = ResourceStorage::namespaced_key(&storage_resource(&group, &resource), &namespace, &name);
    // Shared path so server-side apply upserts a missing CR + tracks managedFields (#45).
    let obj = crate::handlers::resource::patch_stored_object(
        &state, &key, &resource, &name, Some(&namespace), &headers,
        query.as_deref().unwrap_or(""), &body,
    )
    .await?;
    let obj = register_if_crd(&state, &resource, &obj).await;
    Ok(Json(obj))
}

/// GET the /status subresource of a namespaced CR.
pub async fn crd_get_status_ns(
    State(state): State<AppState>,
    Path((group, version, namespace, resource, name)): Path<(String, String, String, String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    validate_crd(&state, &group, &version, &resource).await?;
    let key = ResourceStorage::namespaced_key(&storage_resource(&group, &resource), &namespace, &name);
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
    let key = ResourceStorage::namespaced_key(&storage_resource(&group, &resource), &namespace, &name);
    // Conditional on the body's resourceVersion, as every status PUT is (#78).
    let obj = crate::handlers::resource::put_status(&state, &key, &body).await?;
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
    let key = ResourceStorage::namespaced_key(&storage_resource(&group, &resource), &namespace, &name);
    let obj = patch_cr_status(&state, &key, &headers, &body).await?;
    Ok(Json(obj))
}

/// DELETE a namespaced CRD instance.
pub async fn crd_delete_ns(
    State(state): State<AppState>,
    Path((group, version, namespace, resource, name)): Path<(String, String, String, String, String)>,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, ApiError> {
    validate_crd(&state, &group, &version, &resource).await?;
    let key = ResourceStorage::namespaced_key(&storage_resource(&group, &resource), &namespace, &name);
    let obj = state.storage.get(&key).await?;
    let opts = crate::handlers::resource::parse_delete_options(&body);
    let (item_kind, _) = crd_kinds(&state, &group, &version, &resource).await;
    let out = crate::handlers::resource::perform_delete(
        &state, &key, obj, &opts, &name, Some(&namespace), &item_kind,
    )
    .await?;
    Ok(Json(out))
}

/// Patch only the `status` stanza of a CR, leaving spec/metadata untouched —
/// what `patch_status` in kube-rs/client-go expects from the subresource.
async fn patch_cr_status(
    state: &AppState,
    key: &str,
    headers: &axum::http::HeaderMap,
    body: &[u8],
) -> Result<Value, ApiError> {
    let ct = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    // Re-read and re-applied on a lost CAS, like every PATCH (#77).
    crate::handlers::resource::guaranteed_patch(state, key, ct, body, |mut existing| {
        // Apply to a copy, then take only its status back — so a patch body
        // that touches other fields can't sneak spec changes through /status.
        let mut scratch = existing.clone();
        crate::handlers::resource::apply_patch_body(&mut scratch, ct, body)?;
        existing["status"] = scratch["status"].clone();
        Ok(existing)
    })
    .await
}

/// PATCH a cluster-scoped CRD instance.
pub async fn crd_patch_cluster(
    State(state): State<AppState>,
    Path((group, version, resource, name)): Path<(String, String, String, String)>,
    headers: axum::http::HeaderMap,
    RawQuery(query): RawQuery,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, ApiError> {
    validate_crd(&state, &group, &version, &resource).await?;
    check_crd_write(&resource, Some(&name), None)?;
    let key = ResourceStorage::cluster_key(&storage_resource(&group, &resource), &name);
    // Shared path so server-side apply upserts a missing CR + tracks managedFields (#45).
    let obj = crate::handlers::resource::patch_stored_object(
        &state, &key, &resource, &name, None, &headers, query.as_deref().unwrap_or(""), &body,
    )
    .await?;
    let obj = register_if_crd(&state, &resource, &obj).await;
    Ok(Json(obj))
}

/// GET the /status subresource of a cluster-scoped CR.
pub async fn crd_get_status_cluster(
    State(state): State<AppState>,
    Path((group, version, resource, name)): Path<(String, String, String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    validate_crd(&state, &group, &version, &resource).await?;
    let key = ResourceStorage::cluster_key(&storage_resource(&group, &resource), &name);
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
    let key = ResourceStorage::cluster_key(&storage_resource(&group, &resource), &name);
    // Conditional on the body's resourceVersion, as every status PUT is (#78).
    let obj = crate::handlers::resource::put_status(&state, &key, &body).await?;
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
    let key = ResourceStorage::cluster_key(&storage_resource(&group, &resource), &name);
    let obj = patch_cr_status(&state, &key, &headers, &body).await?;
    Ok(Json(obj))
}

/// LIST cluster-scoped CRD instances.
pub async fn crd_list_cluster(
    State(state): State<AppState>,
    Path((group, version, resource)): Path<(String, String, String)>,
    headers: axum::http::HeaderMap,
    RawQuery(query): RawQuery,
) -> Result<Response, ApiError> {
    validate_crd(&state, &group, &version, &resource).await?;
    let params = crate::watch::WatchParams::from_query(query.as_deref().unwrap_or(""));
    let metadata_only = crate::handlers::resource::accept_partial_metadata(&headers);
    let (item_kind, list_kind) = crd_kinds(&state, &group, &version, &resource).await;
    let prefix = ResourceStorage::cluster_prefix(&storage_resource(&group, &resource));

    if params.watch {
        return crate::handlers::resource::watch_prefix(
            &state.storage,
            &prefix,
            &params,
            format!("{group}/{version}"),
            item_kind,
            metadata_only,
        )
        .await;
    }

    let limit = params.limit.unwrap_or(500);
    let (items, continue_token, revision) = state
        .storage
        .list(&prefix, limit, params.continue_token.as_deref())
        .await?;
    let items = crate::selector::filter_objects(items, &params.label_selector, &params.field_selector);

    let mut list = json!({
        "apiVersion": format!("{group}/{version}"),
        "kind": list_kind,
        "metadata": { "resourceVersion": revision.to_string() },
        "items": items
    });
    if let Some(token) = continue_token {
        list["metadata"]["continue"] = Value::String(token);
    }
    let body = crate::handlers::resource::project_list(list, metadata_only);
    // A custom resource gets a Table too, built from the columns its own CRD
    // declares. Without this every `oc get <anything custom>` printed NAME and
    // AGE and nothing else, however much the CRD had to say.
    if crate::table::wants_table(&headers) {
        let cols = state.crd_registry.printer_columns(&group, &version, &resource).await;
        return Ok(Json(crate::table::to_table_crd(body, &cols)).into_response());
    }
    Ok(Json(body).into_response())
}

/// POST — create cluster-scoped CRD instance.
/// Keep the dynamic API in step with a CRD object that has just been written.
///
/// POST did this inline and every other write path did not, so a CRD created
/// by server-side apply — which is how an operator installs its own at
/// startup — was **stored but never registered**: `kubectl get crds` listed
/// it and every request for its CRs returned
///
/// ```text
/// resource "cloudimages" not found: NotFound … code: 404
/// ```
///
/// forever, or until the apiserver restarted, because `load_existing_crds`
/// re-reads storage at boot. That last part is what made it read as a
/// cold-start race rather than a missing call, and it cost a VM create that
/// failed with no explanation anyone could act on (#74).
///
/// It matters on update as much as on create: a CRD whose spec is changed by
/// apply — a new version served, different printer columns — would otherwise
/// leave the registry holding whatever it was given at create time.
///
/// Registration is idempotent, so calling it on every write is correct rather
/// than merely harmless.
async fn register_if_crd(state: &AppState, resource: &str, obj: &Value) -> Value {
    if resource != "customresourcedefinitions" {
        return obj.clone();
    }
    // Establish it if nothing has, then register.
    //
    // POST establishes before storing. Apply upserts a *missing* CRD through
    // the shared patch path, which knows nothing about CRDs, so one installed
    // that way arrived with no status at all — and a client that waits for
    // `Established=True` before using its own resource waits for ever.
    let mut out = obj.clone();
    let established = out["status"]["conditions"]
        .as_array()
        .map(|cs| {
            cs.iter().any(|c| {
                c["type"].as_str() == Some("Established") && c["status"].as_str() == Some("True")
            })
        })
        .unwrap_or(false);
    if !established {
        establish_crd_status(&mut out);
        let name = out["metadata"]["name"].as_str().unwrap_or("").to_string();
        if !name.is_empty() {
            let key = ResourceStorage::cluster_key(resource, &name);
            // Best effort: the registration below is what makes the CRs
            // reachable, and failing to persist a status condition must not
            // undo that. A status written on the next apply is a smaller
            // problem than a resource that 404s.
            if let Ok(stored) = state.storage.update(&key, out.clone(), None).await {
                out = stored;
            }
        }
    }
    state.crd_registry.register(&out).await;
    out
}

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
    let key = ResourceStorage::cluster_key(&storage_resource(&group, &resource), &name);

    // Special handling: if this is a CRD being created, establish it (populate
    // status so clients that wait for Established=True proceed — #36) and
    // register it for the dynamic API.
    if resource == "customresourcedefinitions" {
        check_crd_write(&resource, Some(&name), Some(&body))?;
        establish_crd_status(&mut body);
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
    let key = ResourceStorage::cluster_key(&storage_resource(&group, &resource), &name);
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
    check_crd_write(&resource, Some(&name), Some(&body))?;
    let key = ResourceStorage::cluster_key(&storage_resource(&group, &resource), &name);
    let obj = crate::handlers::resource::put_object(&state, &key, &name, None, body).await?;
    let obj = register_if_crd(&state, &resource, &obj).await;
    Ok(Json(obj))
}

/// DELETE a cluster-scoped CRD instance.
pub async fn crd_delete_cluster(
    State(state): State<AppState>,
    Path((group, version, resource, name)): Path<(String, String, String, String)>,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, ApiError> {
    validate_crd(&state, &group, &version, &resource).await?;
    let key = ResourceStorage::cluster_key(&storage_resource(&group, &resource), &name);
    let obj = state.storage.get(&key).await?;
    let opts = crate::handlers::resource::parse_delete_options(&body);
    let (item_kind, _) = crd_kinds(&state, &group, &version, &resource).await;
    let out = crate::handlers::resource::perform_delete(
        &state, &key, obj, &opts, &name, None, &item_kind,
    )
    .await?;

    // If a CRD was actually removed (not left terminating), unregister it.
    if resource == "customresourcedefinitions" && out["kind"] == "Status" {
        state.crd_registry.unregister(&name).await;
    }

    Ok(Json(out))
}

/// Validate that the resource exists in the CRD registry or is a built-in CRD resource.
/// The `(kind, listKind)` for a CRD resource — the CRD's declared
/// `spec.names.kind` (e.g. `CiliumNetworkPolicy`), NOT the plural. Using the
/// plural (`ciliumnetworkpoliciesList`) makes client-go reject the response with
/// "no kind registered", so a CR informer (the Cilium agent's) never syncs.
async fn crd_kinds(state: &AppState, group: &str, version: &str, resource: &str) -> (String, String) {
    if let Some(def) = state.crd_registry.lookup(group, version, resource).await {
        return (def.kind.clone(), format!("{}List", def.kind));
    }
    // apiextensions customresourcedefinitions and any unlisted fallback.
    let kind = crate::handlers::resource::resource_to_kind(resource);
    let list = format!("{kind}List");
    (kind, list)
}

/// Where the handlers in this module store `resource` of `group`.
///
/// Custom resources live under their group (#76). The CRDs themselves are
/// served by the same handlers and are a built-in, so they keep
/// `/registry/customresourcedefinitions/` — the key every existing cluster
/// already has them under.
fn storage_resource(group: &str, resource: &str) -> String {
    if group == "apiextensions.k8s.io" && resource == "customresourcedefinitions" {
        resource.to_string()
    } else {
        ResourceStorage::custom_resource(group, resource)
    }
}

/// A CRD's group must contain a dot, as upstream requires.
///
/// Upstream asks for it because a group is a DNS subdomain the author owns.
/// Here it also keeps the keyspace sound: CRs are stored under
/// `/registry/{group}/` and built-ins under `/registry/{plural}/`, and the dot
/// is what makes those two never meet.
fn validate_crd_group(group: &str) -> Result<(), ApiError> {
    if group.contains('.') {
        return Ok(());
    }
    Err(ApiError::invalid(&format!(
        "spec.group: Invalid value: \"{group}\": should be a domain with at least one dot"
    )))
}

/// Refuse a CRD write whose group has no dot, before anything is stored.
///
/// PATCH has no finished object to look at before it is stored, so it is
/// judged by name: a CRD is named `{plural}.{group}`, and that name is in the
/// path.
fn check_crd_write(resource: &str, name: Option<&str>, body: Option<&Value>) -> Result<(), ApiError> {
    if resource != "customresourcedefinitions" {
        return Ok(());
    }
    if let Some(group) = body.and_then(|b| b["spec"]["group"].as_str()) {
        return validate_crd_group(group);
    }
    match name.and_then(|n| n.split_once('.')) {
        Some((_, group)) => validate_crd_group(group),
        None => validate_crd_group(name.unwrap_or("")),
    }
}

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

#[cfg(test)]
mod establish_tests {
    use super::*;

    #[test]
    fn establishes_names_conditions_and_stored_versions() {
        let mut crd = json!({
            "apiVersion": "apiextensions.k8s.io/v1", "kind": "CustomResourceDefinition",
            "metadata": {"name": "widgets.demo.io"},
            "spec": {
                "group": "demo.io", "scope": "Namespaced",
                "names": {"plural": "widgets", "kind": "Widget"},
                "versions": [
                    {"name": "v1", "served": true, "storage": false},
                    {"name": "v2", "served": true, "storage": true}
                ]
            }
        });
        establish_crd_status(&mut crd);
        let s = &crd["status"];
        // acceptedNames copied + defaults filled.
        assert_eq!(s["acceptedNames"]["kind"], "Widget");
        assert_eq!(s["acceptedNames"]["plural"], "widgets");
        assert_eq!(s["acceptedNames"]["listKind"], "WidgetList");
        assert_eq!(s["acceptedNames"]["singular"], "widget");
        // stored version = the storage:true one.
        assert_eq!(s["storedVersions"], json!(["v2"]));
        // both conditions True.
        let conds = s["conditions"].as_array().unwrap();
        assert!(conds.iter().any(|c| c["type"] == "NamesAccepted" && c["status"] == "True"));
        assert!(conds.iter().any(|c| c["type"] == "Established" && c["status"] == "True"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every served version registers, not just the first. Cilium ships v2 and
    /// v2alpha1 kinds side by side; taking versions[0] made the rest 404.
    #[tokio::test]
    async fn all_served_versions_register() {
        let reg = CrdRegistry::new();
        reg.register(&serde_json::json!({
            "spec": {
                "group": "cilium.io",
                "scope": "Namespaced",
                "names": {"plural": "ciliumendpoints", "kind": "CiliumEndpoint"},
                "versions": [
                    {"name": "v2", "served": true,
                     "additionalPrinterColumns": [
                        {"name": "Endpoint", "type": "integer", "jsonPath": ".status.id"}
                     ]},
                    {"name": "v2alpha1", "served": true},
                    {"name": "v1", "served": false},
                ]
            }
        }))
        .await;

        assert!(!reg.api_resources("cilium.io", "v2").await.is_empty(), "v2 missing");
        assert!(
            !reg.api_resources("cilium.io", "v2alpha1").await.is_empty(),
            "v2alpha1 missing — only the first version registered"
        );
        // served: false is storage-only and is not offered over the API.
        assert!(reg.api_resources("cilium.io", "v1").await.is_empty(), "v1 is not served");

        // Columns travel with the version that declared them.
        let cols = reg.printer_columns("cilium.io", "v2", "ciliumendpoints").await;
        assert_eq!(cols.len(), 1);
        assert_eq!(cols[0]["name"], "Endpoint");
        assert!(reg
            .printer_columns("cilium.io", "v2alpha1", "ciliumendpoints")
            .await
            .is_empty());
    }

    /// CRs are stored under their group; the CRDs themselves keep their
    /// built-in key (#76).
    #[test]
    fn storage_resource_qualifies_custom_resources_only() {
        assert_eq!(storage_resource("storm.io", "cloudimages"), "storm.io/cloudimages");
        assert_eq!(
            storage_resource("apiextensions.k8s.io", "customresourcedefinitions"),
            "customresourcedefinitions"
        );
    }

    /// A group with no dot would put CRs under `/registry/{group}/`, where a
    /// built-in of that name lives, so it is refused on every path in.
    #[tokio::test]
    async fn a_group_without_a_dot_is_refused() {
        let crd = |group: &str| {
            serde_json::json!({"spec": {"group": group, "scope": "Cluster",
                "names": {"plural": "widgets", "kind": "Widget"},
                "versions": [{"name": "v1", "served": true}]}})
        };
        assert!(check_crd_write("customresourcedefinitions", None, Some(&crd("pods"))).is_err());
        assert!(check_crd_write("customresourcedefinitions", None, Some(&crd("demo.io"))).is_ok());
        // PATCH is judged by the name, `{plural}.{group}`.
        assert!(check_crd_write("customresourcedefinitions", Some("widgets.pods"), None).is_err());
        assert!(check_crd_write("customresourcedefinitions", Some("widgets"), None).is_err());
        assert!(check_crd_write("customresourcedefinitions", Some("widgets.demo.io"), None).is_ok());
        // Only CRD writes are checked.
        assert!(check_crd_write("widgets", Some("anything"), None).is_ok());

        let reg = CrdRegistry::new();
        reg.register(&crd("pods")).await;
        assert!(reg.lookup("pods", "v1", "widgets").await.is_none());
    }

    /// Two CRDs sharing a plural are two definitions, and a CRD serving two
    /// versions is one — which is what the key migration walks.
    #[tokio::test]
    async fn definitions_are_one_per_group_and_plural() {
        let reg = CrdRegistry::new();
        for group in ["metal3.io", "metal.storm.io"] {
            reg.register(&serde_json::json!({"spec": {"group": group, "scope": "Namespaced",
                "names": {"plural": "baremetalhosts", "kind": "BareMetalHost"},
                "versions": [{"name": "v1alpha1", "served": true}, {"name": "v1", "served": true}]}}))
                .await;
        }
        let mut got: Vec<_> = reg
            .definitions()
            .await
            .into_iter()
            .map(|d| format!("{}.{}", d.plural, d.group))
            .collect();
        got.sort();
        assert_eq!(got, ["baremetalhosts.metal.storm.io", "baremetalhosts.metal3.io"]);
    }
}
