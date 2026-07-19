//! Watch event streaming.
//!
//! Implements the K8s watch protocol: chunked JSON stream of WatchEvent
//! objects, each terminated by a newline.

use crate::selector;
use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use futures::StreamExt;
use apimachinery::watch::WatchEvent;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// Convert a WatchStream into an HTTP response with chunked JSON events.
/// Optionally filters events by label and field selectors.
pub fn watch_response(
    rx: mpsc::Receiver<WatchEvent>,
    label_selector: Option<String>,
    field_selector: Option<String>,
    api_version: String,
    kind: String,
) -> Response {
    let stream = ReceiverStream::new(rx).filter_map(move |event| {
        let label_sel = label_selector.clone();
        let field_sel = field_selector.clone();
        let api_version = api_version.clone();
        let kind = kind.clone();
        async move {
            let (event_type, object) = match &event {
                WatchEvent::Added { value, revision, .. } => {
                    let mut obj: serde_json::Value =
                        serde_json::from_slice(value).unwrap_or(json!({}));
                    inject_resource_version(&mut obj, *revision);
                    if !selector::matches_selectors(&obj, &label_sel, &field_sel) {
                        return None;
                    }
                    ("ADDED", obj)
                }
                WatchEvent::Modified { value, revision, .. } => {
                    let mut obj: serde_json::Value =
                        serde_json::from_slice(value).unwrap_or(json!({}));
                    inject_resource_version(&mut obj, *revision);
                    if !selector::matches_selectors(&obj, &label_sel, &field_sel) {
                        return None;
                    }
                    ("MODIFIED", obj)
                }
                WatchEvent::Deleted { revision, key, .. } => {
                    // The store doesn't carry the prior value on delete, so we
                    // synthesize the tombstone. It MUST carry apiVersion/kind:
                    // client-go refuses to decode a watch event whose object has
                    // no Kind ("unable to decode watch event: Object 'Kind' is
                    // missing"), which kills the whole informer stream.
                    let (namespace, name) = split_key(key);
                    let mut meta = json!({
                        "name": name,
                        "resourceVersion": revision.to_string()
                    });
                    if let Some(ns) = namespace {
                        meta["namespace"] = json!(ns);
                    }
                    let obj = json!({
                        "apiVersion": api_version,
                        "kind": kind,
                        "metadata": meta
                    });
                    ("DELETED", obj)
                }
                WatchEvent::Bookmark { revision } => {
                    // Bookmarks carry the watched type too, for the same reason.
                    let obj = json!({
                        "apiVersion": api_version,
                        "kind": kind,
                        "metadata": {
                            "resourceVersion": revision.to_string()
                        }
                    });
                    ("BOOKMARK", obj)
                }
            };

            // ADDED/MODIFIED come straight from storage and normally carry their
            // own TypeMeta, but backfill it if an object was stored without one.
            let mut object = object;
            if object.get("kind").and_then(|k| k.as_str()).is_none() {
                object["apiVersion"] = json!(api_version);
                object["kind"] = json!(kind);
            }

            let watch_event = json!({
                "type": event_type,
                "object": object
            });

            let mut line = serde_json::to_string(&watch_event).unwrap_or_default();
            line.push('\n');
            Some(Ok::<_, std::convert::Infallible>(line))
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json;stream=watch")
        .header("transfer-encoding", "chunked")
        .body(Body::from_stream(stream))
        .unwrap()
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
    }
}
