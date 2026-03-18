//! Watch event streaming.
//!
//! Implements the K8s watch protocol: chunked JSON stream of WatchEvent
//! objects, each terminated by a newline.

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use futures::StreamExt;
use rk_core::watch::WatchEvent;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// Convert a WatchStream into an HTTP response with chunked JSON events.
pub fn watch_response(rx: mpsc::Receiver<WatchEvent>) -> Response {
    let stream = ReceiverStream::new(rx).map(|event| {
        let (event_type, object) = match &event {
            WatchEvent::Added { value, revision, .. } => {
                let mut obj: serde_json::Value =
                    serde_json::from_slice(value).unwrap_or(json!({}));
                inject_resource_version(&mut obj, *revision);
                ("ADDED", obj)
            }
            WatchEvent::Modified { value, revision, .. } => {
                let mut obj: serde_json::Value =
                    serde_json::from_slice(value).unwrap_or(json!({}));
                inject_resource_version(&mut obj, *revision);
                ("MODIFIED", obj)
            }
            WatchEvent::Deleted { revision, key, .. } => {
                // For deletes, we may not have the full object
                let obj = json!({
                    "metadata": {
                        "resourceVersion": revision.to_string(),
                        "name": key.rsplit('/').next().unwrap_or("")
                    }
                });
                ("DELETED", obj)
            }
            WatchEvent::Bookmark { revision } => {
                let obj = json!({
                    "metadata": {
                        "resourceVersion": revision.to_string()
                    }
                });
                ("BOOKMARK", obj)
            }
        };

        let watch_event = json!({
            "type": event_type,
            "object": object
        });

        let mut line = serde_json::to_string(&watch_event).unwrap_or_default();
        line.push('\n');
        Ok::<_, std::convert::Infallible>(line)
    });

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json;stream=watch")
        .header("transfer-encoding", "chunked")
        .body(Body::from_stream(stream))
        .unwrap()
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
        for pair in query.split('&') {
            let mut parts = pair.splitn(2, '=');
            let key = parts.next().unwrap_or("");
            let val = parts.next().unwrap_or("");
            match key {
                "watch" => params.watch = val == "true" || val == "1",
                "resourceVersion" => params.resource_version = val.parse().ok(),
                "limit" => params.limit = val.parse().ok(),
                "continue" => {
                    if !val.is_empty() {
                        params.continue_token = Some(val.to_string());
                    }
                }
                "labelSelector" => {
                    if !val.is_empty() {
                        params.label_selector = Some(val.to_string());
                    }
                }
                "fieldSelector" => {
                    if !val.is_empty() {
                        params.field_selector = Some(val.to_string());
                    }
                }
                _ => {}
            }
        }
        params
    }
}
