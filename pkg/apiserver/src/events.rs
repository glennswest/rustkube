//! `events.k8s.io/v1` — served over the SAME stored objects as core/v1 `Event`,
//! translating field names both directions. Upstream has served both since 1.19
//! (client-go's current EventRecorder writes events.k8s.io/v1, kubectl prefers
//! it). The stored representation stays core/v1; these handlers translate on the
//! way in and out (#48).

use crate::error::ApiError;
use crate::handlers::resource::{apply_patch_body, ensure_metadata_pub};
use crate::handlers::AppState;
use crate::storage::ResourceStorage;
use crate::watch::{self, WatchParams};
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};

const GV: &str = "events.k8s.io/v1";
const RESOURCE: &str = "events";

fn take<'a>(m: &'a mut Map<String, Value>, k: &str) -> Option<Value> {
    m.remove(k)
}

/// Stored core/v1 Event -> events.k8s.io/v1 representation.
pub fn core_to_events(mut e: Value) -> Value {
    let Some(m) = e.as_object_mut() else { return e };
    m.insert("apiVersion".into(), json!(GV));
    m.insert("kind".into(), json!("Event"));
    if let Some(v) = take(m, "involvedObject") {
        m.insert("regarding".into(), v);
    }
    if let Some(v) = take(m, "message") {
        m.insert("note".into(), v);
    }
    // reportingController from reportingComponent or source.component.
    let source = take(m, "source");
    if !m.contains_key("reportingController") {
        if let Some(rc) = take(m, "reportingComponent").filter(|v| !v.is_null()) {
            m.insert("reportingController".into(), rc);
        } else if let Some(c) = source.as_ref().and_then(|s| s.get("component")) {
            m.insert("reportingController".into(), c.clone());
        }
    }
    if !m.contains_key("reportingInstance") {
        if let Some(h) = source.as_ref().and_then(|s| s.get("host")) {
            m.insert("reportingInstance".into(), h.clone());
        }
    }
    if let Some(s) = source {
        m.insert("deprecatedSource".into(), s);
    }
    for (from, to) in [
        ("firstTimestamp", "deprecatedFirstTimestamp"),
        ("lastTimestamp", "deprecatedLastTimestamp"),
        ("count", "deprecatedCount"),
    ] {
        if let Some(v) = take(m, from) {
            m.insert(to.into(), v);
        }
    }
    e
}

/// events.k8s.io/v1 -> stored core/v1 Event representation.
pub fn events_to_core(mut e: Value) -> Value {
    let Some(m) = e.as_object_mut() else { return e };
    m.insert("apiVersion".into(), json!("v1"));
    m.insert("kind".into(), json!("Event"));
    if let Some(v) = take(m, "regarding") {
        m.insert("involvedObject".into(), v);
    }
    if let Some(v) = take(m, "note") {
        m.insert("message".into(), v);
    }
    let controller = take(m, "reportingController");
    if let Some(c) = controller.clone() {
        m.insert("reportingComponent".into(), c);
    }
    // source: prefer deprecatedSource, else synthesize from controller/instance.
    if let Some(s) = take(m, "deprecatedSource") {
        m.insert("source".into(), s);
    } else {
        let mut src = Map::new();
        if let Some(c) = controller {
            src.insert("component".into(), c);
        }
        if let Some(h) = m.get("reportingInstance").cloned() {
            src.insert("host".into(), h);
        }
        if !src.is_empty() {
            m.insert("source".into(), Value::Object(src));
        }
    }
    for (from, to) in [
        ("deprecatedFirstTimestamp", "firstTimestamp"),
        ("deprecatedLastTimestamp", "lastTimestamp"),
        ("deprecatedCount", "count"),
    ] {
        if let Some(v) = take(m, from) {
            m.entry(to.to_string()).or_insert(v);
        }
    }
    // Fold series.{count,lastObservedTime} into the core count/lastTimestamp.
    if let Some(series) = m.get("series").cloned() {
        if let Some(c) = series.get("count") {
            m.insert("count".into(), c.clone());
        }
        if let Some(l) = series.get("lastObservedTime") {
            m.entry("lastTimestamp".to_string()).or_insert(l.clone());
        }
    }
    e
}

