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
    /// Optional per-object transform applied before rendering (e.g. translating
    /// stored core/v1 Events into the events.k8s.io/v1 representation).
    pub transform: Option<fn(Value) -> Value>,
    /// For `sendInitialEvents=true` (WatchList): the current objects and the
    /// revision they reflect. Streamed as ADDED events, followed by an
    /// `initial-events-end` BOOKMARK, before any live events. The caller must
    /// open the live watch at this same revision so there is no gap or overlap.
    pub initial: Option<(Vec<Value>, u64)>,
    /// End the initial events with the `initial-events-end` BOOKMARK — only
    /// for `sendInitialEvents=true`; a plain watch with no resourceVersion
    /// gets the ADDED events alone, as upstream sends them.
    pub initial_end_bookmark: bool,
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
        transform,
        initial,
        initial_end_bookmark,
    } = opts;

    // Under `as=PartialObjectMetadata`, every event object (and the type on
    // bookmarks/tombstones) is a meta.k8s.io/v1 PartialObjectMetadata.
    let (api_version, kind) = if metadata_only {
        (
            "meta.k8s.io/v1".to_string(),
            "PartialObjectMetadata".to_string(),
        )
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
                    obj,
                    &label_selector,
                    &field_selector,
                    &api_version,
                    &kind,
                    list_rev,
                    metadata_only,
                    transform,
                ) {
                    if tx.send(Ok(line)).await.is_err() {
                        return;
                    }
                }
            }
            // End-of-initial-list signal: without this, client-go WatchList
            // informers never report synced.
            if initial_end_bookmark
                && tx
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
                _ = tx.closed() => return,
                maybe = rx.recv() => match maybe {
                    Some(event) => {
                        last_rev = event.revision();
                        if let Some(line) = render_event(
                            &event, &label_selector, &field_selector, &api_version, &kind, metadata_only, transform,
                        ) {
                            if tx.send(Ok(line)).await.is_err() {
                                return;
                            }
                        }
                        if matches!(event, WatchEvent::Error { .. }) { return; }
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
    let mut line =
        serde_json::to_string(&json!({"type": event_type, "object": object})).unwrap_or_default();
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
    transform: Option<fn(Value) -> Value>,
) -> Option<String> {
    let (event_type, mut object) = match event {
        WatchEvent::Added {
            value, revision, ..
        } => {
            let mut obj: Value = serde_json::from_slice(value).unwrap_or(json!({}));
            inject_resource_version(&mut obj, *revision);
            if !selector::matches_selectors(&obj, label_sel, field_sel) {
                return None;
            }
            // Selectors match on the full stored object; translate/project after.
            if let Some(f) = transform {
                obj = f(obj);
            }
            if metadata_only {
                obj = to_partial_object_metadata(&obj);
            }
            ("ADDED", obj)
        }
        WatchEvent::Modified {
            value,
            revision,
            prev_value,
            ..
        } => {
            let mut obj: Value = serde_json::from_slice(value).unwrap_or(json!({}));
            inject_resource_version(&mut obj, *revision);
            // Upstream's selector semantics: to a watcher of `app=web`, a
            // write that takes an object out of `app=web` is its DELETED
            // (carrying the new state), and one that brings it in is its
            // ADDED. Without the previous state (a watch below the cache's
            // window) a non-matching write is dropped and a matching one is
            // MODIFIED, as before.
            let now = selector::matches_selectors(&obj, label_sel, field_sel);
            let before = prev_value
                .as_deref()
                .and_then(|b| serde_json::from_slice::<Value>(b).ok())
                .map(|p| selector::matches_selectors(&p, label_sel, field_sel));
            let event_type = match (before, now) {
                (_, true) if before == Some(false) => "ADDED",
                (_, true) => "MODIFIED",
                (Some(true), false) => "DELETED",
                (_, false) => return None,
            };
            if let Some(f) = transform {
                obj = f(obj);
            }
            if metadata_only {
                obj = to_partial_object_metadata(&obj);
            }
            (event_type, obj)
        }
        WatchEvent::Deleted {
            revision,
            key,
            prev_value,
        } => {
            // The object's last state, when the watch cache held it (#100):
            // what upstream sends, and what selectors are applied to — a
            // watcher of `app=web` hears about the deletion of a web pod and
            // not of every other pod. Its resourceVersion is the delete's.
            if let Some(mut obj) = prev_value
                .as_deref()
                .and_then(|b| serde_json::from_slice::<Value>(b).ok())
                .filter(Value::is_object)
            {
                inject_resource_version(&mut obj, *revision);
                if !selector::matches_selectors(&obj, label_sel, field_sel) {
                    return None;
                }
                if let Some(f) = transform {
                    obj = f(obj);
                }
                if metadata_only {
                    obj = to_partial_object_metadata(&obj);
                }
                ("DELETED", obj)
            } else {
                // Nobody held it — a watch opened below the cache's window
                // reads the store directly — so the tombstone is synthesized
                // from the key. It MUST carry apiVersion/kind: client-go
                // refuses to decode a watch event whose object has no Kind
                // ("unable to decode watch event: Object 'Kind' is missing"),
                // which kills the informer. Unfiltered, because there is
                // nothing to match a selector against.
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
        }
        WatchEvent::Error { code, message, .. } => {
            return Some(render_line(
                "ERROR",
                json!({
                    "apiVersion": "v1", "kind": "Status", "status": "Failure",
                    "reason": if *code == 410 { "Expired" } else { "ServiceUnavailable" },
                    "code": code, "message": message,
                }),
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
    transform: Option<fn(Value) -> Value>,
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
    if let Some(f) = transform {
        obj = f(obj);
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
/// A built-in resource is one segment and a custom resource two — its group
/// and its plural (#76):
///
/// ```text
/// /registry/<resource>/[<namespace>/]<name>
/// /registry/<group>/<plural>/[<namespace>/]<name>
/// ```
///
/// A CRD group always contains a dot and a built-in plural never does, so the
/// segment after `registry` says which. Reading the namespace at a fixed
/// position instead gave every custom resource's tombstone its plural as a
/// namespace — `virtualmachineinstances/web-1` for `default/web-1` — and an
/// informer keyed on namespace/name never dropped the object (#100).
fn split_key(key: &str) -> (Option<String>, String) {
    let parts: Vec<&str> = key.trim_start_matches('/').split('/').collect();
    let resource_len = if parts.get(1).is_some_and(|seg| seg.contains('.')) {
        2
    } else {
        1
    };
    match parts.get(1 + resource_len..).unwrap_or(&[]) {
        [namespace, name] => (Some(namespace.to_string()), name.to_string()),
        [name] => (None, name.to_string()),
        _ => (None, parts.last().copied().unwrap_or_default().to_string()),
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
    /// `resourceVersion` as sent, and `resourceVersionMatch`: what a LIST or
    /// GET may be served from ([`Self::read`], #171).
    pub resource_version_raw: Option<String>,
    pub resource_version_match: Option<String>,
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
    /// Does this watch start with the current state, as ADDED events?
    ///
    /// For `sendInitialEvents=true`, and — as upstream — for a watch with no
    /// `resourceVersion` or `"0"`: "get state and start at most recent". Only
    /// a watch from a specific revision starts after it. A bare watch used to
    /// start from now and say nothing about what exists, so a client waiting
    /// for the ADDED of an object it had just created waited forever (#67:
    /// the ServiceAccount lifecycle conformance spec hung to the suite
    /// timeout).
    pub fn wants_initial_state(&self) -> bool {
        self.send_initial_events || matches!(self.resource_version, None | Some(0))
    }

    /// Where a LIST or GET with these parameters may be read from (#171).
    pub fn read(&self) -> Result<crate::storage::Read, crate::error::ApiError> {
        crate::storage::Read::from_params(
            self.resource_version_raw.as_deref(),
            self.resource_version_match.as_deref(),
        )
    }

    pub fn from_query(query: &str) -> Self {
        let mut params = Self {
            watch: false,
            resource_version: None,
            resource_version_raw: None,
            resource_version_match: None,
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
                "allowWatchBookmarks" => params.allow_watch_bookmarks = val == "true" || val == "1",
                "sendInitialEvents" => params.send_initial_events = val == "true" || val == "1",
                "resourceVersion" => {
                    params.resource_version = val.parse().ok();
                    params.resource_version_raw = Some(val.into_owned());
                }
                "resourceVersionMatch" => params.resource_version_match = Some(val.into_owned()),
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

    /// No resourceVersion, or "0", starts with the current state (#67).
    #[test]
    fn a_watch_without_a_revision_starts_with_the_state() {
        assert!(WatchParams::from_query("watch=true").wants_initial_state());
        assert!(WatchParams::from_query("watch=true&resourceVersion=0").wants_initial_state());
        assert!(!WatchParams::from_query("watch=true&resourceVersion=42").wants_initial_state());
        assert!(
            WatchParams::from_query("watch=true&resourceVersion=42&sendInitialEvents=true")
                .wants_initial_state()
        );
    }

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
        let q =
            "watch=true&labelSelector=app%3Dnginx%2Ctier%3Dweb&fieldSelector=metadata.name%3Dfoo";
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
        assert!(
            p.get("spec").is_none() && p.get("status").is_none(),
            "spec/status dropped"
        );
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

    /// A DELETED event names the object in the namespace it lived in, for
    /// built-ins and custom resources of either scope (#100).
    #[test]
    fn tombstones_name_the_real_namespace() {
        use super::split_key;
        let cases = [
            ("/registry/pods/default/web-1", Some("default"), "web-1"),
            ("/registry/nodes/n1", None, "n1"),
            (
                "/registry/kubevirt.io/virtualmachineinstances/default/web-1",
                Some("default"),
                "web-1",
            ),
            ("/registry/demo.io/gadgets/g1", None, "g1"),
            // The CRD object itself: a built-in, whose *name* has dots.
            (
                "/registry/customresourcedefinitions/widgets.demo.io",
                None,
                "widgets.demo.io",
            ),
        ];
        for (key, ns, name) in cases {
            let (got_ns, got_name) = split_key(key);
            assert_eq!(got_ns.as_deref(), ns, "{key}");
            assert_eq!(got_name, name, "{key}");
        }
    }

    fn deleted(key: &str, prev: Option<serde_json::Value>) -> apimachinery::watch::WatchEvent {
        apimachinery::watch::WatchEvent::Deleted {
            key: key.into(),
            revision: 35,
            prev_value: prev.map(|v| serde_json::to_vec(&v).unwrap()),
        }
    }

    fn render(
        ev: &apimachinery::watch::WatchEvent,
        label: Option<&str>,
    ) -> Option<serde_json::Value> {
        super::render_event(
            ev,
            &label.map(str::to_string),
            &None,
            "kubevirt.io/v1",
            "VirtualMachineInstance",
            false,
            None,
        )
        .map(|line| serde_json::from_str(line.trim()).unwrap())
    }

    #[test]
    fn expired_watch_is_status_even_with_selectors_and_metadata_projection() {
        let event = apimachinery::watch::WatchEvent::Error {
            code: 410,
            message: "watch history expired".into(),
            revision: 35,
        };
        let line = super::render_event(
            &event,
            &Some("app=absent".into()),
            &Some("metadata.name=absent".into()),
            "v1",
            "Pod",
            true,
            Some(|_| panic!("Status must bypass object transforms")),
        )
        .unwrap();
        let rendered: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(rendered["type"], "ERROR");
        assert_eq!(rendered["object"]["kind"], "Status");
        assert_eq!(rendered["object"]["code"], 410);
        assert_eq!(rendered["object"]["reason"], "Expired");
    }

    /// The case stormconsole found: a VMI deleted in `default`.
    #[test]
    fn a_custom_resource_tombstone_is_droppable() {
        let ev = deleted(
            "/registry/kubevirt.io/virtualmachineinstances/default/web-1",
            None,
        );
        let e = render(&ev, None).unwrap();
        assert_eq!(e["type"], "DELETED");
        assert_eq!(e["object"]["kind"], "VirtualMachineInstance");
        assert_eq!(e["object"]["metadata"]["namespace"], "default");
        assert_eq!(e["object"]["metadata"]["name"], "web-1");
        assert_eq!(e["object"]["metadata"]["resourceVersion"], "35");
    }

    /// With the last state held, DELETED carries the object — at the delete's
    /// resourceVersion — and selectors apply to it.
    #[test]
    fn a_deleted_event_carries_the_last_state() {
        let last = serde_json::json!({
            "apiVersion": "kubevirt.io/v1", "kind": "VirtualMachineInstance",
            "metadata": { "name": "web-1", "namespace": "default", "uid": "u1",
                          "labels": { "app": "web" }, "finalizers": ["f"] },
            "status": { "phase": "Running" },
        });
        let ev = deleted(
            "/registry/kubevirt.io/virtualmachineinstances/default/web-1",
            Some(last),
        );
        let e = render(&ev, None).unwrap();
        assert_eq!(e["type"], "DELETED");
        assert_eq!(e["object"]["metadata"]["namespace"], "default");
        assert_eq!(e["object"]["metadata"]["uid"], "u1");
        assert_eq!(e["object"]["metadata"]["resourceVersion"], "35");
        assert_eq!(e["object"]["status"]["phase"], "Running");

        assert!(render(&ev, Some("app=web")).is_some());
        assert!(
            render(&ev, Some("app=db")).is_none(),
            "a watcher of app=db heard about a web VMI"
        );
    }

    fn modified(prev: Option<&str>, now: &str) -> apimachinery::watch::WatchEvent {
        let obj = |app: &str| {
            serde_json::to_vec(&serde_json::json!({
                "apiVersion": "kubevirt.io/v1", "kind": "VirtualMachineInstance",
                "metadata": { "name": "web-1", "namespace": "default", "labels": { "app": app } },
            }))
            .unwrap()
        };
        apimachinery::watch::WatchEvent::Modified {
            key: "/registry/kubevirt.io/virtualmachineinstances/default/web-1".into(),
            value: obj(now),
            revision: 36,
            prev_value: prev.map(obj),
        }
    }

    /// An object that stops matching a selector is DELETED to that watcher;
    /// one that starts matching is ADDED (#67, the Watchers conformance spec).
    #[test]
    fn a_selector_watch_sees_objects_leave_and_enter() {
        let ty = |ev, sel| render(&ev, sel).map(|e| e["type"].as_str().unwrap().to_string());
        assert_eq!(
            ty(modified(Some("web"), "db"), Some("app=web")).as_deref(),
            Some("DELETED")
        );
        let e = render(&modified(Some("web"), "db"), Some("app=web")).unwrap();
        assert_eq!(
            e["object"]["metadata"]["labels"]["app"], "db",
            "DELETED carries the new state"
        );
        assert_eq!(
            ty(modified(Some("db"), "web"), Some("app=web")).as_deref(),
            Some("ADDED")
        );
        assert_eq!(
            ty(modified(Some("web"), "web"), Some("app=web")).as_deref(),
            Some("MODIFIED")
        );
        assert_eq!(ty(modified(Some("db"), "db"), Some("app=web")), None);
        // No previous state: matching is MODIFIED, not matching is dropped.
        assert_eq!(
            ty(modified(None, "web"), Some("app=web")).as_deref(),
            Some("MODIFIED")
        );
        assert_eq!(ty(modified(None, "db"), Some("app=web")), None);
        assert_eq!(
            ty(modified(Some("db"), "web"), None).as_deref(),
            Some("MODIFIED")
        );
    }
}

/// Every frame of a metadata-only watch (`as=PartialObjectMetadata`), as a
/// client-go metadata informer decodes it (#180): the cilium agent's CRD
/// watch logged "unable to decode an event from the watch stream" on server1.
/// Each frame type the watch can send is rendered here and held to what Go's
/// `metav1.PartialObjectMetadata` decodes — the right TypeMeta, nothing beside
/// `metadata`, and every `ObjectMeta` field of the Go type — so a frame that
/// would fail that decode fails here.
#[cfg(test)]
mod metadata_projection_tests {
    use super::*;

    /// Would Go's `json.Unmarshal` into `metav1.ObjectMeta` accept this?
    pub(crate) fn go_object_meta(meta: &Value) -> Result<(), String> {
        let m = meta.as_object().ok_or("metadata is not an object")?;
        let rfc3339 = |v: &Value| {
            v.is_null() || v.as_str().is_some_and(|s| chrono::DateTime::parse_from_rfc3339(s).is_ok())
        };
        let string_map = |v: &Value| v.is_null() || v.as_object().is_some_and(|o| o.values().all(Value::is_string));
        for (k, v) in m {
            let ok = match k.as_str() {
                "name" | "generateName" | "namespace" | "selfLink" | "uid" | "resourceVersion" => {
                    v.is_string() || v.is_null()
                }
                "generation" | "deletionGracePeriodSeconds" => v.is_i64() || v.is_u64() || v.is_null(),
                "creationTimestamp" | "deletionTimestamp" => rfc3339(v),
                "labels" | "annotations" => string_map(v),
                "finalizers" => v.is_null() || v.as_array().is_some_and(|a| a.iter().all(Value::is_string)),
                "ownerReferences" => v.is_null() || v.as_array().is_some_and(|a| a.iter().all(|r| {
                    r.as_object().is_some_and(|r| r.iter().all(|(k, v)| match k.as_str() {
                        "controller" | "blockOwnerDeletion" => v.is_boolean() || v.is_null(),
                        _ => v.is_string(),
                    }))
                })),
                "managedFields" => v.is_null() || v.as_array().is_some_and(|a| a.iter().all(|e| {
                    e.as_object().is_some_and(|e| e.iter().all(|(k, v)| match k.as_str() {
                        "time" => rfc3339(v),
                        "fieldsV1" => v.is_object() || v.is_null(),
                        _ => v.is_string() || v.is_null(),
                    }))
                })),
                _ => true, // unknown fields are ignored by encoding/json
            };
            if !ok {
                return Err(format!("metadata.{k} = {v} does not decode into ObjectMeta"));
            }
        }
        Ok(())
    }

    /// The frame as client-go's metadata watch decodes it.
    fn decodes(line: &str) -> Result<(String, Value), String> {
        let ev: Value = serde_json::from_str(line.trim_end()).map_err(|e| format!("not JSON: {e}"))?;
        let ty = ev["type"].as_str().ok_or("no type")?.to_string();
        let obj = ev["object"].clone();
        if ty == "ERROR" {
            return (obj["kind"] == "Status").then_some((ty, obj)).ok_or("ERROR without a Status".into());
        }
        if !["ADDED", "MODIFIED", "DELETED", "BOOKMARK"].contains(&ty.as_str()) {
            return Err(format!("unknown type {ty}"));
        }
        if obj["apiVersion"] != "meta.k8s.io/v1" || obj["kind"] != "PartialObjectMetadata" {
            return Err(format!("{ty} object is {} {}", obj["apiVersion"], obj["kind"]));
        }
        if let Some(extra) = obj.as_object().unwrap().keys().find(|k| !["apiVersion", "kind", "metadata"].contains(&k.as_str())) {
            return Err(format!("{ty} object carries {extra}"));
        }
        go_object_meta(&obj["metadata"])?;
        Ok((ty, obj))
    }

    fn crd(name: &str) -> Value {
        json!({
            "apiVersion": "apiextensions.k8s.io/v1", "kind": "CustomResourceDefinition",
            "metadata": {"name": name, "uid": "u-1", "creationTimestamp": "2026-10-01T00:00:00Z",
                         "generation": 1, "labels": {"app": "x"}, "annotations": {"a": "b"},
                         "finalizers": ["customresourcecleanup.apiextensions.k8s.io"],
                         "managedFields": [{"manager": "cilium-operator", "operation": "Update",
                                            "apiVersion": "apiextensions.k8s.io/v1",
                                            "time": "2026-10-01T00:00:00Z", "fieldsType": "FieldsV1",
                                            "fieldsV1": {"f:spec": {}}}]},
            "spec": {"group": "cilium.io"}, "status": {"conditions": []},
        })
    }
    const AV: &str = "meta.k8s.io/v1";
    const K: &str = "PartialObjectMetadata";

    #[test]
    fn every_metadata_only_frame_decodes_as_partial_object_metadata() {
        let key = "/registry/customresourcedefinitions/ciliumnodes.cilium.io".to_string();
        let bytes = serde_json::to_vec(&crd("ciliumnodes.cilium.io")).unwrap();
        let frames = [
            render_event(&WatchEvent::Added { key: key.clone(), value: bytes.clone(), revision: 7 },
                         &None, &None, AV, K, true, None).unwrap(),
            render_event(&WatchEvent::Modified { key: key.clone(), value: bytes.clone(), revision: 8,
                                                 prev_value: Some(bytes.clone()) },
                         &None, &None, AV, K, true, None).unwrap(),
            // The last state, from the watch cache (#100).
            render_event(&WatchEvent::Deleted { key: key.clone(), revision: 9, prev_value: Some(bytes.clone()) },
                         &None, &None, AV, K, true, None).unwrap(),
            // A tombstone from the key alone, below the cache's window.
            render_event(&WatchEvent::Deleted { key: key.clone(), revision: 9, prev_value: None },
                         &None, &None, AV, K, true, None).unwrap(),
            render_event(&WatchEvent::Bookmark { revision: 10 }, &None, &None, AV, K, true, None).unwrap(),
            render_bookmark(10, false, AV, K),
            render_bookmark(10, true, AV, K),
            render_initial_added(&crd("a.cilium.io"), &None, &None, AV, K, 6, true, None).unwrap(),
            render_event(&WatchEvent::Error { code: 410, message: "too old".into(), revision: 0 },
                         &None, &None, AV, K, true, None).unwrap(),
        ];
        for f in &frames {
            assert!(f.ends_with('\n') && !f[..f.len() - 1].contains('\n'), "one line per frame: {f:?}");
            if let Err(e) = decodes(f) {
                panic!("{e}: {f}");
            }
        }
        // The tombstone names the object and the delete's revision.
        let (_, t) = decodes(&frames[3]).unwrap();
        assert_eq!((t["metadata"]["name"].as_str(), t["metadata"]["resourceVersion"].as_str()),
                   (Some("ciliumnodes.cilium.io"), Some("9")));
        // The end-of-initial-events bookmark carries its annotation.
        let (_, b) = decodes(&frames[6]).unwrap();
        assert_eq!(b["metadata"]["annotations"]["k8s.io/initial-events-end"], "true");
    }

    /// The checker itself catches what Go would refuse.
    #[test]
    fn the_checker_refuses_what_go_refuses() {
        assert!(go_object_meta(&json!({"annotations": {"a": true}})).is_err());
        assert!(go_object_meta(&json!({"labels": {"n": 1}})).is_err());
        assert!(go_object_meta(&json!({"generation": "1"})).is_err());
        assert!(go_object_meta(&json!({"creationTimestamp": "yesterday"})).is_err());
        assert!(go_object_meta(&json!({"finalizers": "f"})).is_err());
        assert!(go_object_meta(&json!({"name": "x", "somethingNew": [1]})).is_ok());
        assert!(decodes("{\"type\":\"ADDED\",\"object\":{\"apiVersion\":\"apiextensions.k8s.io/v1\",\"kind\":\"CustomResourceDefinition\",\"metadata\":{}}}\n").is_err());
    }
}
