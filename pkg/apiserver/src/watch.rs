//! Watch event streaming.
//!
//! Implements the K8s watch protocol: chunked JSON stream of WatchEvent
//! objects, each terminated by a newline. Supports watch bookmarks (KEP-3157)
//! and the streaming-list / `WatchList` protocol (KEP-3670): with
//! `sendInitialEvents=true` the stream replays current state as ADDED events and
//! then emits a BOOKMARK annotated `k8s.io/initial-events-end: "true"`, which is
//! how client-go informers learn their initial list is complete and mark
//! themselves synced. Without it, modern informers (e.g. the Cilium agent) block
//! forever waiting for that bookmark.

use crate::selector;
use apimachinery::watch::WatchEvent;
use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{json, Value};
use std::convert::Infallible;
use tokio::sync::mpsc;
use tokio::time::{interval, Duration, MissedTickBehavior};
use tokio_stream::wrappers::ReceiverStream;

/// How often an otherwise-idle watch emits a heartbeat BOOKMARK when the client
/// set `allowWatchBookmarks=true`. Under the ~1-2min informer bookmark timeout
/// so long-lived, quiet watches don't trip "no events received".
const BOOKMARK_INTERVAL_SECS: u64 = 45;
/// Depth of the rendered-line channel feeding the HTTP body.
const LINE_CHANNEL: usize = 256;

/// True if `Accept` requests the metadata-only projection
/// (`application/json;as=PartialObjectMetadata;g=meta.k8s.io;v=v1`), used by
/// metadata informers — e.g. the Cilium agent watching CRDs.
pub fn wants_partial_metadata(accept: &str) -> bool {
    accept.contains("as=PartialObjectMetadata")
}

/// Project a full object to a meta.k8s.io/v1 `PartialObjectMetadata` (TypeMeta +
/// metadata only), as the `as=PartialObjectMetadata` content negotiation returns.
pub fn to_partial_object_metadata(obj: &Value) -> Value {
    json!({
        "apiVersion": "meta.k8s.io/v1",
        "kind": "PartialObjectMetadata",
        "metadata": obj.get("metadata").cloned().unwrap_or_else(|| json!({})),
    })
}

/// Options for rendering a watch response.
pub struct WatchResponseOpts {
    pub label_selector: Option<String>,
    pub field_selector: Option<String>,
    pub api_version: String,
    pub kind: String,
    /// Client set `allowWatchBookmarks=true` — emit periodic heartbeat bookmarks.
    pub allow_bookmarks: bool,
    /// Client requested `as=PartialObjectMetadata` — project every event object
    /// (and the type advertised on bookmarks) to PartialObjectMetadata.
    pub metadata_only: bool,
    /// For `sendInitialEvents=true` (WatchList): the current objects and the
    /// revision they reflect. Streamed as ADDED events, followed by an
    /// `initial-events-end` BOOKMARK, before any live events. The caller must
    /// open the live watch at this same revision so there is no gap or overlap.
    pub initial: Option<(Vec<Value>, u64)>,
}

