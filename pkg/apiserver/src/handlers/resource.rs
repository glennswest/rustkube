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
    let (initial, live_rev) = if params.wants_initial_state() {
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
            initial_end_bookmark: params.send_initial_events,
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
    let page = state
        .storage
        .list_page(&prefix, limit, params.continue_token.as_deref())
        .await?;
    let (items, continue_token, revision) = (page.items, page.continue_token, page.revision);
    // Upstream leaves it out when a selector filtered the page.
    let remaining = page.remaining.filter(|_| params.label_selector.is_none() && params.field_selector.is_none());

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
    if let Some(n) = remaining {
        list["metadata"]["remainingItemCount"] = json!(n);
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
    let page = state
        .storage
        .list_page(&prefix, limit, params.continue_token.as_deref())
        .await?;
    let (items, continue_token, revision) = (page.items, page.continue_token, page.revision);
    // Upstream leaves it out when a selector filtered the page.
    let remaining = page.remaining.filter(|_| params.label_selector.is_none() && params.field_selector.is_none());

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
    if let Some(n) = remaining {
        list["metadata"]["remainingItemCount"] = json!(n);
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
    let page = state
        .storage
        .list_page(&prefix, limit, params.continue_token.as_deref())
        .await?;
    let (items, continue_token, revision) = (page.items, page.continue_token, page.revision);
    // Upstream leaves it out when a selector filtered the page.
    let remaining = page.remaining.filter(|_| params.label_selector.is_none() && params.field_selector.is_none());

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
    if let Some(n) = remaining {
        list["metadata"]["remainingItemCount"] = json!(n);
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

/// The name a create stores the object under.
///
/// `metadata.name` when it is set; otherwise `metadata.generateName` plus five
/// random characters from upstream's alphabet, written back into the body.
/// A protobuf create always carries `"name": ""`, and an empty name used to be
/// taken as given — the object was keyed `…/` and the second such create was
/// `"" already exists` (#67: the CSR API conformance spec, and everything else
/// the framework creates by generateName).
pub(crate) fn object_name(body: &mut Value) -> Result<String, ApiError> {
    if let Some(n) = body["metadata"]["name"].as_str().filter(|n| !n.is_empty()) {
        return Ok(n.to_string());
    }
    let prefix = body["metadata"]["generateName"]
        .as_str()
        .filter(|g| !g.is_empty())
        .ok_or_else(|| ApiError::invalid("metadata.name or metadata.generateName is required"))?
        .to_string();
    const ALPHABET: &[u8] = b"bcdfghjklmnpqrstvwxz2456789";
    let suffix: String = uuid::Uuid::new_v4()
        .as_bytes()
        .iter()
        .take(5)
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect();
    let name = format!("{prefix}{suffix}");
    if !body["metadata"].is_object() {
        body["metadata"] = json!({});
    }
    body["metadata"]["name"] = json!(name);
    Ok(name)
}

/// POST — create a cluster-scoped resource.
pub async fn create_cluster_resource(
    State(state): State<AppState>,
    Path(resource): Path<String>,
    Json(mut body): Json<Value>,
) -> Result<impl IntoResponse, ApiError> {
    let name = object_name(&mut body)?;

    ensure_metadata(&mut body, &name, None);

    crate::builtin_admission::admit_create(
        &state.storage, &resource, None, &mut body, &state.service_cidr,
    )
    .await?;
    crate::admission::admit(&state, crate::admission::Operation::Create, Some(&mut body), None).await?;
    ensure_metadata(&mut body, &name, None);

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
    let name = object_name(&mut body)?;

    check_body_namespace(&body, &namespace)?;
    ensure_metadata(&mut body, &name, Some(&namespace));

    // Built-in admission (NamespaceLifecycle, ServiceAccount, DefaultTolerationSeconds).
    crate::builtin_admission::admit_create(
        &state.storage, &resource, Some(&namespace), &mut body, &state.service_cidr,
    )
        .await?;
    // Admission webhooks (#82); a patch cannot move the object.
    crate::admission::admit(&state, crate::admission::Operation::Create, Some(&mut body), None).await?;
    ensure_metadata(&mut body, &name, Some(&namespace));

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

/// An immutable ConfigMap or Secret (`immutable: true`) keeps its data, and
/// stays immutable: an update that changes either is a 422, as upstream.
/// Such updates were written (#67: the ConfigMap and Secret immutability
/// conformance specs).
///
/// A PriorityClass's `value` and `preemptionPolicy` never change after
/// create, as upstream's `ValidatePriorityClassUpdate`: pods already carry the
/// value they were admitted with, so a changed class would say one priority
/// while its pods ran at another. They were writable (#67: the PriorityClass
/// endpoints conformance spec, reached once #85 served the kind).
pub(crate) fn check_immutable(key: &str, old: &Value, new: &Value) -> Result<(), ApiError> {
    if key.starts_with("/registry/priorityclasses/") {
        if old["value"] != new["value"] {
            return Err(ApiError::invalid("value: Forbidden: may not be changed in an update."));
        }
        // Not defaulted on create here, so absent is upstream's default: a
        // client that sends the defaulted value back is not changing it.
        let policy = |v: &Value| v["preemptionPolicy"].as_str().unwrap_or("PreemptLowerPriority").to_string();
        if policy(old) != policy(new) {
            return Err(ApiError::invalid("preemptionPolicy: Invalid value: field is immutable"));
        }
        return Ok(());
    }
    let fields: &[&str] = if key.starts_with("/registry/configmaps/") {
        &["data", "binaryData"]
    } else if key.starts_with("/registry/secrets/") {
        &["data", "stringData"]
    } else {
        return Ok(());
    };
    if old["immutable"].as_bool() != Some(true) {
        return Ok(());
    }
    if new["immutable"].as_bool() != Some(true) {
        return Err(ApiError::invalid("immutable: Forbidden: field is immutable when `immutable` is set"));
    }
    // An absent map and an empty one are the same data.
    let same = |f: &str| {
        let empty = |v: &Value| v.is_null() || v.as_object().is_some_and(|m| m.is_empty());
        old[f] == new[f] || (empty(&old[f]) && empty(&new[f]))
    };
    if !fields.iter().all(|f| same(f)) {
        return Err(ApiError::invalid("data: Forbidden: field is immutable when `immutable` is set"));
    }
    Ok(())
}

/// PUT — update a cluster-scoped resource.
pub async fn update_cluster_resource(
    State(state): State<AppState>,
    Path((resource, name)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<impl IntoResponse, ApiError> {
    let key = ResourceStorage::cluster_key(&resource, &name);
    let obj = put_object(&state, &key, &name, None, body, StatusField::Writable).await?;
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
    status: StatusField,
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
    status.on_update(&mut body, &existing);
    check_immutable(key, &existing, &body)?;
    if key.starts_with("/registry/persistentvolumeclaims/") {
        crate::builtin_admission::pvc_update(&state.storage, &existing, &body).await?;
    }
    crate::admission::admit(state, crate::admission::Operation::Update, Some(&mut body), Some(&existing)).await?;
    keep_server_fields(&mut body, &existing, name, namespace);
    status.on_update(&mut body, &existing);
    persist_or_finalize(state, key, body).await
}

/// What a write through the main resource may do to `status` (#128).
///
/// A custom resource whose CRD enables the `status` subresource keeps status
/// for `/status`: upstream's strategy drops the status a create submits and
/// carries the stored status over an update, so a spec writer (a user's
/// `kubectl apply`, a GitOps sync) cannot replace what the controller
/// reported. Every other object is `Writable`, as it was.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum StatusField {
    Writable,
    Kept,
}

impl StatusField {
    /// A create through the main resource.
    pub fn on_create(self, obj: &mut Value) {
        if self == StatusField::Kept {
            if let Some(o) = obj.as_object_mut() {
                o.remove("status");
            }
        }
    }

    /// An update through the main resource: `stored` is what is there now.
    pub fn on_update(self, obj: &mut Value, stored: &Value) {
        if self != StatusField::Kept {
            return;
        }
        if let Some(o) = obj.as_object_mut() {
            match stored.get("status") {
                Some(s) => {
                    o.insert("status".into(), s.clone());
                }
                None => {
                    o.remove("status");
                }
            }
        }
    }
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
    let obj = put_object(&state, &key, &name, Some(&namespace), body, StatusField::Writable).await?;
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

/// Parse DeleteOptions from a request body (JSON — protobuf is transcoded
/// upstream) and the query string. An empty/absent body yields defaults
/// (Background, no preconditions).
///
/// The query carries the same options (`dryRun`, `propagationPolicy`,
/// `gracePeriodSeconds`, `orphanDependents`), as upstream decodes them when
/// there is no body — `kubectl delete --dry-run=server` and plain-HTTP
/// clients send them that way. The body wins where both say something.
/// Without this a `?dryRun=All` delete deleted (#96: the test container's
/// medium suite).
pub(crate) fn parse_delete_options(body: &[u8], query: Option<&str>) -> DeleteOptions {
    let mut opts = parse_delete_body(body);
    for (k, v) in form_urlencoded::parse(query.unwrap_or("").as_bytes()) {
        match k.as_ref() {
            "dryRun" if v == "All" => opts.dry_run = true,
            "propagationPolicy" if opts.propagation_policy.is_none() => opts.propagation_policy = Some(v.into_owned()),
            "orphanDependents" if opts.propagation_policy.is_none() && v == "true" => {
                opts.propagation_policy = Some("Orphan".into())
            }
            "gracePeriodSeconds" if opts.grace_period_seconds.is_none() => opts.grace_period_seconds = v.parse().ok(),
            _ => {}
        }
    }
    opts
}

fn parse_delete_body(body: &[u8]) -> DeleteOptions {
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
    obj: Value,
    opts: &DeleteOptions,
    name: &str,
    namespace: Option<&str>,
    kind: &str,
) -> Result<Value, ApiError> {
    check_preconditions(&obj, opts)?;
    crate::admission::admit(state, crate::admission::Operation::Delete, None, Some(&obj)).await?;

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
        //
        // Written like `GuaranteedUpdate`: a controller writing the object
        // between the read and this write (the deployment controller's status
        // update, say) is re-read and the marks applied again, not a 409 —
        // only a `resourceVersion` precondition makes a stale delete fail
        // (#67: the GC's orphan-propagation conformance spec lost that race).
        let implied = match opts.propagation_policy.as_deref() {
            Some("Foreground") => Some("foregroundDeletion"),
            Some("Orphan") => Some("orphan"),
            _ => None,
        };
        let grace = opts.grace_period_seconds;
        return guaranteed_update(state, key, opts.precondition_rv.clone(), move |mut fresh| {
            let mut fins: Vec<Value> =
                fresh["metadata"]["finalizers"].as_array().cloned().unwrap_or_default();
            if let Some(f) = implied {
                ensure_finalizer(&mut fins, f);
            }
            if fresh["metadata"]["deletionTimestamp"].is_null() {
                fresh["metadata"]["deletionTimestamp"] =
                    json!(chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string());
            }
            if let Some(g) = grace {
                fresh["metadata"]["deletionGracePeriodSeconds"] = json!(g);
            }
            fresh["metadata"]["finalizers"] = json!(fins);
            Ok(fresh)
        })
        .await;
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
    RawQuery(query): RawQuery,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let key = ResourceStorage::cluster_key(&resource, &name);
    // Get the object first so we can return it (and inspect it for namespaces).
    let obj = state.storage.get(&key).await?;
    let opts = parse_delete_options(&body, query.as_deref());
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
    crate::admission::admit(state, crate::admission::Operation::Delete, None, Some(&obj)).await?;
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
    RawQuery(query): RawQuery,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let key = ResourceStorage::namespaced_key(&resource, &namespace, &name);
    let obj = state.storage.get(&key).await?;
    let opts = parse_delete_options(&body, query.as_deref());
    let out = perform_delete(&state, &key, obj, &opts, &name, Some(&namespace), &resource).await?;
    Ok(Json(out))
}

/// What a `deletecollection` removed: the deleted objects, and for each
/// whether it is gone (as opposed to left terminating on its finalizers).
pub(crate) struct DeletedCollection {
    pub items: Vec<Value>,
    pub removed: Vec<(String, bool)>,
}

/// `deletecollection` — DELETE on a collection path, as `kubectl delete
/// --all`, client-go's `DeleteCollection` and many conformance specs send it.
///
/// Every object under `prefix` that the request's label and field selectors
/// match is deleted as a single DELETE would delete it (DeleteOptions,
/// finalizers, propagation); one that is already gone is skipped. Namespaces
/// terminate gracefully. `store_resource` is where the objects are keyed —
/// the plural, or `{group}/{plural}` for a custom resource.
pub(crate) async fn delete_collection(
    state: &AppState,
    prefix: &str,
    store_resource: &str,
    kind: &str,
    query: Option<&str>,
    body: &[u8],
) -> Result<DeletedCollection, ApiError> {
    let params = WatchParams::from_query(query.unwrap_or(""));
    let opts = parse_delete_options(body, query);
    let cluster_path = prefix == ResourceStorage::cluster_prefix(store_resource);
    let mut matched = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let (items, next, _) = state.storage.list(prefix, 500, token.as_deref()).await?;
        matched.extend(selector::filter_objects(items, &params.label_selector, &params.field_selector));
        match next {
            Some(t) => token = Some(t),
            None => break,
        }
    }
    delete_listed(state, matched, cluster_path, store_resource, kind, &opts).await
}

/// The half of `delete_collection` after the list: refuse a namespaced
/// resource on its cluster path, then delete each object.
async fn delete_listed(
    state: &AppState,
    matched: Vec<Value>,
    cluster_path: bool,
    store_resource: &str,
    kind: &str,
    opts: &DeleteOptions,
) -> Result<DeletedCollection, ApiError> {
    let namespaced = |o: &Value| o["metadata"]["namespace"].as_str().map_or(false, |n| !n.is_empty());
    if cluster_path && matched.iter().any(namespaced) {
        return Err(refuse_namespaced(store_resource));
    }
    delete_each(state, matched, store_resource, kind, opts).await
}

/// The cluster-scoped path of a *namespaced* resource (`/api/v1/pods`) lists
/// across namespaces but has no deletecollection upstream — deleting every
/// pod in the cluster is not one request. Refused before anything is deleted.
fn refuse_namespaced(resource: &str) -> ApiError {
    ApiError {
        status: StatusCode::METHOD_NOT_ALLOWED,
        reason: "MethodNotAllowed".into(),
        message: format!("the server does not allow this method on the requested resource (deletecollection {resource} across namespaces)"),
    }
}

async fn delete_each(
    state: &AppState,
    matched: Vec<Value>,
    store_resource: &str,
    kind: &str,
    opts: &DeleteOptions,
) -> Result<DeletedCollection, ApiError> {
    let mut out = DeletedCollection { items: Vec::new(), removed: Vec::new() };
    for obj in matched {
        let Some(name) = obj["metadata"]["name"].as_str().map(str::to_string) else {
            continue;
        };
        let namespace = obj["metadata"]["namespace"]
            .as_str()
            .filter(|n| !n.is_empty())
            .map(str::to_string);
        let key = match &namespace {
            Some(ns) => ResourceStorage::namespaced_key(store_resource, ns, &name),
            None => ResourceStorage::cluster_key(store_resource, &name),
        };
        let result = if store_resource == "namespaces" {
            terminate_namespace(state, &name, obj.clone(), opts).await
        } else {
            perform_delete(state, &key, obj.clone(), opts, &name, namespace.as_deref(), kind).await
        };
        match result {
            Ok(r) => {
                out.removed.push((name, r["kind"] == "Status"));
                out.items.push(if r["kind"] == "Status" { obj } else { r });
            }
            // Deleted by someone else between the list and now.
            Err(e) if e.status == StatusCode::NOT_FOUND => {}
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

fn deleted_list(api_version: &str, list_kind: String, items: Vec<Value>) -> Value {
    json!({ "apiVersion": api_version, "kind": list_kind, "metadata": {}, "items": items })
}

/// DELETE a collection of a cluster-scoped resource (`deletecollection`).
pub async fn delete_cluster_collection(
    State(state): State<AppState>,
    Path(resource): Path<String>,
    RawQuery(query): RawQuery,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let prefix = ResourceStorage::cluster_prefix(&resource);
    let done = delete_collection(&state, &prefix, &resource, &resource, query.as_deref(), &body).await?;
    Ok(Json(deleted_list(resource_to_api_version(&resource), resource_to_list_kind(&resource), done.items)))
}

/// DELETE a collection of a namespaced resource in one namespace.
pub async fn delete_namespaced_collection(
    State(state): State<AppState>,
    Path((namespace, resource)): Path<(String, String)>,
    RawQuery(query): RawQuery,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let prefix = ResourceStorage::namespace_prefix(&resource, &namespace);
    let done = delete_collection(&state, &prefix, &resource, &resource, query.as_deref(), &body).await?;
    Ok(Json(deleted_list(resource_to_api_version(&resource), resource_to_list_kind(&resource), done.items)))
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
        status_write_metadata(&mut existing, body, false);
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
        let before = fresh.clone();
        let mut obj = mutate(fresh)?;
        check_immutable(key, &before, &obj)?;
        if key.starts_with("/registry/persistentvolumeclaims/") {
            crate::builtin_admission::pvc_update(&state.storage, &before, &obj).await?;
        }
        if !obj["metadata"].is_object() {
            return Err(ApiError::invalid("metadata must be an object"));
        }
        // A patch cannot rename the object or rewrite what the server owns —
        // the conformance suite's own ConfigMap patch sends
        // `creationTimestamp: null`, which used to delete it (#67).
        keep_server_fields(&mut obj, &stored_meta, &name, namespace.as_deref());
        // Admission webhooks (#82) for a PATCH and every `/status` write;
        // again each attempt, as upstream admits inside GuaranteedUpdate.
        crate::admission::admit(state, crate::admission::Operation::Update, Some(&mut obj), Some(&before)).await?;
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
    status: StatusField,
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
            let stored = existing.clone();
            let mut out = if !is_apply {
                let mut existing = existing;
                apply_patch_body(&mut existing, content_type, body)?;
                existing
            } else {
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
                    })?
            };
            status.on_update(&mut out, &stored);
            Ok(out)
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
                crate::admission::admit(state, crate::admission::Operation::Create, Some(&mut obj), None).await?;
                ensure_metadata(&mut obj, name, namespace);
                status.on_create(&mut obj);
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
    let obj = patch_stored_object(&state, &key, &resource, &name, None, &headers, q, &body, StatusField::Writable).await?;
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
        patch_stored_object(&state, &key, &resource, &name, Some(&namespace), &headers, q, &body, StatusField::Writable)
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

/// List fields upstream merges as a set of scalars (`patchStrategy:"merge"`
/// on a list of strings): a strategic patch adds its values, and removes the
/// ones named in `$deleteFromPrimitiveList/<field>`.
fn strategic_merge_set(field: &str) -> bool {
    field == "finalizers"
}

/// Strategic-merge-patch (schema-lite): recursively merge objects; merge list
/// fields that have a known `patchMergeKey` by upserting entries by that key
/// (preserving unmatched existing entries); overwrite scalars and unkeyed lists;
/// a `null` value deletes the key.
///
/// And the directives a patch made by client-go's `CreateTwoWayMergePatch`
/// carries (#63) — without them a removal was stored as data: an element
/// `{"type": "Resizing", "$patch": "delete"}` was merged *into* the Resizing
/// condition, and `$setElementOrder/conditions` became a field of the object:
///
/// - `"$patch": "delete"` on a list element removes the element with that
///   key; on a map, the map; `"$patch": "replace"` replaces the map, or (as a
///   list element) the list, with the rest of the patch;
/// - `"$retainKeys": [..]` drops the map's keys it does not name;
/// - `"$deleteFromPrimitiveList/<field>"` removes those values from a set
///   list (`finalizers`);
/// - `"$setElementOrder/<field>"` orders a merged list; it is a hint and is
///   never stored.
pub(crate) fn strategic_merge(target: &mut Value, patch: &Value) {
    let Value::Object(p) = patch else {
        *target = patch.clone();
        return;
    };
    if p.get("$patch").and_then(Value::as_str) == Some("replace") {
        *target = strip_directives(patch);
        return;
    }
    if !target.is_object() {
        *target = json!({});
    }
    let t = target.as_object_mut().unwrap();
    for (k, pv) in p {
        if k.starts_with('$') {
            continue; // directives, handled below
        }
        if pv.is_null() || pv.get("$patch").and_then(Value::as_str) == Some("delete") {
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
            Some(tv) if strategic_merge_set(k) && tv.is_array() && pv.is_array() => {
                let list = tv.as_array_mut().unwrap();
                for v in pv.as_array().unwrap() {
                    if !list.contains(v) {
                        list.push(v.clone());
                    }
                }
            }
            Some(tv) if tv.is_object() && pv.is_object() => strategic_merge(tv, pv),
            _ => {
                t.insert(k.clone(), strip_directives(pv));
            }
        }
    }
    for (k, pv) in p {
        if let Some(field) = k.strip_prefix("$deleteFromPrimitiveList/") {
            if let (Some(list), Some(gone)) = (t.get_mut(field).and_then(Value::as_array_mut), pv.as_array()) {
                list.retain(|v| !gone.contains(v));
            }
        } else if let Some(field) = k.strip_prefix("$setElementOrder/") {
            if let (Some(list), Some(order)) = (t.get_mut(field).and_then(Value::as_array_mut), pv.as_array()) {
                set_element_order(list, order, strategic_merge_key(field));
            }
        }
    }
    if let Some(keep) = p.get("$retainKeys").and_then(Value::as_array) {
        t.retain(|k, _| keep.iter().any(|x| x.as_str() == Some(k.as_str())));
    }
}

/// Order `list` as `order` names its elements (by merge key, or by value for
/// a list of scalars); elements `order` does not name keep their place after
/// the named ones, as upstream orders them.
fn set_element_order(list: &mut Vec<Value>, order: &[Value], key: Option<&str>) {
    let id = |v: &Value| -> Value {
        match key {
            Some(k) => v.get(k).cloned().unwrap_or(Value::Null),
            None => v.clone(),
        }
    };
    let rank = |v: &Value| -> usize {
        let i = id(v);
        order.iter().position(|o| id(o) == i).unwrap_or(order.len())
    };
    list.sort_by_key(|v| rank(v)); // stable: unnamed keep their order
}

/// A patch value stored as data: without the directive keys it may carry.
fn strip_directives(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .filter(|(k, _)| !k.starts_with('$'))
                .map(|(k, x)| (k.clone(), strip_directives(x)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(
            a.iter()
                .filter(|x| x.get("$patch").is_none() || x.as_object().is_some_and(|m| m.len() > 1))
                .map(strip_directives)
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Upsert each `patch` item into `target` by `key`: an item whose key matches an
/// existing entry is strategic-merged into it; a new key is appended. An item
/// `{"$patch": "delete", <key>: …}` removes that entry, and an item
/// `{"$patch": "replace"}` makes the list exactly the patch's other items.
fn strategic_merge_list(target: &mut Vec<Value>, patch: &[Value], key: &str) {
    let directive = |v: &Value| v.get("$patch").and_then(Value::as_str).map(str::to_string);
    if patch.iter().any(|v| directive(v).as_deref() == Some("replace")) {
        *target = patch.iter().filter(|v| directive(v).is_none()).map(strip_directives).collect();
        return;
    }
    for pitem in patch {
        let pkey = pitem.get(key);
        if directive(pitem).as_deref() == Some("delete") {
            if let Some(pk) = pkey {
                target.retain(|t| t.get(key) != Some(pk));
            }
            continue;
        }
        match pkey.and_then(|pk| target.iter_mut().find(|t| t.get(key) == Some(pk))) {
            Some(existing) => strategic_merge(existing, pitem),
            None => target.push(strip_directives(pitem)),
        }
    }
}

/// Apply a status subresource patch to `existing`, honoring the patch
/// Content-Type: strategic-merge (merge keyed lists like conditions), merge-patch
/// (RFC-7386, arrays replaced), or JSON patch.
/// What a status write may change besides `status`: labels and annotations,
/// as upstream's status strategies allow (they reset only the spec). A
/// status PATCH that also set an annotation lost the annotation (#67: the
/// CronJob API conformance spec).
fn status_write_metadata(existing: &mut Value, patch: &Value, merge: bool) {
    for f in ["labels", "annotations"] {
        let Some(new) = patch["metadata"].get(f) else { continue };
        if !existing["metadata"].is_object() {
            existing["metadata"] = json!({});
        }
        if merge {
            merge_json(&mut existing["metadata"][f], new);
        } else {
            existing["metadata"][f] = new.clone();
        }
    }
}

fn apply_status_patch(existing: &mut Value, content_type: &str, body: &[u8]) -> Result<(), ApiError> {
    match content_type.split(';').next().unwrap_or("").trim() {
        "application/json-patch+json" => {
            // Applied whole, then the spec put back: a status write never
            // changes it.
            let spec = existing.get("spec").cloned();
            let mut ops: Value = serde_json::from_slice(body)
                .map_err(|e| ApiError::invalid(&format!("invalid JSON Patch: {e}")))?;
            normalize_json_patch(existing, &mut ops);
            let patch: json_patch::Patch = serde_json::from_value(ops)
                .map_err(|e| ApiError::invalid(&format!("invalid JSON Patch: {e}")))?;
            json_patch::patch(existing, &patch)
                .map_err(|e| ApiError::invalid(&format!("JSON Patch could not be applied: {e}")))?;
            match spec {
                Some(s) => existing["spec"] = s,
                None => {
                    if let Some(o) = existing.as_object_mut() {
                        o.remove("spec");
                    }
                }
            }
        }
        "application/strategic-merge-patch+json" => {
            let patch: Value = serde_json::from_slice(body)
                .map_err(|e| ApiError::invalid(&format!("invalid patch: {e}")))?;
            if let Some(sp) = patch.get("status") {
                strategic_merge(&mut existing["status"], sp);
            }
            status_write_metadata(existing, &patch, true);
        }
        _ => {
            let patch: Value = serde_json::from_slice(body)
                .map_err(|e| ApiError::invalid(&format!("invalid patch: {e}")))?;
            if let Some(sp) = patch.get("status") {
                merge_json(&mut existing["status"], sp);
            }
            status_write_metadata(existing, &patch, true);
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

    // Same rule as uid below: a protobuf create carries `"namespace": ""`,
    // and an object stored with an empty namespace is returned with one —
    // client-go then GETs `/apis/apps/v1/deployments/{name}`, which is no
    // path at all (#67: every webhook spec's server Deployment). A namespace
    // that names a *different* one is refused by the caller
    // (`check_body_namespace`), not rewritten here.
    let empty = |v: Option<&Value>| v.and_then(Value::as_str).map_or(true, str::is_empty);
    if let Some(ns) = namespace {
        if empty(meta.get("namespace")) {
            meta.insert("namespace".into(), Value::String(ns.to_string()));
        }
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
    let unset = empty;
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

/// A create's body may leave `metadata.namespace` empty — it is taken from
/// the URL — but may not name another one: upstream refuses that with 400.
pub(crate) fn check_body_namespace(body: &Value, namespace: &str) -> Result<(), ApiError> {
    match body["metadata"]["namespace"].as_str().filter(|n| !n.is_empty()) {
        Some(ns) if ns != namespace => Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            reason: "BadRequest".into(),
            message: format!(
                "the namespace of the provided object does not match the namespace sent on the request ({ns} != {namespace})"
            ),
        }),
        _ => Ok(()),
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
        "podtemplates" => "PodTemplate",
        "replicationcontrollers" => "ReplicationController",
        "resourcequotas" => "ResourceQuota",
        "limitranges" => "LimitRange",
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
        "volumeattributesclasses" => "VolumeAttributesClass",
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
        | "csistoragecapacities" | "volumeattributesclasses" => "storage.k8s.io/v1",
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
    fn strategic_merge_directives_from_client_go() {
        // external-resizer moving a claim from Resizing to
        // FileSystemResizePending (#63), as CreateTwoWayMergePatch spells it.
        let mut pvc = json!({"status": {"conditions": [
            {"type": "Resizing", "status": "True"}, {"type": "Other", "status": "True"}]}});
        strategic_merge(&mut pvc, &json!({"status": {
            "$setElementOrder/conditions": [{"type": "Other"}, {"type": "FileSystemResizePending"}],
            "conditions": [{"type": "Resizing", "$patch": "delete"},
                           {"type": "FileSystemResizePending", "status": "True"}]}}));
        assert_eq!(pvc, json!({"status": {"conditions": [
            {"type": "Other", "status": "True"}, {"type": "FileSystemResizePending", "status": "True"}]}}));

        // A finalizer removed, one added: the set merges, nothing literal stored.
        let mut obj = json!({"metadata": {"finalizers": ["a", "b"]}});
        strategic_merge(&mut obj, &json!({"metadata": {
            "$deleteFromPrimitiveList/finalizers": ["a"],
            "$setElementOrder/finalizers": ["b", "c"], "finalizers": ["c"]}}));
        assert_eq!(obj, json!({"metadata": {"finalizers": ["b", "c"]}}));

        // $patch on maps, and $retainKeys.
        let mut obj = json!({"spec": {"a": {"x": 1, "y": 2}, "b": {"z": 1}, "strategy": {"type": "RollingUpdate", "rollingUpdate": {"maxSurge": 1}}}});
        strategic_merge(&mut obj, &json!({"spec": {
            "a": {"$patch": "replace", "w": 9}, "b": {"$patch": "delete"},
            "strategy": {"$retainKeys": ["type"], "type": "Recreate"}}}));
        assert_eq!(obj, json!({"spec": {"a": {"w": 9}, "strategy": {"type": "Recreate"}}}));

        // A list replaced wholesale.
        let mut obj = json!({"env": [{"name": "A"}, {"name": "B"}]});
        strategic_merge(&mut obj, &json!({"env": [{"$patch": "replace"}, {"name": "C"}]}));
        assert_eq!(obj, json!({"env": [{"name": "C"}]}));
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
            None,
        );
        assert_eq!(opts.grace_period_seconds, Some(30));
        assert_eq!(opts.propagation_policy.as_deref(), Some("Foreground"));
        assert!(opts.dry_run);

        let obj = json!({"metadata": {"uid": "abc", "resourceVersion": "42"}});
        assert!(check_preconditions(&obj, &opts).is_ok());
        // A uid mismatch is a Conflict.
        let bad = parse_delete_options(br#"{"preconditions":{"uid":"WRONG"}}"#, None);
        assert!(check_preconditions(&obj, &bad).is_err());
        // Empty body → defaults (Background, no preconditions).
        let empty = parse_delete_options(b"", None);
        assert!(empty.propagation_policy.is_none() && !empty.dry_run);
        assert!(check_preconditions(&obj, &empty).is_ok());
    }

    #[test]
    fn delete_options_from_the_query() {
        // No body: the query says it all (`kubectl delete --dry-run=server`).
        let q = parse_delete_options(b"", Some("dryRun=All&propagationPolicy=Foreground&gracePeriodSeconds=0"));
        assert!(q.dry_run);
        assert_eq!(q.propagation_policy.as_deref(), Some("Foreground"));
        assert_eq!(q.grace_period_seconds, Some(0));
        assert_eq!(parse_delete_options(b"", Some("orphanDependents=true")).propagation_policy.as_deref(), Some("Orphan"));
        // The body wins where both speak.
        let both = parse_delete_options(br#"{"propagationPolicy":"Background"}"#, Some("propagationPolicy=Orphan"));
        assert_eq!(both.propagation_policy.as_deref(), Some("Background"));
        assert!(!parse_delete_options(b"", Some("dryRun=")).dry_run);
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

    /// generateName is honoured when the name is absent or empty (#67).
    #[test]
    fn generate_name_makes_a_name() {
        let mut v = json!({"metadata": {"name": "", "generateName": "csr-"}});
        let n = object_name(&mut v).unwrap();
        assert!(n.starts_with("csr-") && n.len() == 9, "{n}");
        assert_eq!(v["metadata"]["name"], n.as_str());
        let mut w = json!({"metadata": {"generateName": "csr-"}});
        assert_ne!(object_name(&mut w).unwrap(), n);
        assert_eq!(object_name(&mut json!({"metadata": {"name": "x", "generateName": "y-"}})).unwrap(), "x");
        assert!(object_name(&mut json!({"metadata": {"name": ""}})).is_err());
    }

    /// A protobuf create's `"namespace": ""` is the URL's namespace (#67);
    /// a body naming another namespace is refused.
    #[test]
    fn an_empty_namespace_is_the_urls() {
        let mut v = json!({"metadata": {"name": "web", "namespace": ""}});
        ensure_metadata(&mut v, "web", Some("demo"));
        assert_eq!(v["metadata"]["namespace"], "demo");
        assert!(check_body_namespace(&json!({"metadata": {"namespace": ""}}), "demo").is_ok());
        assert!(check_body_namespace(&json!({"metadata": {}}), "demo").is_ok());
        assert!(check_body_namespace(&json!({"metadata": {"namespace": "demo"}}), "demo").is_ok());
        let err = check_body_namespace(&json!({"metadata": {"namespace": "other"}}), "demo").unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
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
            admission: Default::default(),
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

    /// deletecollection deletes what the selector matches in the namespace,
    /// leaves a finalized object terminating, and will not sweep a namespaced
    /// resource across namespaces (#67).
    #[tokio::test]
    async fn deletecollection_honours_selectors_and_scope() {
        let s = state();
        for (ns, name, app, fin) in [("a", "c1", "web", false), ("a", "c2", "web", true),
                                     ("a", "c3", "db", false), ("b", "c4", "web", false)] {
            let mut obj = json!({"metadata": {"name": name, "namespace": ns, "labels": {"app": app}}});
            if fin {
                obj["metadata"]["finalizers"] = json!(["x/keep"]);
            }
            s.storage.create(&ResourceStorage::namespaced_key("configmaps", ns, name), obj).await.unwrap();
        }
        // What the LIST would hand over: namespace `a`, filtered by `app=web`.
        // (MemStore does not list; the selector filter has its own tests.)
        let mut in_a = Vec::new();
        for n in ["c1", "c2", "c3"] {
            in_a.push(s.storage.get(&ResourceStorage::namespaced_key("configmaps", "a", n)).await.unwrap());
        }
        let matched = selector::filter_objects(in_a, &Some("app=web".into()), &None);
        let opts = DeleteOptions::default();
        let done = delete_listed(&s, matched, false, "configmaps", "configmaps", &opts).await.unwrap();
        assert_eq!(done.items.len(), 2);
        let get = |ns: &str, n: &str| {
            let (storage, key) = (s.storage.clone(), ResourceStorage::namespaced_key("configmaps", ns, n));
            async move { storage.get(&key).await }
        };
        assert!(get("a", "c1").await.is_err(), "c1 was not deleted");
        assert!(!get("a", "c2").await.unwrap()["metadata"]["deletionTimestamp"].is_null(), "c2 not terminating");
        assert!(get("a", "c3").await.is_ok(), "c3 did not match the selector");
        assert!(get("b", "c4").await.is_ok(), "c4 is in another namespace");

        let c4 = s.storage.get(&ResourceStorage::namespaced_key("configmaps", "b", "c4")).await.unwrap();
        let err = delete_listed(&s, vec![c4], true, "configmaps", "configmaps", &opts)
            .await
            .err()
            .expect("a namespaced resource's cluster path has no deletecollection");
        assert_eq!(err.status, StatusCode::METHOD_NOT_ALLOWED);
        assert!(get("b", "c4").await.is_ok());
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
        let out = put_object(&s, &key, "c1", Some("default"), body, StatusField::Writable).await.unwrap();
        assert_eq!(out["metadata"]["uid"], "u1");
        assert_eq!(out["metadata"]["creationTimestamp"], "2026-01-01T00:00:00Z");
        assert_eq!(out["data"]["a"], "2");

        let rename = json!({"metadata": {"name": "other"}, "data": {}});
        let err = put_object(&s, &key, "c1", Some("default"), rename, StatusField::Writable).await.unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);

        let other_uid = json!({"metadata": {"name": "c1", "uid": "u2"}, "data": {}});
        let err = put_object(&s, &key, "c1", Some("default"), other_uid, StatusField::Writable).await.unwrap_err();
        assert_eq!(err.status, StatusCode::CONFLICT);

        let missing = ResourceStorage::namespaced_key("configmaps", "default", "nope");
        let err = put_object(&s, &missing, "nope", Some("default"), json!({"metadata": {"name": "nope"}}), StatusField::Writable)
            .await.unwrap_err();
        assert_eq!(err.status, StatusCode::NOT_FOUND);
    }

    /// A custom resource whose CRD enables `/status` (#128): PUT, merge
    /// patch, JSON patch and server-side apply through the main resource
    /// change spec and leave the stored status; `Writable` objects keep
    /// whole-object semantics.
    #[tokio::test]
    async fn main_writes_keep_status_when_it_is_a_subresource() {
        let s = state();
        let key = ResourceStorage::namespaced_key("example.com/widgets", "default", "w1");
        s.storage.create(&key, json!({
            "apiVersion": "example.com/v1", "kind": "Widget",
            "metadata": {"name": "w1", "namespace": "default", "uid": "u1"},
            "spec": {"size": 1}, "status": {"phase": "Ready"}})).await.unwrap();
        let hdr = |ct: &str| {
            let mut h = axum::http::HeaderMap::new();
            h.insert(axum::http::header::CONTENT_TYPE, ct.parse().unwrap());
            h
        };

        let out = put_object(&s, &key, "w1", Some("default"), json!({
            "metadata": {"name": "w1"}, "spec": {"size": 2}, "status": {"phase": "Hacked"}}),
            StatusField::Kept).await.unwrap();
        assert_eq!((out["spec"]["size"].clone(), out["status"].clone()), (json!(2), json!({"phase": "Ready"})));
        // A PUT without status does not delete it either.
        let out = put_object(&s, &key, "w1", Some("default"), json!({"metadata": {"name": "w1"}, "spec": {"size": 3}}),
            StatusField::Kept).await.unwrap();
        assert_eq!(out["status"], json!({"phase": "Ready"}));

        for (ct, body) in [
            ("application/merge-patch+json", r#"{"spec":{"size":4},"status":{"phase":"Hacked"}}"#),
            ("application/json-patch+json", r#"[{"op":"replace","path":"/spec/size","value":5},{"op":"replace","path":"/status","value":{"phase":"Hacked"}}]"#),
            ("application/apply-patch+yaml", "apiVersion: example.com/v1\nkind: Widget\nmetadata: {name: w1}\nspec: {size: 6}\nstatus: {phase: Hacked}\n"),
        ] {
            let out = patch_stored_object(&s, &key, "widgets", "w1", Some("default"), &hdr(ct),
                "fieldManager=t&force=true", body.as_bytes(), StatusField::Kept).await.unwrap();
            assert_eq!(out["status"], json!({"phase": "Ready"}), "{ct}");
        }
        assert_eq!(s.storage.get(&key).await.unwrap()["spec"]["size"], 6);

        // Server-side apply creating a missing object: its status is dropped.
        // (Creating needs its namespace, as NamespaceLifecycle checks.)
        s.storage.create(&ResourceStorage::cluster_key("namespaces", "default"),
            json!({"metadata": {"name": "default"}, "status": {"phase": "Active"}})).await.unwrap();
        let key2 = ResourceStorage::namespaced_key("example.com/widgets", "default", "w2");
        let out = patch_stored_object(&s, &key2, "widgets", "w2", Some("default"), &hdr("application/apply-patch+yaml"),
            "fieldManager=t", b"apiVersion: example.com/v1\nkind: Widget\nmetadata: {name: w2}\nspec: {size: 1}\nstatus: {phase: Hacked}\n",
            StatusField::Kept).await.unwrap();
        assert!(out.get("status").is_none(), "{out}");

        // Writable: status is an ordinary field.
        let out = patch_stored_object(&s, &key, "widgets", "w1", Some("default"), &hdr("application/merge-patch+json"),
            "", br#"{"status":{"phase":"Set"}}"#, StatusField::Writable).await.unwrap();
        assert_eq!(out["status"], json!({"phase": "Set"}));
        let out = put_object(&s, &key, "w1", Some("default"), json!({"metadata": {"name": "w1"}, "spec": {}}),
            StatusField::Writable).await.unwrap();
        assert!(out.get("status").is_none());
    }

    #[test]
    fn status_field_on_create_and_update() {
        let mut o = json!({"spec": {}, "status": {"a": 1}});
        StatusField::Writable.on_create(&mut o);
        assert!(o.get("status").is_some());
        StatusField::Kept.on_create(&mut o);
        assert!(o.get("status").is_none());
        let mut o = json!({"spec": {}, "status": {"a": 2}});
        StatusField::Kept.on_update(&mut o, &json!({"spec": {}}));
        assert!(o.get("status").is_none(), "nothing stored, nothing kept");
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

#[cfg(test)]
mod status_write_tests {
    use super::*;

    /// A status write changes status, labels and annotations, never spec (#67).
    #[test]
    fn a_status_patch_keeps_its_annotations_and_not_spec() {
        let base = json!({"metadata": {"name": "c", "annotations": {"a": "1"}},
                          "spec": {"schedule": "* * * * *"}, "status": {}});
        let mut o = base.clone();
        apply_status_patch(&mut o, "application/merge-patch+json",
            br#"{"metadata":{"annotations":{"patchedstatus":"true"}},"status":{"active":[]},"spec":{"schedule":"x"}}"#).unwrap();
        assert_eq!(o["metadata"]["annotations"]["patchedstatus"], "true");
        assert_eq!(o["metadata"]["annotations"]["a"], "1");
        assert_eq!(o["spec"]["schedule"], "* * * * *");
        let mut o = base.clone();
        apply_status_patch(&mut o, "application/json-patch+json",
            br#"[{"op":"replace","path":"/spec/schedule","value":"x"},{"op":"add","path":"/status/x","value":1}]"#).unwrap();
        assert_eq!(o["spec"]["schedule"], "* * * * *");
        assert_eq!(o["status"]["x"], 1);
    }
}

#[cfg(test)]
mod immutable_tests {
    use super::*;

    #[test]
    fn an_immutable_configmap_keeps_its_data() {
        let k = "/registry/configmaps/ns/c";
        let old = json!({"immutable": true, "data": {"a": "1"}});
        assert!(check_immutable(k, &old, &json!({"immutable": true, "data": {"a": "1"}, "metadata": {"labels": {"x": "y"}}})).is_ok());
        assert!(check_immutable(k, &old, &json!({"immutable": true, "data": {"a": "2"}})).is_err());
        assert!(check_immutable(k, &old, &json!({"immutable": false, "data": {"a": "1"}})).is_err());
        assert!(check_immutable(k, &json!({"data": {"a": "1"}}), &json!({"data": {"a": "2"}})).is_ok());
        assert!(check_immutable("/registry/pods/ns/p", &old, &json!({})).is_ok());
    }

    #[test]
    fn a_priority_class_keeps_its_value_and_policy() {
        let k = "/registry/priorityclasses/high";
        let old = json!({"value": 1000, "preemptionPolicy": "PreemptLowerPriority", "description": "a"});
        assert!(check_immutable(k, &old, &json!({"value": 1000, "preemptionPolicy": "PreemptLowerPriority", "description": "b"})).is_ok());
        assert!(check_immutable(k, &old, &json!({"value": 1001, "preemptionPolicy": "PreemptLowerPriority"})).is_err());
        assert!(check_immutable(k, &old, &json!({"value": 1000, "preemptionPolicy": "Never"})).is_err());
        // Absent is the default, both ways round.
        assert!(check_immutable(k, &json!({"value": 1}), &json!({"value": 1, "preemptionPolicy": "PreemptLowerPriority"})).is_ok());
    }
}