fn event_list(items: Vec<Value>, revision: u64, continue_token: Option<String>) -> Value {
    let mut list = json!({
        "apiVersion": GV,
        "kind": "EventList",
        "metadata": { "resourceVersion": revision.to_string() },
        "items": items.into_iter().map(core_to_events).collect::<Vec<_>>(),
    });
    if let Some(t) = continue_token {
        list["metadata"]["continue"] = Value::String(t);
    }
    list
}

/// POST /apis/events.k8s.io/v1/namespaces/{ns}/events
pub async fn create(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
    Json(body): Json<Value>,
) -> Result<impl IntoResponse, ApiError> {
    let mut core = events_to_core(body);
    let name = core["metadata"]["name"]
        .as_str()
        .or_else(|| core["metadata"]["generateName"].as_str())
        .ok_or_else(|| ApiError::invalid("metadata.name is required"))?
        .to_string();
    ensure_metadata_pub(&mut core, &name, Some(&namespace));
    let key = ResourceStorage::namespaced_key(RESOURCE, &namespace, &name);
    let stored = state.storage.create(&key, core).await?;
    Ok((StatusCode::CREATED, Json(core_to_events(stored))))
}

/// GET/WATCH /apis/events.k8s.io/v1/namespaces/{ns}/events
pub async fn list_ns(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Response, ApiError> {
    let prefix = ResourceStorage::namespace_prefix(RESOURCE, &namespace);
    list_or_watch(&state, &prefix, query.as_deref().unwrap_or("")).await
}

/// GET/WATCH /apis/events.k8s.io/v1/events (all namespaces)
pub async fn list_all(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
) -> Result<Response, ApiError> {
    let prefix = ResourceStorage::all_namespaces_prefix(RESOURCE);
    list_or_watch(&state, &prefix, query.as_deref().unwrap_or("")).await
}

async fn list_or_watch(state: &AppState, prefix: &str, query: &str) -> Result<Response, ApiError> {
    let params = WatchParams::from_query(query);
    if params.watch {
        let (initial, live_rev) = if params.send_initial_events {
            let (items, _c, rev) = state.storage.list(prefix, 0, None).await?;
            (Some((items, rev)), rev)
        } else {
            (None, params.resource_version.unwrap_or(0))
        };
        let rx = state.storage.watch(prefix, live_rev).await?;
        return Ok(watch::watch_response(
            rx,
            watch::WatchResponseOpts {
                label_selector: params.label_selector.clone(),
                field_selector: params.field_selector.clone(),
                api_version: GV.to_string(),
                kind: "Event".to_string(),
                allow_bookmarks: params.allow_watch_bookmarks,
                metadata_only: false,
                transform: Some(core_to_events),
                initial,
            },
        ));
    }
    let limit = params.limit.unwrap_or(500);
    let (items, continue_token, revision) =
        state.storage.list(prefix, limit, params.continue_token.as_deref()).await?;
    let items =
        crate::selector::filter_objects(items, &params.label_selector, &params.field_selector);
    Ok(Json(event_list(items, revision, continue_token)).into_response())
}

/// GET /apis/events.k8s.io/v1/namespaces/{ns}/events/{name}
pub async fn get(
    State(state): State<AppState>,
    Path((namespace, name)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    let key = ResourceStorage::namespaced_key(RESOURCE, &namespace, &name);
    Ok(Json(core_to_events(state.storage.get(&key).await?)))
}

/// PUT /apis/events.k8s.io/v1/namespaces/{ns}/events/{name}
pub async fn update(
    State(state): State<AppState>,
    Path((namespace, name)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<impl IntoResponse, ApiError> {
    let key = ResourceStorage::namespaced_key(RESOURCE, &namespace, &name);
    let core = events_to_core(body);
    let prev_rev = core["metadata"]["resourceVersion"].as_str().and_then(|r| r.parse::<u64>().ok());
    let stored = state.storage.update(&key, core, prev_rev).await?;
    Ok(Json(core_to_events(stored)))
}

/// PATCH /apis/events.k8s.io/v1/namespaces/{ns}/events/{name} — the patch is in
/// the events.k8s.io/v1 representation, so apply it to that view then store core.
pub async fn patch(
    State(state): State<AppState>,
    Path((namespace, name)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let key = ResourceStorage::namespaced_key(RESOURCE, &namespace, &name);
    let mut as_events = core_to_events(state.storage.get(&key).await?);
    let ct = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    apply_patch_body(&mut as_events, ct, &body)?;
    let core = events_to_core(as_events);
    let prev_rev = core["metadata"]["resourceVersion"].as_str().and_then(|r| r.parse::<u64>().ok());
    let stored = state.storage.update(&key, core, prev_rev).await?;
    Ok(Json(core_to_events(stored)))
}

/// DELETE /apis/events.k8s.io/v1/namespaces/{ns}/events/{name}
pub async fn delete(
    State(state): State<AppState>,
    Path((namespace, name)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    let key = ResourceStorage::namespaced_key(RESOURCE, &namespace, &name);
    state.storage.delete(&key, None).await?;
    Ok(Json(json!({
        "apiVersion": "v1", "kind": "Status", "metadata": {}, "status": "Success",
        "details": { "name": name, "namespace": namespace, "kind": RESOURCE, "group": "events.k8s.io" }
    })))
}

/// GET /apis/events.k8s.io/v1 — resource discovery.
pub async fn discovery() -> impl IntoResponse {
    Json(json!({
        "kind": "APIResourceList",
        "apiVersion": "v1",
        "groupVersion": GV,
        "resources": [{
            "name": "events",
            "singularName": "event",
            "namespaced": true,
            "kind": "Event",
            "verbs": ["create","delete","deletecollection","get","list","patch","update","watch"],
            "shortNames": ["ev"]
        }]
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_core_and_events_fields() {
        let core = json!({
            "apiVersion": "v1", "kind": "Event",
            "metadata": {"name": "e1", "namespace": "default"},
            "involvedObject": {"kind": "Pod", "name": "p1"},
            "message": "started", "reason": "Started", "type": "Normal",
            "source": {"component": "kubelet", "host": "node1"},
            "count": 3, "firstTimestamp": "t0", "lastTimestamp": "t2",
            "eventTime": "t0"
        });
        let ev = core_to_events(core.clone());
        assert_eq!(ev["apiVersion"], "events.k8s.io/v1");
        assert_eq!(ev["regarding"]["name"], "p1");
        assert_eq!(ev["note"], "started");
        assert_eq!(ev["reportingController"], "kubelet");
        assert_eq!(ev["reportingInstance"], "node1");
        assert_eq!(ev["deprecatedCount"], 3);
        assert!(ev.get("involvedObject").is_none() && ev.get("message").is_none());

        // events.k8s.io/v1 (as a client would send) -> core
        let sent = json!({
            "apiVersion": "events.k8s.io/v1", "kind": "Event",
            "metadata": {"name": "e2"},
            "regarding": {"kind": "Pod", "name": "p2"}, "note": "hi",
            "reportingController": "op", "reportingInstance": "op-0",
            "reason": "R", "type": "Normal", "eventTime": "t0",
            "series": {"count": 5, "lastObservedTime": "t5"}
        });
        let c = events_to_core(sent);
        assert_eq!(c["apiVersion"], "v1");
        assert_eq!(c["involvedObject"]["name"], "p2");
        assert_eq!(c["message"], "hi");
        assert_eq!(c["source"]["component"], "op");
        assert_eq!(c["count"], 5, "series.count folded into core count");
        assert_eq!(c["lastTimestamp"], "t5");
    }
}