/// Convert a watch stream into an HTTP response of chunked JSON watch events,
/// filtered by label/field selectors, with bookmark and WatchList support.
pub fn watch_response(mut rx: mpsc::Receiver<WatchEvent>, opts: WatchResponseOpts) -> Response {
    let WatchResponseOpts {
        label_selector,
        field_selector,
        api_version,
        kind,
        allow_bookmarks,
        metadata_only,
        initial,
    } = opts;

    // Under `as=PartialObjectMetadata`, every event object (and the type on
    // bookmarks/tombstones) is a meta.k8s.io/v1 PartialObjectMetadata.
    let (api_version, kind) = if metadata_only {
        ("meta.k8s.io/v1".to_string(), "PartialObjectMetadata".to_string())
    } else {
        (api_version, kind)
    };

    let (tx, out_rx) = mpsc::channel::<std::result::Result<String, Infallible>>(LINE_CHANNEL);
    tokio::spawn(async move {
        let mut last_rev = 0u64;

        // --- initial events (WatchList / sendInitialEvents=true) --------------
        if let Some((items, list_rev)) = initial {
            for obj in &items {
                if let Some(line) = render_initial_added(
                    obj, &label_selector, &field_selector, &api_version, &kind, list_rev, metadata_only,
                ) {
                    if tx.send(Ok(line)).await.is_err() {
                        return;
                    }
                }
            }
            // End-of-initial-list signal: without this, client-go WatchList
            // informers never report synced.
            if tx
                .send(Ok(render_bookmark(list_rev, true, &api_version, &kind)))
                .await
                .is_err()
            {
                return;
            }
            last_rev = list_rev;
        }

        // --- live events, interleaved with idle heartbeat bookmarks -----------
        let mut idle = interval(Duration::from_secs(BOOKMARK_INTERVAL_SECS));
        idle.set_missed_tick_behavior(MissedTickBehavior::Delay);
        idle.tick().await; // consume the immediate first tick
        loop {
            tokio::select! {
                maybe = rx.recv() => match maybe {
                    Some(event) => {
                        last_rev = event.revision();
                        if let Some(line) = render_event(
                            &event, &label_selector, &field_selector, &api_version, &kind, metadata_only,
                        ) {
                            if tx.send(Ok(line)).await.is_err() {
                                return;
                            }
                        }
                        // Real activity resets the heartbeat so bookmarks only
                        // fill quiet gaps (matching upstream behavior).
                        idle.reset();
                    }
                    None => return, // upstream watch closed
                },
                _ = idle.tick(), if allow_bookmarks => {
                    if tx
                        .send(Ok(render_bookmark(last_rev, false, &api_version, &kind)))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json;stream=watch")
        .header("transfer-encoding", "chunked")
        .body(Body::from_stream(ReceiverStream::new(out_rx)))
        .unwrap()
}

/// Serialize a `{type, object}` watch event to a newline-terminated line.
fn render_line(event_type: &str, object: Value) -> String {
    let mut line = serde_json::to_string(&json!({"type": event_type, "object": object}))
        .unwrap_or_default();
    line.push('\n');
    line
}

/// Render a live `WatchEvent` to a line, applying selectors. `None` if filtered.
fn render_event(
    event: &WatchEvent,
    label_sel: &Option<String>,
    field_sel: &Option<String>,
    api_version: &str,
    kind: &str,
    metadata_only: bool,
) -> Option<String> {
    let (event_type, mut object) = match event {
        WatchEvent::Added { value, revision, .. } => {
            let mut obj: Value = serde_json::from_slice(value).unwrap_or(json!({}));
            inject_resource_version(&mut obj, *revision);
            if !selector::matches_selectors(&obj, label_sel, field_sel) {
                return None;
            }
            // Selectors match on the full object; project after.
            if metadata_only {
                obj = to_partial_object_metadata(&obj);
            }
            ("ADDED", obj)
        }
        WatchEvent::Modified { value, revision, .. } => {
            let mut obj: Value = serde_json::from_slice(value).unwrap_or(json!({}));
            inject_resource_version(&mut obj, *revision);
            if !selector::matches_selectors(&obj, label_sel, field_sel) {
                return None;
            }
            if metadata_only {
                obj = to_partial_object_metadata(&obj);
            }
            ("MODIFIED", obj)
        }
        WatchEvent::Deleted { revision, key, .. } => {
            // The store doesn't carry the prior value on delete, so we synthesize
            // the tombstone. It MUST carry apiVersion/kind: client-go refuses to
            // decode a watch event whose object has no Kind ("unable to decode
            // watch event: Object 'Kind' is missing"), which kills the informer.
            let (namespace, name) = split_key(key);
            let mut meta = json!({"name": name, "resourceVersion": revision.to_string()});
            if let Some(ns) = namespace {
                meta["namespace"] = json!(ns);
            }
            return Some(render_line(
                "DELETED",
                json!({"apiVersion": api_version, "kind": kind, "metadata": meta}),
            ));
        }
        WatchEvent::Bookmark { revision } => {
            return Some(render_bookmark(*revision, false, api_version, kind));
        }
    };

    // ADDED/MODIFIED come straight from storage and normally carry their own
    // TypeMeta, but backfill it if an object was stored without one.
    if object.get("kind").and_then(|k| k.as_str()).is_none() {
        object["apiVersion"] = json!(api_version);
        object["kind"] = json!(kind);
    }
    Some(render_line(event_type, object))
}

/// Render a WatchList initial object as an ADDED event. `None` if filtered.
fn render_initial_added(
    obj: &Value,
    label_sel: &Option<String>,
    field_sel: &Option<String>,
    api_version: &str,
    kind: &str,
    list_rev: u64,
    metadata_only: bool,
) -> Option<String> {
    let mut obj = obj.clone();
    if obj.get("kind").and_then(|k| k.as_str()).is_none() {
        obj["apiVersion"] = json!(api_version);
        obj["kind"] = json!(kind);
    }
    // Keep the object's own resourceVersion; only backfill if it lacks one.
    let has_rv = obj
        .pointer("/metadata/resourceVersion")
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    if !has_rv {
        inject_resource_version(&mut obj, list_rev);
    }
    if !selector::matches_selectors(&obj, label_sel, field_sel) {
        return None;
    }
    if metadata_only {
        obj = to_partial_object_metadata(&obj);
    }
    Some(render_line("ADDED", obj))
}

/// Render a BOOKMARK event at `revision`. When `initial_end` is set it carries
/// the `k8s.io/initial-events-end: "true"` annotation (WatchList end-of-list).
fn render_bookmark(revision: u64, initial_end: bool, api_version: &str, kind: &str) -> String {
    let mut meta = json!({"resourceVersion": revision.to_string()});
    if initial_end {
        meta["annotations"] = json!({"k8s.io/initial-events-end": "true"});
    }
    render_line(
        "BOOKMARK",
        json!({"apiVersion": api_version, "kind": kind, "metadata": meta}),
    )
}

/// Split a registry key into `(namespace, name)`.
///
/// Keys look like `/registry/<resource>/<namespace>/<name>` for namespaced
/// resources and `/registry/<resource>/<name>` for cluster-scoped ones.
fn split_key(key: &str) -> (Option<String>, String) {
    let parts: Vec<&str> = key.trim_start_matches('/').split('/').collect();
    // ["registry", resource, ...rest]
    match parts.len() {
        n if n >= 4 => (
            Some(parts[2].to_string()),
            parts[n - 1].to_string(),
        ),
        n if n >= 1 => (None, parts[n - 1].to_string()),
        _ => (None, String::new()),
    }
}

fn inject_resource_version(obj: &mut serde_json::Value, revision: u64) {
    if let Some(meta) = obj.get_mut("metadata").and_then(|m| m.as_object_mut()) {
        meta.insert(
            "resourceVersion".into(),
            serde_json::Value::String(revision.to_string()),
        );
    }
}

/// Parse watch query parameters.
pub struct WatchParams {
    pub watch: bool,
    pub resource_version: Option<u64>,
    pub limit: Option<usize>,
    pub continue_token: Option<String>,
    pub label_selector: Option<String>,
    pub field_selector: Option<String>,
    /// `allowWatchBookmarks=true` — client accepts periodic BOOKMARK events.
    pub allow_watch_bookmarks: bool,
    /// `sendInitialEvents=true` — WatchList: replay current state then emit the
    /// `initial-events-end` bookmark before live events.
    pub send_initial_events: bool,
}

impl WatchParams {
    pub fn from_query(query: &str) -> Self {
        let mut params = Self {
            watch: false,
            resource_version: None,
            limit: None,
            continue_token: None,
            label_selector: None,
            field_selector: None,
            allow_watch_bookmarks: false,
            send_initial_events: false,
        };
        // Percent-decode keys and values. Clients (kubectl, client-go) URL-encode
        // query values — notably the `continue` token, which is a raw store key
        // full of `/` (`%2F`), and label/field selectors (`=` → `%3D`, `,` →
        // `%2C`). Without decoding, a `%2F…`-prefixed continue token sorts before
        // every real key, so pagination silently restarts from the top and a
        // multi-page LIST loops forever (kubectl hang on large collections).
        for (key, val) in form_urlencoded::parse(query.as_bytes()) {
            match key.as_ref() {
                "watch" => params.watch = val == "true" || val == "1",
                "allowWatchBookmarks" => {
                    params.allow_watch_bookmarks = val == "true" || val == "1"
                }
                "sendInitialEvents" => {
                    params.send_initial_events = val == "true" || val == "1"
                }
                "resourceVersion" => params.resource_version = val.parse().ok(),
                "limit" => params.limit = val.parse().ok(),
                "continue" => {
                    if !val.is_empty() {
                        params.continue_token = Some(val.into_owned());
                    }
                }
                "labelSelector" => {
                    if !val.is_empty() {
                        params.label_selector = Some(val.into_owned());
                    }
                }
                "fieldSelector" => {
                    if !val.is_empty() {
                        params.field_selector = Some(val.into_owned());
                    }
                }
                _ => {}
            }
        }
        params
    }
}

#[cfg(test)]
mod tests {
    use super::WatchParams;

    #[test]
    fn continue_token_is_percent_decoded() {
        // kubectl sends the store key URL-encoded; the decoded token must be the
        // raw key so pagination resumes after it instead of restarting.
        let q = "limit=500&continue=%2Fregistry%2Fnamespaces%2Fsoak-1495";
        let p = WatchParams::from_query(q);
        assert_eq!(p.limit, Some(500));
        assert_eq!(
            p.continue_token.as_deref(),
            Some("/registry/namespaces/soak-1495")
        );
    }

    #[test]
    fn selectors_and_watch_flags_decode() {
        let q = "watch=true&labelSelector=app%3Dnginx%2Ctier%3Dweb&fieldSelector=metadata.name%3Dfoo";
        let p = WatchParams::from_query(q);
        assert!(p.watch);
        assert_eq!(p.label_selector.as_deref(), Some("app=nginx,tier=web"));
        assert_eq!(p.field_selector.as_deref(), Some("metadata.name=foo"));
    }

    #[test]
    fn empty_and_missing_values_stay_none() {
        let p = WatchParams::from_query("continue=&labelSelector=");
        assert!(p.continue_token.is_none());
        assert!(p.label_selector.is_none());
        let p = WatchParams::from_query("");
        assert!(!p.watch);
        assert!(p.limit.is_none());
        // Bookmark flags default off.
        assert!(!p.allow_watch_bookmarks);
        assert!(!p.send_initial_events);
    }

    #[test]
    fn watchlist_flags_parse() {
        // client-go WatchList issues both flags.
        let q = "watch=true&allowWatchBookmarks=true&sendInitialEvents=true&resourceVersion=";
        let p = WatchParams::from_query(q);
        assert!(p.watch);
        assert!(p.allow_watch_bookmarks);
        assert!(p.send_initial_events);
    }

    #[test]
    fn partial_object_metadata_projection() {
        assert!(super::wants_partial_metadata(
            "application/json;as=PartialObjectMetadata;g=meta.k8s.io;v=v1"
        ));
        assert!(!super::wants_partial_metadata("application/json"));
        let full = serde_json::json!({
            "apiVersion": "apiextensions.k8s.io/v1", "kind": "CustomResourceDefinition",
            "metadata": {"name": "ciliumidentities.cilium.io", "resourceVersion": "42"},
            "spec": {"group": "cilium.io"}, "status": {"conditions": []}
        });
        let p = super::to_partial_object_metadata(&full);
        assert_eq!(p["apiVersion"], "meta.k8s.io/v1");
        assert_eq!(p["kind"], "PartialObjectMetadata");
        assert_eq!(p["metadata"]["name"], "ciliumidentities.cilium.io");
        assert_eq!(p["metadata"]["resourceVersion"], "42");
        assert!(p.get("spec").is_none() && p.get("status").is_none(), "spec/status dropped");
    }

    #[test]
    fn initial_events_end_bookmark_is_annotated() {
        let line = super::render_bookmark(4242, true, "cilium.io/v2", "CiliumIdentity");
        let v: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(v["type"], "BOOKMARK");
        assert_eq!(v["object"]["kind"], "CiliumIdentity");
        assert_eq!(v["object"]["metadata"]["resourceVersion"], "4242");
        assert_eq!(
            v["object"]["metadata"]["annotations"]["k8s.io/initial-events-end"],
            "true"
        );
        // A plain heartbeat bookmark carries no initial-events-end annotation.
        let hb = super::render_bookmark(5, false, "v1", "Pod");
        let hv: serde_json::Value = serde_json::from_str(hb.trim_end()).unwrap();
        assert!(hv["object"]["metadata"]["annotations"].is_null());
    }
}
