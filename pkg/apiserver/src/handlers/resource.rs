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

/// Build a watch `Response` for `prefix`, honoring `sendInitialEvents`
/// (WatchList: replay current state as ADDED, then an `initial-events-end`
/// BOOKMARK) and `allowWatchBookmarks` (periodic heartbeat bookmarks) from
/// `params`. Shared by every LIST/WATCH handler (core + CRD).
pub(crate) async fn watch_prefix(
    storage: &ResourceStorage,
    prefix: &str,
    params: &WatchParams,
    api_version: String,
    kind: String,
    metadata_only: bool,
) -> Result<Response, ApiError> {
    // For WatchList, snapshot the current state and open the live watch at the
    // SAME revision so there is no gap or overlap between the initial list and
    // the live stream. Otherwise start from the requested resourceVersion.
    let (initial, live_rev) = if params.send_initial_events {
        let (items, _continue, rev) = storage.list(prefix, 0, None).await?;
        (Some((items, rev)), rev)
    } else {
        (None, params.resource_version.unwrap_or(0))
    };
    let rx = storage.watch(prefix, live_rev).await?;
    Ok(watch::watch_response(
        rx,
        watch::WatchResponseOpts {
            label_selector: params.label_selector.clone(),
            field_selector: params.field_selector.clone(),
            api_version,
            kind,
            allow_bookmarks: params.allow_watch_bookmarks,
            metadata_only,
            initial,
        },
    ))
}

/// Whether the request `Accept`s the metadata-only projection.
pub(crate) fn accept_partial_metadata(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(watch::wants_partial_metadata)
        .unwrap_or(false)
}

/// Project a LIST response to a `PartialObjectMetadataList` when the client asked
/// for `as=PartialObjectMetadata` (metadata informers, e.g. Cilium on CRDs).
pub(crate) fn project_list(mut list: Value, metadata_only: bool) -> Value {
    if !metadata_only {
        return list;
    }
    if let Some(items) = list.get_mut("items").and_then(|i| i.as_array_mut()) {
        for it in items.iter_mut() {
            *it = watch::to_partial_object_metadata(it);
        }
    }
    list["apiVersion"] = json!("meta.k8s.io/v1");
    list["kind"] = json!("PartialObjectMetadataList");
    list
}

