//! Event recorder.
//!
//! core/v1 Events are the API surface for "why did this happen" — failed
//! scheduling, image-pull errors, scaling actions. The resource was served but
//! nothing ever emitted an Event (#15), so `kubectl get events` / `kubectl
//! describe` were always empty. This posts properly-shaped Events for the
//! controllers' significant actions, matching upstream reasons
//! (SuccessfulCreate / SuccessfulDelete / ScalingReplicaSet / …).

use crate::runner::ApiClient;
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::debug;

/// Emits Events attributed to a named component (e.g. `replicaset-controller`).
#[derive(Clone)]
pub struct EventRecorder {
    api: Arc<ApiClient>,
    component: String,
    host: String,
}

impl EventRecorder {
    pub fn new(api: Arc<ApiClient>, component: &str) -> Self {
        let host = std::env::var("NODE_NAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_else(|_| "controller-manager".to_string());
        Self {
            api,
            component: component.to_string(),
            host,
        }
    }

    /// Record an Event about `involved` (a full object with metadata). `etype`
    /// is "Normal" or "Warning".
    pub async fn event(&self, involved: &Value, etype: &str, reason: &str, message: &str) {
        let meta = &involved["metadata"];
        let namespace = meta["namespace"].as_str().unwrap_or("default");
        let name = meta["name"].as_str().unwrap_or("");
        let now = chrono::Utc::now()
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        // Upstream names events "<object>.<16-hex>"; a uuid suffix keeps them
        // unique without an aggregation round-trip.
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let event_name = format!("{name}.{}", &suffix[..16]);

        let event = json!({
            "apiVersion": "v1",
            "kind": "Event",
            "metadata": { "name": event_name, "namespace": namespace },
            "involvedObject": {
                "apiVersion": involved["apiVersion"],
                "kind": involved["kind"],
                "namespace": namespace,
                "name": name,
                "uid": meta["uid"],
            },
            "reason": reason,
            "message": message,
            "type": etype,
            "source": { "component": self.component, "host": self.host },
            "reportingComponent": self.component,
            "reportingInstance": self.host,
            "firstTimestamp": now,
            "lastTimestamp": now,
            "eventTime": now,
            "count": 1,
        });

        if let Err(e) = self
            .api
            .create(&format!("/api/v1/namespaces/{namespace}/events"), &event)
            .await
        {
            debug!("failed to record event {reason} for {namespace}/{name}: {e}");
        }
    }
}
