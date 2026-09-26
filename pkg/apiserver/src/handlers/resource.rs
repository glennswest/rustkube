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
            transform: None,
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
    // The group's version, not "v1".
    //
    // **A client looks up (apiVersion, kind) as a pair.** Saying "v1" for a
    // list of NetworkPolicies claims core/v1, where no NetworkPolicyList is
    // registered, so a typed client rejects the whole response — Cilium
    // reported `no kind "NetworkPolicyList" is registered for version "v1"`
    // and stopped watching policies and endpoint slices entirely. Every
    // non-core group was affected. The watch path beside this already had it
    // right, which is why watches worked and lists did not.
    let mut list = json!({
        "apiVersion": resource_to_api_version(&resource),
        "kind": kind,
        "metadata": {
            "resourceVersion": revision.to_string()
        },
        "items": items
    });

    if let Some(token) = continue_token {
        list["metadata"]["continue"] = Value::String(token);
    }

    {
        let body = project_list(list, metadata_only);
        // `kubectl get` and `oc get` ask for a Table and print NAME + AGE when
        // they do not get one — which is why every listing looked empty of
        // detail while the objects were complete (#53).
        if crate::table::wants_table(&headers) {
            return Ok(Json(crate::table::to_table(&resource, body)).into_response());
        }
        Ok(Json(body).into_response())
    }
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
    // The group's version, not "v1".
    //
    // **A client looks up (apiVersion, kind) as a pair.** Saying "v1" for a
    // list of NetworkPolicies claims core/v1, where no NetworkPolicyList is
    // registered, so a typed client rejects the whole response — Cilium
    // reported `no kind "NetworkPolicyList" is registered for version "v1"`
    // and stopped watching policies and endpoint slices entirely. Every
    // non-core group was affected. The watch path beside this already had it
    // right, which is why watches worked and lists did not.
    let mut list = json!({
        "apiVersion": resource_to_api_version(&resource),
        "kind": kind,
        "metadata": {
            "resourceVersion": revision.to_string()
        },
        "items": items
    });

    if let Some(token) = continue_token {
        list["metadata"]["continue"] = Value::String(token);
    }

    {
        let body = project_list(list, metadata_only);
        // `kubectl get` and `oc get` ask for a Table and print NAME + AGE when
        // they do not get one — which is why every listing looked empty of
        // detail while the objects were complete (#53).
        if crate::table::wants_table(&headers) {
            return Ok(Json(crate::table::to_table(&resource, body)).into_response());
        }
        Ok(Json(body).into_response())
    }
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
    // The group's version, not "v1".
    //
    // **A client looks up (apiVersion, kind) as a pair.** Saying "v1" for a
    // list of NetworkPolicies claims core/v1, where no NetworkPolicyList is
    // registered, so a typed client rejects the whole response — Cilium
    // reported `no kind "NetworkPolicyList" is registered for version "v1"`
    // and stopped watching policies and endpoint slices entirely. Every
    // non-core group was affected. The watch path beside this already had it
    // right, which is why watches worked and lists did not.
    let mut list = json!({
        "apiVersion": resource_to_api_version(&resource),
        "kind": kind,
        "metadata": {
            "resourceVersion": revision.to_string()
        },
        "items": items
    });

    if let Some(token) = continue_token {
        list["metadata"]["continue"] = Value::String(token);
    }

    {
        let body = project_list(list, metadata_only);
        // `kubectl get` and `oc get` ask for a Table and print NAME + AGE when
        // they do not get one — which is why every listing looked empty of
        // detail while the objects were complete (#53).
        if crate::table::wants_table(&headers) {
            return Ok(Json(crate::table::to_table(&resource, body)).into_response());
        }
        Ok(Json(body).into_response())
    }
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

    crate::builtin_admission::admit_create(
        &state.storage, &resource, None, &mut body, &state.service_cidr,
    )
    .await?;

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
    crate::builtin_admission::admit_create(
        &state.storage, &resource, Some(&namespace), &mut body, &state.service_cidr,
    )
        .await?;

    let key = ResourceStorage::namespaced_key(&resource, &namespace, &name);
    let obj = state.storage.create(&key, body).await?;
    Ok((StatusCode::CREATED, Json(obj)))
}

/// Persist an updated object — or remove it, when the update was the write
/// that cleared its last finalizer.
///
/// A finalizer is a promise that something has to happen before an object
/// goes away: the delete sets `deletionTimestamp` and the object stays until
/// the list is empty. **The write that empties it is the delete.** Without
/// this half, a controller removes its finalizer and the object lives forever
/// with a `deletionTimestamp` on it — a PVC that never goes, a PV that never
/// releases, a namespace stuck Terminating — and every controller that treats
/// "being deleted" as in-flight waits on it for the life of the cluster.
pub(crate) async fn persist_or_finalize(
    state: &AppState,
    key: &str,
    obj: Value,
) -> Result<Value, ApiError> {
    let terminating = obj["metadata"]["deletionTimestamp"].as_str().is_some();
    let finalizers_left = obj["metadata"]["finalizers"]
        .as_array()
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    let prev_rev = obj["metadata"]["resourceVersion"]
        .as_str()
        .and_then(|rv| rv.parse::<u64>().ok());
    if terminating && !finalizers_left {
        state.storage.delete(key, prev_rev).await?;
        return Ok(obj);
    }
    state.storage.update(key, obj, prev_rev).await
}