/// LIST/WATCH cluster-scoped resources.
pub async fn list_cluster_resources(
    State(state): State<AppState>,
    Path(resource): Path<String>,
    headers: axum::http::HeaderMap,
    RawQuery(query): RawQuery,
) -> Result<Response, ApiError> {
    let params = WatchParams::from_query(query.as_deref().unwrap_or(""));
    let metadata_only = accept_partial_metadata(&headers);
    let prefix = ResourceStorage::cluster_prefix(&resource);

    if params.watch {
        return watch_prefix(
            &state.storage,
            &prefix,
            &params,
            resource_to_api_version(&resource).to_string(),
            resource_to_kind(&resource),
            metadata_only,
        )
        .await;
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

    Ok(Json(project_list(list, metadata_only)).into_response())
}

/// LIST/WATCH namespace-scoped resources in a single namespace.
pub async fn list_namespaced_resources(
    State(state): State<AppState>,
    Path((namespace, resource)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    RawQuery(query): RawQuery,
) -> Result<Response, ApiError> {
    let params = WatchParams::from_query(query.as_deref().unwrap_or(""));
    let metadata_only = accept_partial_metadata(&headers);
    let prefix = ResourceStorage::namespace_prefix(&resource, &namespace);

    if params.watch {
        return watch_prefix(
            &state.storage,
            &prefix,
            &params,
            resource_to_api_version(&resource).to_string(),
            resource_to_kind(&resource),
            metadata_only,
        )
        .await;
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

    Ok(Json(project_list(list, metadata_only)).into_response())
}

/// LIST namespace-scoped resources across all namespaces.
pub async fn list_all_namespaces_resources(
    State(state): State<AppState>,
    Path(resource): Path<String>,
    headers: axum::http::HeaderMap,
    RawQuery(query): RawQuery,
) -> Result<Response, ApiError> {
    let params = WatchParams::from_query(query.as_deref().unwrap_or(""));
    let metadata_only = accept_partial_metadata(&headers);
    let prefix = ResourceStorage::all_namespaces_prefix(&resource);

    if params.watch {
        return watch_prefix(
            &state.storage,
            &prefix,
            &params,
            resource_to_api_version(&resource).to_string(),
            resource_to_kind(&resource),
            metadata_only,
        )
        .await;
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

    Ok(Json(project_list(list, metadata_only)).into_response())
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

/// The subset of `meta/v1` DeleteOptions the apiserver acts on.
#[derive(Default)]
pub(crate) struct DeleteOptions {
    grace_period_seconds: Option<i64>,
    /// Foreground | Background | Orphan (None = the resource default, Background).
    propagation_policy: Option<String>,
    precondition_uid: Option<String>,
    precondition_rv: Option<String>,
    dry_run: bool,
}

/// Parse a DeleteOptions request body (JSON — protobuf is transcoded upstream).
/// An empty/absent body yields defaults (Background, no preconditions).
pub(crate) fn parse_delete_options(body: &[u8]) -> DeleteOptions {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    DeleteOptions {
        grace_period_seconds: v.get("gracePeriodSeconds").and_then(Value::as_i64),
        propagation_policy: v
            .get("propagationPolicy")
            .and_then(Value::as_str)
            .map(String::from),
        precondition_uid: v
            .pointer("/preconditions/uid")
            .and_then(Value::as_str)
            .map(String::from),
        precondition_rv: v
            .pointer("/preconditions/resourceVersion")
            .and_then(Value::as_str)
            .map(String::from),
        // dryRun: ["All"] means don't persist.
        dry_run: v
            .get("dryRun")
            .and_then(Value::as_array)
            .map(|a| a.iter().any(|x| x.as_str() == Some("All")))
            .unwrap_or(false),
    }
}

/// Return an error if `opts` carries preconditions the object doesn't satisfy
/// (RFC: a uid/resourceVersion mismatch is a 409 Conflict).
fn check_preconditions(obj: &Value, opts: &DeleteOptions) -> Result<(), ApiError> {
    if let Some(uid) = &opts.precondition_uid {
        if obj["metadata"]["uid"].as_str() != Some(uid.as_str()) {
            return Err(ApiError::conflict(
                "the UID in the precondition no longer matches the UID of the object",
            ));
        }
    }
    if let Some(rv) = &opts.precondition_rv {
        if obj["metadata"]["resourceVersion"].as_str() != Some(rv.as_str()) {
            return Err(ApiError::conflict(
                "the resourceVersion in the precondition no longer matches the object",
            ));
        }
    }
    Ok(())
}

fn ensure_finalizer(finalizers: &mut Vec<Value>, name: &str) {
    if !finalizers.iter().any(|f| f.as_str() == Some(name)) {
        finalizers.push(Value::String(name.into()));
    }
}

/// A `Status` success object for a completed hard delete.
fn delete_success(name: &str, namespace: Option<&str>, kind: &str) -> Value {
    let mut details = json!({ "name": name, "kind": kind });
    if let Some(ns) = namespace {
        details["namespace"] = json!(ns);
    }
    json!({
        "apiVersion": "v1", "kind": "Status", "metadata": {},
        "status": "Success", "details": details
    })
}

/// Delete `key` honoring DeleteOptions: preconditions (409 on mismatch), dry-run
/// (no persist), finalizers and propagationPolicy (Foreground/Orphan add the
/// corresponding finalizer and set a deletionTimestamp instead of removing — the
/// GC controller then cascades dependents and clears finalizers), and
/// gracePeriodSeconds. Returns the Terminating object or a Success `Status`.
pub(crate) async fn perform_delete(
    state: &AppState,
    key: &str,
    mut obj: Value,
    opts: &DeleteOptions,
    name: &str,
    namespace: Option<&str>,
    kind: &str,
) -> Result<Value, ApiError> {
    check_preconditions(&obj, opts)?;

    // Finalizers the object must outlive: any it already carries, plus the one
    // implied by a Foreground/Orphan propagation policy.
    let mut finalizers: Vec<Value> = obj["metadata"]["finalizers"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    match opts.propagation_policy.as_deref() {
        Some("Foreground") => ensure_finalizer(&mut finalizers, "foregroundDeletion"),
        Some("Orphan") => ensure_finalizer(&mut finalizers, "orphan"),
        _ => {}
    }
    let terminating = !finalizers.is_empty();

    if opts.dry_run {
        return Ok(if terminating {
            obj
        } else {
            delete_success(name, namespace, kind)
        });
    }

    if terminating {
        // Mark for deletion and persist; controllers finish the job. If it was
        // already terminating and its finalizers are now clear, this branch isn't
        // reached (finalizers empty) and the hard delete below removes it.
        if obj["metadata"]["deletionTimestamp"].is_null() {
            obj["metadata"]["deletionTimestamp"] =
                json!(chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string());
        }
        if let Some(g) = opts.grace_period_seconds {
            obj["metadata"]["deletionGracePeriodSeconds"] = json!(g);
        }
        obj["metadata"]["finalizers"] = json!(finalizers);
        let prev_rev = obj["metadata"]["resourceVersion"]
            .as_str()
            .and_then(|r| r.parse::<u64>().ok());
        return state.storage.update(key, obj, prev_rev).await;
    }

    state.storage.delete(key, None).await?;
    Ok(delete_success(name, namespace, kind))
}

/// DELETE a cluster-scoped resource.
pub async fn delete_cluster_resource(
    State(state): State<AppState>,
    Path((resource, name)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let key = ResourceStorage::cluster_key(&resource, &name);
    // Get the object first so we can return it (and inspect it for namespaces).
    let obj = state.storage.get(&key).await?;
    let opts = parse_delete_options(&body);
    check_preconditions(&obj, &opts)?;

    // Namespaces terminate gracefully (#28): instead of a hard delete, mark the
    // namespace Terminating with a deletionTimestamp and a `kubernetes`
    // finalizer. The namespace controller then purges every contained resource
    // and clears the finalizer via /finalize, at which point the object is
    // actually removed. Admission already blocks new content in Terminating
    // namespaces (builtin_admission), so this closes the loop.
    if resource == "namespaces" {
        // Dry-run: report the object without starting termination.
        if opts.dry_run {
            return Ok(Json(obj));
        }
        let finalizers_empty = obj["spec"]["finalizers"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(true);
        let already_terminating = obj["metadata"]["deletionTimestamp"].as_str().is_some();
        if already_terminating && finalizers_empty {
            // Finalization already complete — actually remove it.
            state.storage.delete(&key, None).await?;
            return Ok(Json(obj));
        }

        let mut ns = obj.clone();
        if !ns["metadata"].is_object() {
            ns["metadata"] = json!({});
        }
        ns["metadata"]["deletionTimestamp"] = Value::String(
            chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        );
        if !ns["status"].is_object() {
            ns["status"] = json!({});
        }
        ns["status"]["phase"] = Value::String("Terminating".into());
        // Ensure the `kubernetes` finalizer is present so the object survives
        // until the controller finishes purging content.
        let mut finalizers = ns["spec"]["finalizers"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if !finalizers.iter().any(|f| f.as_str() == Some("kubernetes")) {
            finalizers.push(Value::String("kubernetes".into()));
        }
        if !ns["spec"].is_object() {
            ns["spec"] = json!({});
        }
        ns["spec"]["finalizers"] = Value::Array(finalizers);

        let prev_rev = ns["metadata"]["resourceVersion"]
            .as_str()
            .and_then(|r| r.parse::<u64>().ok());
        let updated = state.storage.update(&key, ns, prev_rev).await?;
        return Ok(Json(updated));
    }

    let out = perform_delete(&state, &key, obj, &opts, &name, None, &resource).await?;
    Ok(Json(out))
}

/// PUT /api/v1/namespaces/{name}/finalize — apply the submitted finalizer list.
/// When the finalizers become empty and the namespace is terminating, the object
/// is actually removed from storage (the namespace controller calls this after
/// purging all contained resources). Mirrors the upstream `/finalize`
/// subresource. (#28)
pub async fn finalize_namespace(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<Value>,
) -> Result<impl IntoResponse, ApiError> {
    let key = ResourceStorage::cluster_key("namespaces", &name);
    let mut obj = state.storage.get(&key).await?;

    // Apply the caller's finalizer list verbatim.
    let submitted = body["spec"]["finalizers"].clone();
    if !obj["spec"].is_object() {
        obj["spec"] = json!({});
    }
    obj["spec"]["finalizers"] = if submitted.is_array() {
        submitted
    } else {
        json!([])
    };

    let empty = obj["spec"]["finalizers"]
        .as_array()
        .map(|a| a.is_empty())
        .unwrap_or(true);
    let terminating = obj["metadata"]["deletionTimestamp"].as_str().is_some();
    if empty && terminating {
        state.storage.delete(&key, None).await?;
        return Ok(Json(obj));
    }

    let prev_rev = obj["metadata"]["resourceVersion"]
        .as_str()
        .and_then(|r| r.parse::<u64>().ok());
    let updated = state.storage.update(&key, obj, prev_rev).await?;
    Ok(Json(updated))
}

/// DELETE a namespace-scoped resource.
pub async fn delete_namespaced_resource(
    State(state): State<AppState>,
    Path((namespace, resource, name)): Path<(String, String, String)>,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let key = ResourceStorage::namespaced_key(&resource, &namespace, &name);
    let obj = state.storage.get(&key).await?;
    let opts = parse_delete_options(&body);
    let out = perform_delete(&state, &key, obj, &opts, &name, Some(&namespace), &resource).await?;
    Ok(Json(out))
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
/// Apply a Kubernetes PATCH body to `target`, dispatching on the request
/// Content-Type (#23):
///
/// - `application/json-patch+json` — RFC 6902 operation list
/// - `application/merge-patch+json` — RFC 7386 merge
/// - `application/strategic-merge-patch+json` — treated as a merge; true
///   strategic merge needs per-field schema metadata we don't carry yet, which
///   only differs for lists with merge keys
/// - `application/apply-patch+yaml` — server-side apply, applied as a merge of
///   the submitted intent (no field-ownership tracking yet)
///
/// An unrecognized/absent Content-Type is treated as a merge patch, matching
/// what most clients expect.
pub fn apply_patch_body(
    target: &mut Value,
    content_type: &str,
    body: &[u8],
) -> Result<(), ApiError> {
    let ct = content_type.split(';').next().unwrap_or("").trim();
    match ct {
        "application/json-patch+json" => {
            let mut ops: Value = serde_json::from_slice(body)
                .map_err(|e| ApiError::invalid(&format!("invalid JSON Patch: {e}")))?;
            normalize_json_patch(target, &mut ops);
            let patch: json_patch::Patch = serde_json::from_value(ops)
                .map_err(|e| ApiError::invalid(&format!("invalid JSON Patch: {e}")))?;
            json_patch::patch(target, &patch)
                .map_err(|e| ApiError::invalid(&format!("JSON Patch could not be applied: {e}")))?;
        }
        "application/apply-patch+yaml" => {
            let patch: Value = serde_yaml::from_slice(body)
                .map_err(|e| ApiError::invalid(&format!("invalid apply patch: {e}")))?;
            json_patch::merge(target, &patch);
        }
        _ => {
            let patch: Value = serde_json::from_slice(body)
                .map_err(|e| ApiError::invalid(&format!("invalid merge patch: {e}")))?;
            json_patch::merge(target, &patch);
        }
    }
    Ok(())
}

/// Normalize an RFC-6902 patch to the leniency kube-apiserver (evanphx/json-patch)
/// has but the strict `json_patch` crate lacks: a `test` whose value is `null`
/// against an **absent** path holds (absent == null). Controllers CAS-guard an
/// optional field this way — e.g. cilium-operator adds the
/// `node.cilium.io/agent-not-ready` taint with
/// `[{test /spec/taints null},{add /spec/taints [...]}]`. The strict crate errors
/// on that test with "path is invalid", so drop the tests that hold, leaving any
/// real (path-present) test for the crate to evaluate.
fn normalize_json_patch(target: &Value, ops: &mut Value) {
    let Some(arr) = ops.as_array_mut() else { return };
    arr.retain(|op| {
        let is_null_test = op.get("op").and_then(Value::as_str) == Some("test")
            && op.get("value").map(Value::is_null).unwrap_or(true);
        if !is_null_test {
            return true;
        }
        // Keep the test only if the path resolves (let the crate check it);
        // an absent path means `test null` holds, so drop it.
        match op.get("path").and_then(Value::as_str) {
            Some(p) => target.pointer(p).is_some(),
            None => true,
        }
    });
}

/// Read-modify-write a stored object through `apply_patch_body`, preserving the
/// object's identity (name/namespace can't be patched away).
pub(crate) async fn patch_stored_object(
    state: &AppState,
    key: &str,
    resource: &str,
    name: &str,
    namespace: Option<&str>,
    headers: &axum::http::HeaderMap,
    body: &[u8],
) -> Result<Value, ApiError> {
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let is_apply = content_type.split(';').next().unwrap_or("").trim()
        == "application/apply-patch+yaml";

    match state.storage.get(key).await {
        Ok(mut existing) => {
            apply_patch_body(&mut existing, content_type, body)?;
            let prev_rev = existing["metadata"]["resourceVersion"]
                .as_str()
                .and_then(|rv| rv.parse::<u64>().ok());
            state.storage.update(key, existing, prev_rev).await
        }
        // Server-side apply is an upsert (KEP-555): applying to a missing object
        // CREATES it — the apply body is the fully-specified desired object.
        // (Merge/JSON/strategic patches still 404 a missing object.)
        Err(e) if is_apply && e.status == StatusCode::NOT_FOUND => {
            let mut obj: Value = serde_yaml::from_slice(body)
                .map_err(|e| ApiError::invalid(&format!("invalid apply patch: {e}")))?;
            ensure_metadata(&mut obj, name, namespace);
            if let Some(ns) = namespace {
                crate::builtin_admission::admit_create(&state.storage, resource, Some(ns), &mut obj)
                    .await?;
            }
            state.storage.create(key, obj).await
        }
        Err(e) => Err(e),
    }
}

/// PATCH a cluster-scoped resource (whole object).
pub async fn patch_cluster_resource(
    State(state): State<AppState>,
    Path((resource, name)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let key = ResourceStorage::cluster_key(&resource, &name);
    let obj = patch_stored_object(&state, &key, &resource, &name, None, &headers, &body).await?;
    Ok(Json(obj))
}

/// PATCH a namespace-scoped resource (whole object).
pub async fn patch_namespaced_resource(
    State(state): State<AppState>,
    Path((namespace, resource, name)): Path<(String, String, String)>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let key = ResourceStorage::namespaced_key(&resource, &namespace, &name);
    let obj =
        patch_stored_object(&state, &key, &resource, &name, Some(&namespace), &headers, &body)
            .await?;
    Ok(Json(obj))
}

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
        // storage.k8s.io/v1 — CSI ecosystem (#24)
        "storageclasses" => "StorageClass",
        "csidrivers" => "CSIDriver",
        "csinodes" => "CSINode",
        "volumeattachments" => "VolumeAttachment",
        "csistoragecapacities" => "CSIStorageCapacity",
        "endpointslices" => "EndpointSlice",
        "certificatesigningrequests" => "CertificateSigningRequest",
        "priorityclasses" => "PriorityClass",
        "poddisruptionbudgets" => "PodDisruptionBudget",
        other => other,
    };
    format!("{singular}List")
}

/// The `apiVersion` a built-in resource plural belongs to. Watch tombstones and
/// bookmarks must carry TypeMeta or client-go can't decode them, and the generic
/// handlers only see the plural — not the group from the route.
pub fn resource_to_api_version(resource: &str) -> &'static str {
    match resource {
        "deployments" | "replicasets" | "statefulsets" | "daemonsets" | "controllerrevisions" => {
            "apps/v1"
        }
        "jobs" | "cronjobs" => "batch/v1",
        "leases" => "coordination.k8s.io/v1",
        "endpointslices" => "discovery.k8s.io/v1",
        "storageclasses" | "csidrivers" | "csinodes" | "volumeattachments"
        | "csistoragecapacities" => "storage.k8s.io/v1",
        "clusterroles" | "clusterrolebindings" | "roles" | "rolebindings" => {
            "rbac.authorization.k8s.io/v1"
        }
        "certificatesigningrequests" => "certificates.k8s.io/v1",
        "customresourcedefinitions" => "apiextensions.k8s.io/v1",
        "horizontalpodautoscalers" => "autoscaling/v2",
        "networkpolicies" | "ingresses" | "ingressclasses" => "networking.k8s.io/v1",
        "priorityclasses" => "scheduling.k8s.io/v1",
        "poddisruptionbudgets" => "policy/v1",
        "mutatingwebhookconfigurations" | "validatingwebhookconfigurations" => {
            "admissionregistration.k8s.io/v1"
        }
        "gatewayclasses" | "gateways" | "httproutes" => "gateway.networking.k8s.io/v1",
        "apiservices" => "apiregistration.k8s.io/v1",
        "podmigrations" => "rustkube.io/v1alpha1",
        // Core group.
        _ => "v1",
    }
}

/// Singular kind for a resource plural (drops the `List` suffix).
pub fn resource_to_kind(resource: &str) -> String {
    let list = resource_to_list_kind(resource);
    list.strip_suffix("List").unwrap_or(&list).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // rustkube#9: a client request with an unexpected shape must never panic.
    #[test]
    #[test]
    fn merge_patch_updates_and_deletes_fields() {
        // RFC 7386: null removes a key, objects merge recursively.
        let mut obj = json!({"spec":{"replicas":1,"paused":true},"status":{"phase":"A"}});
        apply_patch_body(
            &mut obj,
            "application/merge-patch+json",
            br#"{"spec":{"replicas":3,"paused":null}}"#,
        )
        .unwrap();
        assert_eq!(obj["spec"]["replicas"], 3);
        assert!(obj["spec"].get("paused").is_none(), "null must delete the key");
        assert_eq!(obj["status"]["phase"], "A", "untouched fields survive");
    }

    #[test]
    fn delete_options_parse_and_preconditions() {
        let opts = parse_delete_options(
            br#"{"gracePeriodSeconds":30,"propagationPolicy":"Foreground",
                 "preconditions":{"uid":"abc","resourceVersion":"42"},"dryRun":["All"]}"#,
        );
        assert_eq!(opts.grace_period_seconds, Some(30));
        assert_eq!(opts.propagation_policy.as_deref(), Some("Foreground"));
        assert!(opts.dry_run);

        let obj = json!({"metadata": {"uid": "abc", "resourceVersion": "42"}});
        assert!(check_preconditions(&obj, &opts).is_ok());
        // A uid mismatch is a Conflict.
        let bad = parse_delete_options(br#"{"preconditions":{"uid":"WRONG"}}"#);
        assert!(check_preconditions(&obj, &bad).is_err());
        // Empty body → defaults (Background, no preconditions).
        let empty = parse_delete_options(b"");
        assert!(empty.propagation_policy.is_none() && !empty.dry_run);
        assert!(check_preconditions(&obj, &empty).is_ok());
    }

    #[test]
    fn json_patch_test_null_on_absent_path_holds() {
        // cilium-operator's node-taint CAS: `test /spec/taints null` guards
        // `add /spec/taints [...]`. taints is absent, so the test must hold and
        // the add must apply (was rejected "path is invalid" — blocked Cilium).
        let mut node = json!({"spec": {"podCIDR": "10.244.0.0/24"}});
        apply_patch_body(
            &mut node,
            "application/json-patch+json",
            br#"[{"op":"test","path":"/spec/taints","value":null},
                 {"op":"add","path":"/spec/taints","value":[{"key":"node.cilium.io/agent-not-ready","effect":"NoSchedule"}]}]"#,
        )
        .unwrap();
        assert_eq!(node["spec"]["taints"][0]["key"], "node.cilium.io/agent-not-ready");

        // A `test null` against a path that IS present-and-non-null must still fail.
        let mut n2 = json!({"spec": {"taints": [{"key": "x"}]}});
        assert!(apply_patch_body(
            &mut n2,
            "application/json-patch+json",
            br#"[{"op":"test","path":"/spec/taints","value":null}]"#,
        )
        .is_err());
    }

    #[test]
    fn json_patch_rfc6902_applies_ops() {
        let mut obj = json!({"spec":{"replicas":1}});
        apply_patch_body(
            &mut obj,
            "application/json-patch+json",
            br#"[{"op":"replace","path":"/spec/replicas","value":5}]"#,
        )
        .unwrap();
        assert_eq!(obj["spec"]["replicas"], 5);
    }

    #[test]
    fn content_type_params_and_default_are_handled() {
        // charset parameter must not break dispatch; absent CT defaults to merge.
        let mut obj = json!({"a":1});
        apply_patch_body(&mut obj, "application/merge-patch+json; charset=utf-8", br#"{"a":2}"#).unwrap();
        assert_eq!(obj["a"], 2);
        apply_patch_body(&mut obj, "", br#"{"a":3}"#).unwrap();
        assert_eq!(obj["a"], 3);
    }

    #[test]
    fn malformed_patch_is_an_error_not_a_panic() {
        let mut obj = json!({"a":1});
        assert!(apply_patch_body(&mut obj, "application/merge-patch+json", b"not json").is_err());
        assert!(apply_patch_body(&mut obj, "application/json-patch+json", b"{}").is_err());
    }

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

#[cfg(test)]
mod list_kind_tests {
    use super::resource_to_list_kind;

    #[test]
    fn csi_and_group_resources_map_to_proper_kinds() {
        // storage.k8s.io/v1 (#24) and other non-core groups must not fall
        // through to the raw plural, which would emit e.g. "storageclassesList".
        assert_eq!(resource_to_list_kind("storageclasses"), "StorageClassList");
        assert_eq!(resource_to_list_kind("csidrivers"), "CSIDriverList");
        assert_eq!(resource_to_list_kind("csinodes"), "CSINodeList");
        assert_eq!(resource_to_list_kind("volumeattachments"), "VolumeAttachmentList");
        assert_eq!(
            resource_to_list_kind("csistoragecapacities"),
            "CSIStorageCapacityList"
        );
        assert_eq!(resource_to_list_kind("endpointslices"), "EndpointSliceList");
    }
}