/// PUT — update a cluster-scoped resource.
pub async fn update_cluster_resource(
    State(state): State<AppState>,
    Path((resource, name)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<impl IntoResponse, ApiError> {
    let key = ResourceStorage::cluster_key(&resource, &name);
    let obj = put_object(&state, &key, &name, None, body).await?;
    Ok(Json(obj))
}

/// PUT of a whole object: the body replaces the stored one, **except for what
/// the server owns**.
///
/// It used to be stored as sent, which upstream never does:
/// - a PUT to an object that does not exist created it, where upstream
///   answers 404 (no built-in resource allows create-on-update);
/// - a body could rename the object away from its URL, or carry a different
///   `uid` or `creationTimestamp` — or a zero-valued one, which is what a
///   protobuf body decodes to — and that is what was stored.
///
/// Now `uid` and `creationTimestamp` come from the stored object; a body
/// that names a different name or namespace is refused (400), and one that
/// names a different uid is a failed precondition (409), as upstream. The
/// write is conditional on the body's resourceVersion when it has one.
pub(crate) async fn put_object(
    state: &AppState,
    key: &str,
    name: &str,
    namespace: Option<&str>,
    mut body: Value,
) -> Result<Value, ApiError> {
    let existing = state.storage.get(key).await?;
    if !body.is_object() {
        return Err(ApiError::invalid("the body of a PUT must be an object"));
    }
    if !body["metadata"].is_object() {
        body["metadata"] = json!({});
    }
    let meta = &body["metadata"];
    let bad_request = |message: String| ApiError {
        status: StatusCode::BAD_REQUEST,
        reason: "BadRequest".into(),
        message,
    };
    if let Some(n) = meta["name"].as_str().filter(|n| !n.is_empty() && *n != name) {
        return Err(bad_request(format!(
            "the name of the object ({n}) does not match the name on the URL ({name})"
        )));
    }
    if let (Some(ns), Some(want)) = (meta["namespace"].as_str().filter(|n| !n.is_empty()), namespace) {
        if ns != want {
            return Err(bad_request(format!(
                "the namespace of the object ({ns}) does not match the namespace on the URL ({want})"
            )));
        }
    }
    let stored_uid = existing["metadata"]["uid"].clone();
    if let Some(u) = meta["uid"].as_str().filter(|u| !u.is_empty()) {
        if Some(u) != stored_uid.as_str() {
            return Err(ApiError::conflict(&format!(
                "Precondition failed: UID in precondition: {}, UID in object meta: {u}",
                stored_uid.as_str().unwrap_or("")
            )));
        }
    }
    keep_server_fields(&mut body, &existing, name, namespace);
    persist_or_finalize(state, key, body).await
}

/// Carry the fields the server owns from the stored object into its
/// replacement: identity (name, namespace from the URL), `uid` and
/// `creationTimestamp`. Shared by PUT and PATCH (#67).
pub(crate) fn keep_server_fields(obj: &mut Value, stored: &Value, name: &str, namespace: Option<&str>) {
    if !obj["metadata"].is_object() {
        obj["metadata"] = json!({});
    }
    obj["metadata"]["name"] = json!(name);
    if let Some(ns) = namespace {
        obj["metadata"]["namespace"] = json!(ns);
    }
    for field in ["uid", "creationTimestamp"] {
        match stored["metadata"].get(field) {
            Some(v) if !v.is_null() => obj["metadata"][field] = v.clone(),
            _ => {}
        }
    }
}

/// PUT — update a namespace-scoped resource.
pub async fn update_namespaced_resource(
    State(state): State<AppState>,
    Path((namespace, resource, name)): Path<(String, String, String)>,
    Json(body): Json<Value>,
) -> Result<impl IntoResponse, ApiError> {
    let key = ResourceStorage::namespaced_key(&resource, &namespace, &name);
    let obj = put_object(&state, &key, &name, Some(&namespace), body).await?;
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

    // Give the ClusterIP back before the Service goes.
    //
    // A leaked claim is worse than a leaked Service: the Service is visible in
    // `get svc` and the claim is not, so the range quietly fills with
    // allocations nothing owns. Released before the delete, because after it
    // the address is no longer recoverable from the object.
    if kind == "services" {
        crate::service_ip::release(&state.storage, &obj).await;
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

    // Namespaces terminate gracefully (#28).
    if resource == "namespaces" {
        return Ok(Json(terminate_namespace(&state, &name, obj, &opts).await?));
    }

    let out = perform_delete(&state, &key, obj, &opts, &name, None, &resource).await?;
    Ok(Json(out))
}

/// Start a namespace's graceful deletion — the namespace half of DELETE, and
/// of deleting the Project that is the same namespace (#97).
///
/// Namespaces terminate gracefully (#28): instead of a hard delete, the
/// namespace is marked Terminating with a deletionTimestamp and a `kubernetes`
/// finalizer. The namespace controller then purges every contained resource
/// and clears the finalizer via /finalize, at which point the object is
/// actually removed. Admission already blocks new content in Terminating
/// namespaces (builtin_admission), so this closes the loop.
pub(crate) async fn terminate_namespace(
    state: &AppState,
    name: &str,
    obj: Value,
    opts: &DeleteOptions,
) -> Result<Value, ApiError> {
    let key = ResourceStorage::cluster_key("namespaces", name);
    check_preconditions(&obj, opts)?;
    // Dry-run: report the object without starting termination.
    if opts.dry_run {
        return Ok(obj);
    }
    let finalizers_empty = obj["spec"]["finalizers"]
        .as_array()
        .map(|a| a.is_empty())
        .unwrap_or(true);
    let already_terminating = obj["metadata"]["deletionTimestamp"].as_str().is_some();
    if already_terminating && finalizers_empty {
        // Finalization already complete — actually remove it.
        state.storage.delete(&key, None).await?;
        return Ok(obj);
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
    state.storage.update(&key, ns, prev_rev).await
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
    Ok(Json(put_status(&state, &key, &body).await?))
}

/// PUT of a `/status` subresource: the body's `status` onto the stored
/// object, everything else left as stored (#78).
///
/// **Conditional on the body's `resourceVersion`**, as upstream's PUT always
/// is. This used to CAS against a fresh read instead, which was wrong both
/// ways round: a controller writing status from a stale cache overwrote the
/// newer status it had never seen, and a writer whose version was current
/// could still get a 409 when another write landed between the read and the
/// swap. A leader-elected controller relies on that 409 to learn that another
/// replica wrote.
///
/// A body with no resourceVersion is an unconditional update, which upstream
/// allows for status: it is retried against a fresh read until it lands, like
/// a PATCH without one (#77).
///
/// Shared by every status PUT — core and grouped, cluster-scoped and
/// namespaced, custom resources, and the CSR `/approval` subresource.
pub(crate) async fn put_status(
    state: &AppState,
    key: &str,
    body: &Value,
) -> Result<Value, ApiError> {
    let precondition = body["metadata"]["resourceVersion"]
        .as_str()
        .filter(|rv| !rv.is_empty())
        .map(str::to_owned);
    guaranteed_update(state, key, precondition, |mut existing| {
        if let Some(status) = body.get("status") {
            existing["status"] = status.clone();
        }
        Ok(existing)
    })
    .await
}

/// PATCH status for a cluster-scoped resource.
/// Apply a Kubernetes PATCH body to `target`, dispatching on the request
/// Content-Type (#23):
///
/// - `application/json-patch+json` — RFC 6902 operation list
/// - `application/merge-patch+json` — RFC 7386 merge
/// - `application/strategic-merge-patch+json` — a merge in which the lists
///   named in `strategic_merge_key` (conditions by `type`, containers by
///   `name`, …) are merged by that key rather than replaced (#47)
/// - `application/apply-patch+yaml` — a plain merge here. The built-in object
///   PATCH handler sends apply to `crate::apply::server_side_apply` instead
///   (`managedFields` ownership and conflicts); the other callers get this
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
            json_patch::patch(target, &patch).map_err(|e| {
                // **Say which patch.** "operation '/0' failed at path
                // '/spec/taints'" names an index into a document the reader
                // does not have, so the next question is always "what was the
                // patch" — and on a node with no shell there is no way to
                // find out. Quoting it turns one round trip into none.
                let body = String::from_utf8_lossy(body);
                ApiError::invalid(&format!(
                    "JSON Patch could not be applied: {e} — patch was: {}",
                    body.chars().take(400).collect::<String>()
                ))
            })?;
        }
        "application/apply-patch+yaml" => {
            let patch: Value = serde_yaml::from_slice(body)
                .map_err(|e| ApiError::invalid(&format!("invalid apply patch: {e}")))?;
            json_patch::merge(target, &patch);
        }
        "application/strategic-merge-patch+json" => {
            // Merge keyed lists (conditions by type, containers by name, …) by
            // their patchMergeKey instead of replacing them wholesale (#47).
            let patch: Value = serde_json::from_slice(body)
                .map_err(|e| ApiError::invalid(&format!("invalid strategic merge patch: {e}")))?;
            strategic_merge(target, &patch);
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
        if op.get("op").and_then(Value::as_str) != Some("test") {
            return true;
        }
        // A test whose value is "nothing" — null, or an empty list or object.
        //
        // **Absent and empty are the same thing to a controller.** Go marshals
        // a nil slice as `null` and an allocated-but-empty one as `[]`, and
        // which one a client sends is an accident of how it built the struct;
        // cilium-operator CAS-guards the node taint list either way. Treating
        // only `null` as holding made the empty-list spelling fail against a
        // node that simply has no taints yet.
        let holds_when_absent = match op.get("value") {
            None | Some(Value::Null) => true,
            Some(Value::Array(a)) => a.is_empty(),
            Some(Value::Object(o)) => o.is_empty(),
            _ => false,
        };
        if !holds_when_absent {
            return true;
        }
        // Keep the test only if the path resolves (let the crate check it);
        // an absent path means the test holds, so drop it.
        match op.get("path").and_then(Value::as_str) {
            Some(p) => target.pointer(p).is_some(),
            None => true,
        }
    });

    // `replace` on an absent member whose parent exists becomes `add`.
    //
    // RFC 6902 says replace requires the target to exist; kube-apiserver's
    // evanphx/json-patch is lenient, and controllers rely on that leniency
    // because on a real cluster the field usually *does* exist and they never
    // find out. Here the node genuinely has no taints, so the strict reading
    // rejects a patch that works everywhere else.
    for op in arr.iter_mut() {
        if op.get("op").and_then(Value::as_str) != Some("replace") {
            continue;
        }
        let Some(path) = op.get("path").and_then(Value::as_str).map(str::to_owned) else {
            continue;
        };
        if target.pointer(&path).is_some() {
            continue; // present: an ordinary replace
        }
        // Only when the parent is there — `add` cannot create a path two
        // levels deep either, and inventing one would hide a real mistake.
        let parent = match path.rfind('/') {
            Some(0) | None => String::new(),
            Some(i) => path[..i].to_string(),
        };
        let parent_exists = if parent.is_empty() {
            true
        } else {
            target.pointer(&parent).is_some()
        };
        if parent_exists {
            if let Some(o) = op.as_object_mut() {
                o.insert("op".to_string(), Value::String("add".to_string()));
            }
        }
    }
}

/// How long a patch keeps re-reading and re-applying after losing CAS races
/// before the 409 is let through. Upstream does not bound it at all; this keeps
/// a pathologically hot key from pinning a request forever, and is well inside
/// the minute a client waits for a response.
const PATCH_RETRY_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

/// The pause before retry `attempt`: random, in a window that doubles from
/// 10 ms to a 200 ms ceiling.
///
/// **A retry with no pause starves.** The first version retried at once, 16
/// times, and lost all 16 to a writer patching the same object in a loop: each
/// re-read lands just after the other writer's commit, both swap against the
/// same revision, and the other one's swap is ahead in the queue — every time,
/// in lockstep, for as long as it keeps writing. Measured on dev with two curl
/// loops, the first patch of each loop still 409'd after all 16. Random delay
/// breaks the lockstep; the window grows so a busy key sheds load rather than
/// piling retries onto it.
fn patch_retry_pause(attempt: u32) -> std::time::Duration {
    let window_ms = (10u64 << attempt.clamp(1, 6).saturating_sub(1)).min(200);
    let jitter = (uuid::Uuid::new_v4().as_u128() as u64) % window_ms;
    std::time::Duration::from_millis(jitter)
}

/// The `resourceVersion` a patch body names, if it names one.
///
/// This is the only thing that makes a PATCH conditional. A JSON Patch states
/// its conditions as `test` operations, which are evaluated against each
/// re-read like the rest of the patch, so it has none here.
fn patch_precondition(content_type: &str, body: &[u8]) -> Option<String> {
    let doc: Value = match content_type.split(';').next().unwrap_or("").trim() {
        "application/json-patch+json" => return None,
        "application/apply-patch+yaml" => serde_yaml::from_slice(body).ok()?,
        _ => serde_json::from_slice(body).ok()?,
    };
    doc["metadata"]["resourceVersion"]
        .as_str()
        .filter(|rv| !rv.is_empty())
        .map(str::to_owned)
}

/// Apply a patch the way upstream's `GuaranteedUpdate` does (#77): read, apply,
/// compare-and-swap against the revision just read, and on losing the swap
/// read again and re-apply.
///
/// **A PATCH that names no resourceVersion never conflicts.** A merge patch
/// says "set these fields", not "set these fields on the version I saw", and
/// clients are written to that contract — client-go, kube-rs and kubectl do not
/// retry a merge patch. Returning the CAS miss as 409 lost writes at random: a
/// `spec.online` flip from one client vanished while a controller was writing
/// `/status` every few seconds. Only a patch that carries a resourceVersion
/// gets a 409, and only when that version is no longer current.
///
/// `mutate` gets the freshly read object and returns what to store; an error
/// from it (a bad patch, a server-side-apply field conflict) is final and not
/// retried — only the store's CAS miss is.
pub(crate) async fn guaranteed_patch<F>(
    state: &AppState,
    key: &str,
    content_type: &str,
    body: &[u8],
    mutate: F,
) -> Result<Value, ApiError>
where
    F: FnMut(Value) -> Result<Value, ApiError>,
{
    let precondition = patch_precondition(content_type, body);
    guaranteed_update(state, key, precondition, mutate).await
}

/// The loop under [`guaranteed_patch`], for a caller that already knows its
/// precondition.
///
/// With `precondition` set, the write happens only if the object is still at
/// that resourceVersion: a stale one is a 409 before anything is written, and
/// a swap lost to a concurrent writer re-reads, finds the version moved on,
/// and is a 409 too. Without one, a lost swap is retried against the fresh
/// read until it lands, within `PATCH_RETRY_BUDGET`.
pub(crate) async fn guaranteed_update<F>(
    state: &AppState,
    key: &str,
    precondition: Option<String>,
    mut mutate: F,
) -> Result<Value, ApiError>
where
    F: FnMut(Value) -> Result<Value, ApiError>,
{
    let started = std::time::Instant::now();
    let mut attempt = 0;
    loop {
        let fresh = state.storage.get(key).await?;
        let read_rv = fresh["metadata"]["resourceVersion"].as_str().unwrap_or("").to_string();
        if let Some(want) = &precondition {
            if *want != read_rv {
                let name = fresh["metadata"]["name"].as_str().unwrap_or("");
                return Err(ApiError::conflict(&format!(
                    "Operation cannot be fulfilled on \"{name}\": the object has been modified; \
                     please apply your changes to the latest version and try again"
                )));
            }
        }
        let stored_meta = json!({ "metadata": {
            "uid": fresh["metadata"]["uid"].clone(),
            "creationTimestamp": fresh["metadata"]["creationTimestamp"].clone(),
        }});
        let name = fresh["metadata"]["name"].as_str().unwrap_or_default().to_string();
        let namespace = fresh["metadata"]["namespace"].as_str().map(str::to_owned);
        let mut obj = mutate(fresh)?;
        if !obj["metadata"].is_object() {
            return Err(ApiError::invalid("metadata must be an object"));
        }
        // A patch cannot rename the object or rewrite what the server owns —
        // the conformance suite's own ConfigMap patch sends
        // `creationTimestamp: null`, which used to delete it (#67).
        keep_server_fields(&mut obj, &stored_meta, &name, namespace.as_deref());
        // Swap against what was read, whatever the patch did to the field.
        obj["metadata"]["resourceVersion"] = Value::String(read_rv);
        match persist_or_finalize(state, key, obj).await {
            Err(e) if e.reason == "Conflict" && started.elapsed() < PATCH_RETRY_BUDGET => {
                attempt += 1;
                tokio::time::sleep(patch_retry_pause(attempt)).await;
            }
            result => return result,
        }
    }
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
    query: &str,
    body: &[u8],
) -> Result<Value, ApiError> {
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let is_apply = content_type.split(';').next().unwrap_or("").trim()
        == "application/apply-patch+yaml";
    let (field_manager, force) = apply_params(query);
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    let mut attempt = 0;
    loop {
        let patched = guaranteed_patch(state, key, content_type, body, |existing| {
            if !is_apply {
                let mut existing = existing;
                apply_patch_body(&mut existing, content_type, body)?;
                return Ok(existing);
            }
            // Server-side apply: merge the intent, track field ownership in
            // managedFields, and reject a foreign-owned change unless forced.
            let applied: Value = serde_yaml::from_slice(body)
                .map_err(|e| ApiError::invalid(&format!("invalid apply patch: {e}")))?;
            crate::apply::server_side_apply(existing, &applied, &field_manager, &now, force)
                .map_err(|c| {
                    ApiError::conflict(&format!(
                        "Apply failed with 1 conflict: field \"{}\" is managed by \"{}\" \
                         (set fieldManager force to override)",
                        c.field, c.manager
                    ))
                })
        })
        .await;
        match patched {
            // Server-side apply is an upsert (KEP-555): applying to a missing
            // object CREATES it with the requester as the field manager.
            // (Merge/JSON/strategic patches still 404 a missing object.)
            Err(e) if is_apply && e.status == StatusCode::NOT_FOUND => {
                let applied: Value = serde_yaml::from_slice(body)
                    .map_err(|e| ApiError::invalid(&format!("invalid apply patch: {e}")))?;
                // No existing owners on a create, so this never conflicts.
                let mut obj = crate::apply::server_side_apply(json!({}), &applied, &field_manager, &now, true)
                    .expect("create apply cannot conflict");
                ensure_metadata(&mut obj, name, namespace);
                if let Some(ns) = namespace {
                    crate::builtin_admission::admit_create(
                        &state.storage, resource, Some(ns), &mut obj, &state.service_cidr,
                    )
                        .await?;
                }
                match state.storage.create(key, obj).await {
                    // Somebody created it between the read and the create:
                    // apply to theirs, as the next attempt will.
                    Err(e) if e.reason == "AlreadyExists" && attempt < 3 => attempt += 1,
                    result => return result,
                }
            }
            result => return result,
        }
    }
}

/// Parse `fieldManager` and `force` from the request query for server-side apply.
fn apply_params(query: &str) -> (String, bool) {
    let mut manager = String::new();
    let mut force = false;
    for (k, v) in form_urlencoded::parse(query.as_bytes()) {
        match k.as_ref() {
            "fieldManager" => manager = v.into_owned(),
            "force" => force = v == "true" || v == "1",
            _ => {}
        }
    }
    if manager.is_empty() {
        manager = "apply".to_string();
    }
    (manager, force)
}

/// PATCH a cluster-scoped resource (whole object).
pub async fn patch_cluster_resource(
    State(state): State<AppState>,
    Path((resource, name)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    RawQuery(query): RawQuery,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let key = ResourceStorage::cluster_key(&resource, &name);
    let q = query.as_deref().unwrap_or("");
    let obj = patch_stored_object(&state, &key, &resource, &name, None, &headers, q, &body).await?;
    Ok(Json(obj))
}

/// PATCH a namespace-scoped resource (whole object).
pub async fn patch_namespaced_resource(
    State(state): State<AppState>,
    Path((namespace, resource, name)): Path<(String, String, String)>,
    headers: axum::http::HeaderMap,
    RawQuery(query): RawQuery,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let key = ResourceStorage::namespaced_key(&resource, &namespace, &name);
    let q = query.as_deref().unwrap_or("");
    let obj =
        patch_stored_object(&state, &key, &resource, &name, Some(&namespace), &headers, q, &body)
            .await?;
    Ok(Json(obj))
}

pub async fn patch_cluster_status(
    State(state): State<AppState>,
    Path((resource, name)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let key = ResourceStorage::cluster_key(&resource, &name);
    let ct = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let obj = guaranteed_patch(&state, &key, ct, &body, |mut existing| {
        apply_status_patch(&mut existing, ct, &body)?;
        Ok(existing)
    })
    .await?;
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
    Ok(Json(put_status(&state, &key, &body).await?))
}

/// PATCH status for a namespace-scoped resource.
pub async fn patch_namespaced_status(
    State(state): State<AppState>,
    Path((namespace, resource, name)): Path<(String, String, String)>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let key = ResourceStorage::namespaced_key(&resource, &namespace, &name);
    let ct = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let obj = guaranteed_patch(&state, &key, ct, &body, |mut existing| {
        apply_status_patch(&mut existing, ct, &body)?;
        Ok(existing)
    })
    .await?;
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

/// Well-known Kubernetes list `patchMergeKey`s by field name — the subset needed
/// for strategic-merge patches. A list field not listed here is replaced whole
/// (RFC-7386 semantics). `status.conditions` keyed by `type` is the critical one:
/// without it, a patch adding NetworkUnavailable would drop Ready et al. (#47).
fn strategic_merge_key(field: &str) -> Option<&'static str> {
    Some(match field {
        "conditions" => "type",
        "containers" | "initContainers" | "ephemeralContainers" | "volumes"
        | "volumeMounts" | "imagePullSecrets" | "env" | "envFrom" => "name",
        "ports" => "containerPort",
        "ownerReferences" => "uid",
        "hostAliases" => "ip",
        "topologySpreadConstraints" => "topologyKey",
        _ => return None,
    })
}

/// Strategic-merge-patch (schema-lite): recursively merge objects; merge list
/// fields that have a known `patchMergeKey` by upserting entries by that key
/// (preserving unmatched existing entries); overwrite scalars and unkeyed lists;
/// a `null` value deletes the key.
pub(crate) fn strategic_merge(target: &mut Value, patch: &Value) {
    let Value::Object(p) = patch else {
        *target = patch.clone();
        return;
    };
    if !target.is_object() {
        *target = json!({});
    }
    let t = target.as_object_mut().unwrap();
    for (k, pv) in p {
        if pv.is_null() {
            t.remove(k);
            continue;
        }
        let mk = strategic_merge_key(k);
        match t.get_mut(k) {
            Some(tv) if mk.is_some() && tv.is_array() && pv.is_array() => {
                strategic_merge_list(
                    tv.as_array_mut().unwrap(),
                    pv.as_array().unwrap(),
                    mk.unwrap(),
                );
            }
            Some(tv) if tv.is_object() && pv.is_object() => strategic_merge(tv, pv),
            _ => {
                t.insert(k.clone(), pv.clone());
            }
        }
    }
}

/// Upsert each `patch` item into `target` by `key`: an item whose key matches an
/// existing entry is strategic-merged into it; a new key is appended.
fn strategic_merge_list(target: &mut Vec<Value>, patch: &[Value], key: &str) {
    for pitem in patch {
        let pkey = pitem.get(key);
        match pkey.and_then(|pk| target.iter_mut().find(|t| t.get(key) == Some(pk))) {
            Some(existing) => strategic_merge(existing, pitem),
            None => target.push(pitem.clone()),
        }
    }
}

/// Apply a status subresource patch to `existing`, honoring the patch
/// Content-Type: strategic-merge (merge keyed lists like conditions), merge-patch
/// (RFC-7386, arrays replaced), or JSON patch.
fn apply_status_patch(existing: &mut Value, content_type: &str, body: &[u8]) -> Result<(), ApiError> {
    match content_type.split(';').next().unwrap_or("").trim() {
        "application/json-patch+json" => {
            let mut ops: Value = serde_json::from_slice(body)
                .map_err(|e| ApiError::invalid(&format!("invalid JSON Patch: {e}")))?;
            normalize_json_patch(existing, &mut ops);
            let patch: json_patch::Patch = serde_json::from_value(ops)
                .map_err(|e| ApiError::invalid(&format!("invalid JSON Patch: {e}")))?;
            json_patch::patch(existing, &patch)
                .map_err(|e| ApiError::invalid(&format!("JSON Patch could not be applied: {e}")))?;
        }
        "application/strategic-merge-patch+json" => {
            let patch: Value = serde_json::from_slice(body)
                .map_err(|e| ApiError::invalid(&format!("invalid patch: {e}")))?;
            if let Some(sp) = patch.get("status") {
                strategic_merge(&mut existing["status"], sp);
            }
        }
        _ => {
            let patch: Value = serde_json::from_slice(body)
                .map_err(|e| ApiError::invalid(&format!("invalid patch: {e}")))?;
            if let Some(sp) = patch.get("status") {
                merge_json(&mut existing["status"], sp);
            }
        }
    }
    Ok(())
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

    // `uid` and `creationTimestamp` are the server's: assigned when the client
    // left them absent, empty or null. **Present is not the same as set.** A
    // protobuf body decodes to JSON with every field at its zero value, so a
    // client-go create arrives with `"uid": ""` and `"creationTimestamp": null`
    // — and checking only for the key stored objects with no uid. `oc create
    // deployment` is such a client: its Deployment had uid "", the deployment
    // controller copied that into its ReplicaSet's ownerReference, and the
    // garbage collector, which ignores empty uids when collecting live owners,
    // deleted the ReplicaSet as orphaned on every pass (#99, found by #69).
    let unset = |v: Option<&Value>| v.and_then(Value::as_str).map_or(true, str::is_empty);
    if unset(meta.get("uid")) {
        meta.insert("uid".into(), Value::String(uuid::Uuid::new_v4().to_string()));
    }
    if unset(meta.get("creationTimestamp")) {
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
    fn strategic_merge_conditions_by_type() {
        // #47: a strategic-merge patch adding NetworkUnavailable must UPSERT by
        // `type`, preserving the existing conditions (not replace the whole list).
        let mut status = json!({"conditions": [
            {"type": "Ready", "status": "True"},
            {"type": "MemoryPressure", "status": "False"},
        ]});
        let patch = json!({"conditions": [
            {"type": "NetworkUnavailable", "status": "False", "reason": "CiliumIsUp"},
            {"type": "Ready", "status": "True", "reason": "KubeletReady"},
        ]});
        strategic_merge(&mut status, &patch);
        let conds = status["conditions"].as_array().unwrap();
        let by_type: std::collections::HashMap<_, _> = conds
            .iter()
            .map(|c| (c["type"].as_str().unwrap(), c))
            .collect();
        assert_eq!(by_type.len(), 3, "Ready, MemoryPressure preserved + NetworkUnavailable added");
        assert_eq!(by_type["MemoryPressure"]["status"], "False");
        assert_eq!(by_type["NetworkUnavailable"]["reason"], "CiliumIsUp");
        // existing Ready entry merged in place (reason added), not duplicated.
        assert_eq!(by_type["Ready"]["reason"], "KubeletReady");
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

    /// Absent and empty are the same thing to a controller.
    ///
    /// Go marshals a nil slice as `null` and an allocated-but-empty one as
    /// `[]`; which one a client sends is an accident of how it built the
    /// struct. cilium-operator CAS-guards the node taint list either way, and
    /// only the `null` spelling used to hold — so the empty-list one failed
    /// against a node that simply has no taints yet.
    #[test]
    fn json_patch_test_empty_list_on_absent_path_also_holds() {
        for guard in [r#"[]"#, r#"null"#, r#"{}"#] {
            let mut node = json!({"spec": {"podCIDR": "10.244.0.0/24"}});
            let body = format!(
                r#"[{{"op":"test","path":"/spec/taints","value":{guard}}},
                    {{"op":"add","path":"/spec/taints","value":[{{"key":"node.cilium.io/agent-not-ready","effect":"NoSchedule"}}]}}]"#
            );
            apply_patch_body(&mut node, "application/json-patch+json", body.as_bytes())
                .unwrap_or_else(|e| panic!("guard {guard} should hold: {e:?}"));
            assert_eq!(node["spec"]["taints"][0]["key"], "node.cilium.io/agent-not-ready");
        }

        // A non-empty test value against an absent path must still fail — that
        // is a genuine CAS miss, not a spelling difference.
        let mut n = json!({"spec": {}});
        assert!(apply_patch_body(
            &mut n,
            "application/json-patch+json",
            br#"[{"op":"test","path":"/spec/taints","value":[{"key":"x"}]}]"#,
        )
        .is_err());
    }

    /// `replace` on an absent member whose parent exists behaves as `add`,
    /// which is the leniency kube-apiserver has and controllers rely on
    /// without knowing it — on a real cluster the field usually exists.
    #[test]
    fn json_patch_replace_on_an_absent_member_adds_it() {
        let mut node = json!({"spec": {}});
        apply_patch_body(
            &mut node,
            "application/json-patch+json",
            br#"[{"op":"replace","path":"/spec/taints","value":[{"key":"k"}]}]"#,
        )
        .expect("replace on an absent member should add it");
        assert_eq!(node["spec"]["taints"][0]["key"], "k");

        // But not when the parent is missing too: inventing two levels would
        // hide a real mistake.
        let mut empty = json!({});
        assert!(apply_patch_body(
            &mut empty,
            "application/json-patch+json",
            br#"[{"op":"replace","path":"/spec/taints","value":[]}]"#,
        )
        .is_err());
    }

    /// A rejected patch quotes itself. "operation '/0' failed at path
    /// '/spec/taints'" names an index into a document the reader does not
    /// have.
    #[test]
    fn a_rejected_patch_says_what_the_patch_was() {
        let mut obj = json!({"spec": {"taints": [{"key": "real"}]}});
        let err = apply_patch_body(
            &mut obj,
            "application/json-patch+json",
            br#"[{"op":"test","path":"/spec/taints","value":[{"key":"other"}]}]"#,
        )
        .unwrap_err();
        assert!(err.message.contains("patch was:"), "{}", err.message);
        assert!(err.message.contains("/spec/taints"), "{}", err.message);
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

    /// What a protobuf create decodes to: every metadata field present at
    /// its zero value. The server's fields must still be the server's (#99).
    #[test]
    fn a_zero_valued_uid_and_timestamp_are_assigned() {
        let mut v = json!({"metadata": {"name": "web", "uid": "", "creationTimestamp": null,
                                        "generation": 0, "selfLink": ""}});
        ensure_metadata(&mut v, "web", Some("demo"));
        assert!(!v["metadata"]["uid"].as_str().unwrap().is_empty());
        assert!(!v["metadata"]["creationTimestamp"].as_str().unwrap().is_empty());

        // A uid the server already gave (a stored object re-created by the
        // manifest applier, say) is kept.
        let mut v = json!({"metadata": {"name": "web", "uid": "u1",
                                        "creationTimestamp": "2026-01-01T00:00:00Z"}});
        ensure_metadata(&mut v, "web", None);
        assert_eq!(v["metadata"]["uid"], "u1");
        assert_eq!(v["metadata"]["creationTimestamp"], "2026-01-01T00:00:00Z");
    }

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

#[cfg(test)]
mod patch_precondition_tests {
    use super::patch_precondition;

    const MERGE: &str = "application/merge-patch+json";

    #[test]
    fn a_merge_patch_without_a_resource_version_is_unconditional() {
        // #77: the common case, and the one that was 409ing under a race.
        assert_eq!(patch_precondition(MERGE, br#"{"metadata":{"labels":{"t":"v"}}}"#), None);
        assert_eq!(patch_precondition(MERGE, br#"{"status":{"x":1}}"#), None);
    }

    #[test]
    fn a_resource_version_in_the_body_is_the_precondition() {
        let body = br#"{"metadata":{"resourceVersion":"42"},"spec":{"online":true}}"#;
        assert_eq!(patch_precondition(MERGE, body).as_deref(), Some("42"));
        assert_eq!(
            patch_precondition("application/strategic-merge-patch+json; charset=utf-8", body)
                .as_deref(),
            Some("42")
        );
    }

    #[test]
    fn server_side_apply_reads_its_yaml_body() {
        let body = b"apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: x\n  resourceVersion: \"7\"\n";
        assert_eq!(
            patch_precondition("application/apply-patch+yaml", body).as_deref(),
            Some("7")
        );
        let body = b"apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: x\n";
        assert_eq!(patch_precondition("application/apply-patch+yaml", body), None);
    }

    #[test]
    fn a_json_patch_states_its_conditions_as_tests() {
        let body = br#"[{"op":"test","path":"/metadata/resourceVersion","value":"3"}]"#;
        assert_eq!(patch_precondition("application/json-patch+json", body), None);
    }

    #[test]
    fn retry_pauses_stay_inside_a_doubling_window_capped_at_200ms() {
        use super::patch_retry_pause;
        for (attempt, window) in [(1, 10), (2, 20), (3, 40), (4, 80), (5, 160), (6, 200), (40, 200)] {
            for _ in 0..200 {
                assert!(patch_retry_pause(attempt).as_millis() < window, "attempt {attempt}");
            }
        }
    }

    #[test]
    fn an_empty_resource_version_is_no_precondition() {
        assert_eq!(patch_precondition(MERGE, br#"{"metadata":{"resourceVersion":""}}"#), None);
    }
}

/// PUT of `/status` is conditional on the body's resourceVersion (#78), in
/// every handler that serves one.
#[cfg(test)]
mod status_put_tests {
    use super::*;
    use crate::crd::CrdRegistry;
    use crate::test_store::MemStore;
    use std::future::Future;
    use std::sync::Arc;

    fn state() -> AppState {
        AppState {
            storage: Arc::new(ResourceStorage::new(Arc::new(MemStore::default()))),
            crd_registry: Arc::new(CrdRegistry::new()),
            service_cidr: "10.96.0.0/12".into(),
        }
    }

    fn body(rv: Option<&str>, phase: &str) -> Value {
        let mut b = json!({
            "metadata": {},
            // A status PUT carries the whole object; nothing but status may land.
            "spec": { "x": 999 },
            "status": { "phase": phase },
        });
        if let Some(rv) = rv {
            b["metadata"]["resourceVersion"] = json!(rv);
        }
        b
    }

    /// One status writer is one version behind another; then the one that is
    /// current; then one that names no version at all.
    async fn scenario<F, Fut>(state: &AppState, key: &str, put: F)
    where
        F: Fn(Value) -> Fut,
        Fut: Future<Output = Result<(), ApiError>>,
    {
        let seed = json!({
            "metadata": { "name": "o" }, "spec": { "x": 1 }, "status": { "phase": "A" },
        });
        let v1 = state.storage.create(key, seed).await.unwrap();
        let rv1 = v1["metadata"]["resourceVersion"].as_str().unwrap().to_string();

        // Another writer moves the status on.
        let mut other = v1.clone();
        other["status"]["phase"] = json!("B");
        let v2 = state.storage.update(key, other, rv1.parse().ok()).await.unwrap();
        let rv2 = v2["metadata"]["resourceVersion"].as_str().unwrap().to_string();

        // Stale: 409, and the newer status survives.
        let err = put(body(Some(&rv1), "stale")).await.expect_err("a stale status PUT must 409");
        assert_eq!(err.status, StatusCode::CONFLICT, "{key}: {}", err.message);
        let stored = state.storage.get(key).await.unwrap();
        assert_eq!(stored["status"]["phase"], "B", "{key}: the stale write landed");

        // Current: written, and only status.
        put(body(Some(&rv2), "C")).await.unwrap();
        let stored = state.storage.get(key).await.unwrap();
        assert_eq!(stored["status"]["phase"], "C", "{key}");
        assert_eq!(stored["spec"]["x"], 1, "{key}: a status PUT changed spec");

        // No resourceVersion, or an empty one: unconditional.
        put(body(None, "D")).await.unwrap();
        put(body(Some(""), "E")).await.unwrap();
        let stored = state.storage.get(key).await.unwrap();
        assert_eq!(stored["status"]["phase"], "E", "{key}");
    }

    /// PUT keeps what the server owns, refuses a rename, and 404s a missing
    /// object rather than creating it (#67).
    #[tokio::test]
    async fn put_keeps_server_fields_and_does_not_create() {
        let s = state();
        let key = ResourceStorage::namespaced_key("configmaps", "default", "c1");
        let stored = s.storage.create(&key, json!({
            "metadata": {"name": "c1", "namespace": "default", "uid": "u1",
                         "creationTimestamp": "2026-01-01T00:00:00Z"},
            "data": {"a": "1"}})).await.unwrap();
        let rv = stored["metadata"]["resourceVersion"].as_str().unwrap().to_string();

        // Zero-valued server fields, as a protobuf body decodes: kept.
        let body = json!({"metadata": {"name": "c1", "uid": "", "creationTimestamp": null,
                                       "resourceVersion": rv}, "data": {"a": "2"}});
        let out = put_object(&s, &key, "c1", Some("default"), body).await.unwrap();
        assert_eq!(out["metadata"]["uid"], "u1");
        assert_eq!(out["metadata"]["creationTimestamp"], "2026-01-01T00:00:00Z");
        assert_eq!(out["data"]["a"], "2");

        let rename = json!({"metadata": {"name": "other"}, "data": {}});
        let err = put_object(&s, &key, "c1", Some("default"), rename).await.unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);

        let other_uid = json!({"metadata": {"name": "c1", "uid": "u2"}, "data": {}});
        let err = put_object(&s, &key, "c1", Some("default"), other_uid).await.unwrap_err();
        assert_eq!(err.status, StatusCode::CONFLICT);

        let missing = ResourceStorage::namespaced_key("configmaps", "default", "nope");
        let err = put_object(&s, &missing, "nope", Some("default"), json!({"metadata": {"name": "nope"}}))
            .await.unwrap_err();
        assert_eq!(err.status, StatusCode::NOT_FOUND);
    }

    /// A patch that nulls creationTimestamp — the conformance suite's own
    /// ConfigMap patch does — leaves it as stored.
    #[tokio::test]
    async fn a_patch_cannot_remove_the_creation_timestamp() {
        let s = state();
        let key = ResourceStorage::namespaced_key("configmaps", "default", "c1");
        s.storage.create(&key, json!({
            "metadata": {"name": "c1", "namespace": "default", "uid": "u1",
                         "creationTimestamp": "2026-01-01T00:00:00Z"}})).await.unwrap();
        let body = br#"{"metadata":{"creationTimestamp":null,"labels":{"x":"y"}}}"#;
        let out = guaranteed_patch(&s, &key, "application/strategic-merge-patch+json", body, |mut o| {
            apply_patch_body(&mut o, "application/strategic-merge-patch+json", body)?;
            Ok(o)
        }).await.unwrap();
        assert_eq!(out["metadata"]["creationTimestamp"], "2026-01-01T00:00:00Z");
        assert_eq!(out["metadata"]["uid"], "u1");
        assert_eq!(out["metadata"]["labels"]["x"], "y");
    }

    #[tokio::test]
    async fn core_cluster_scoped() {
        let s = state();
        let key = ResourceStorage::cluster_key("nodes", "o");
        scenario(&s, &key, |b| {
            let s = s.clone();
            async move {
                update_cluster_status(State(s), Path(("nodes".into(), "o".into())), Json(b))
                    .await
                    .map(|_| ())
            }
        })
        .await;
    }

    #[tokio::test]
    async fn core_namespaced() {
        let s = state();
        let key = ResourceStorage::namespaced_key("pods", "default", "o");
        scenario(&s, &key, |b| {
            let s = s.clone();
            async move {
                update_namespaced_status(
                    State(s),
                    Path(("default".into(), "pods".into(), "o".into())),
                    Json(b),
                )
                .await
                .map(|_| ())
            }
        })
        .await;
    }

    async fn register(s: &AppState, plural: &str, scope: &str) {
        s.crd_registry
            .register(&json!({
                "spec": {
                    "group": "demo.io", "scope": scope,
                    "names": { "plural": plural, "kind": "Thing" },
                    "versions": [{ "name": "v1", "served": true, "storage": true }],
                },
            }))
            .await;
    }

    #[tokio::test]
    async fn custom_resource_namespaced() {
        let s = state();
        register(&s, "widgets", "Namespaced").await;
        let key = ResourceStorage::namespaced_key(
            &ResourceStorage::custom_resource("demo.io", "widgets"),
            "default",
            "o",
        );
        scenario(&s, &key, |b| {
            let s = s.clone();
            async move {
                crate::crd::crd_update_status_ns(
                    State(s),
                    Path((
                        "demo.io".into(), "v1".into(), "default".into(), "widgets".into(), "o".into(),
                    )),
                    Json(b),
                )
                .await
                .map(|_| ())
            }
        })
        .await;
    }

    #[tokio::test]
    async fn custom_resource_cluster_scoped() {
        let s = state();
        register(&s, "gadgets", "Cluster").await;
        let key = ResourceStorage::cluster_key(
            &ResourceStorage::custom_resource("demo.io", "gadgets"),
            "o",
        );
        scenario(&s, &key, |b| {
            let s = s.clone();
            async move {
                crate::crd::crd_update_status_cluster(
                    State(s),
                    Path(("demo.io".into(), "v1".into(), "gadgets".into(), "o".into())),
                    Json(b),
                )
                .await
                .map(|_| ())
            }
        })
        .await;
    }
}
