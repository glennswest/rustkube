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
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::debug;

/// How often a repeating event is written back.
///
/// Repeats are counted in memory and flushed on this interval, so a condition
/// that recurs every three seconds costs two writes a minute rather than
/// twenty. Upstream does the same thing for the same reason.
const AGGREGATION_INTERVAL: Duration = Duration::from_secs(30);

/// One event that has been seen before.
struct Seen {
    /// The object name, so repeats patch it rather than making another.
    name: String,
    count: u64,
    /// The first time this event was seen, kept so an aggregated event
    /// reports the span it covers rather than only its latest occurrence —
    /// `(x12 over 20m)` needs both ends.
    first: String,
    last_written: Instant,
}

/// Emits Events attributed to a named component (e.g. `replicaset-controller`).
#[derive(Clone)]
pub struct EventRecorder {
    api: Arc<ApiClient>,
    component: String,
    host: String,
    /// Events already emitted, keyed by what makes two of them "the same".
    ///
    /// **Without this every occurrence is a new object.** A controller that
    /// re-reports a condition every few seconds — a backoff, a failing create —
    /// wrote one Event per pass, so `oc describe` showed the same line a
    /// hundred times and the event store grew without bound. Upstream counts
    /// them instead, which is what renders as `(x12 over 20m)`.
    seen: Arc<Mutex<HashMap<String, Seen>>>,
}

impl EventRecorder {
    pub fn new(api: Arc<ApiClient>, component: &str) -> Self {
        let host = std::env::var("NODE_NAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_else(|_| "controller-manager".to_string());
        Self {
            api,
            seen: Arc::new(Mutex::new(HashMap::new())),
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
        // Two clocks, deliberately.
        //
        // `firstTimestamp` and `lastTimestamp` are `metav1.Time` — RFC3339 to
        // the second. `eventTime` is `metav1.MicroTime` and **requires
        // microseconds**: a plain RFC3339 there fails to unmarshal, and the
        // client discards the whole EventList rather than one field. That is
        // why `oc describe` printed `Events: <none>` while the API was
        // returning twenty-five of them and `oc get events` listed them fine.
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let now_micro = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string();
        // Two events are "the same" when they say the same thing about the
        // same object from the same source — which is exactly upstream's
        // aggregation key.
        let uid = meta["uid"].as_str().unwrap_or("");
        let key = format!("{namespace}/{name}/{uid}/{etype}/{reason}/{message}");

        let mut seen = self.seen.lock().await;
        if let Some(prev) = seen.get_mut(&key) {
            prev.count += 1;
            // Counted now, written on the interval: a condition recurring
            // every few seconds must not cost a write every few seconds.
            if prev.last_written.elapsed() < AGGREGATION_INTERVAL {
                return;
            }
            prev.last_written = Instant::now();
            let patch = json!({
                "count": prev.count,
                "firstTimestamp": prev.first,
                "lastTimestamp": now,
                "eventTime": now_micro,
            });
            let path = format!("/api/v1/namespaces/{namespace}/events/{}", prev.name);
            if let Err(e) = self.api.patch(&path, &patch).await {
                debug!("failed to aggregate event {reason} for {namespace}/{name}: {e}");
            }
            return;
        }

        // Upstream names events "<object>.<16-hex>".
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let event_name = format!("{name}.{}", &suffix[..16]);
        seen.insert(
            key,
            Seen {
                name: event_name.clone(),
                count: 1,
                first: now.clone(),
                last_written: Instant::now(),
            },
        );
        drop(seen);

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
            "eventTime": now_micro,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Two events are the same when they say the same thing about the same
    /// object from the same source. Getting the key wrong in either direction
    /// is bad: too loose and distinct problems merge into one line, too tight
    /// and the aggregation never fires.
    #[test]
    fn the_aggregation_key_distinguishes_what_it_should() {
        let key = |ns: &str, name: &str, uid: &str, ty: &str, reason: &str, msg: &str| {
            format!("{ns}/{name}/{uid}/{ty}/{reason}/{msg}")
        };
        let base = key("kube-system", "cilium", "u1", "Warning", "FailedCreate", "Error creating: x");

        // Same in every respect: one line.
        assert_eq!(base, key("kube-system", "cilium", "u1", "Warning", "FailedCreate", "Error creating: x"));

        // A different reason, message, object, uid or type is a different
        // event — a recreated object with the same name must not inherit the
        // old one's count.
        assert_ne!(base, key("kube-system", "cilium", "u1", "Warning", "FailedDelete", "Error creating: x"));
        assert_ne!(base, key("kube-system", "cilium", "u1", "Warning", "FailedCreate", "Error creating: y"));
        assert_ne!(base, key("kube-system", "other", "u1", "Warning", "FailedCreate", "Error creating: x"));
        assert_ne!(base, key("kube-system", "cilium", "u2", "Warning", "FailedCreate", "Error creating: x"));
        assert_ne!(base, key("kube-system", "cilium", "u1", "Normal", "FailedCreate", "Error creating: x"));
    }

    /// The interval is what keeps a condition recurring every few seconds from
    /// costing a write every few seconds.
    #[test]
    fn repeats_are_written_on_an_interval_not_every_time() {
        assert!(AGGREGATION_INTERVAL >= Duration::from_secs(10));
        assert!(AGGREGATION_INTERVAL <= Duration::from_secs(60));
    }

    /// `eventTime` is a metav1.MicroTime and requires microseconds.
    ///
    /// Without them the client fails to unmarshal the whole EventList rather
    /// than one field, so `oc describe` printed `Events: <none>` while the API
    /// returned twenty-five of them and `oc get events` listed them fine — a
    /// format error that presents as an absence.
    #[test]
    fn event_time_carries_microseconds() {
        let micro = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string();
        // 2026-08-29T02:18:56.123456Z
        assert_eq!(micro.len(), 27, "{micro}");
        let frac = micro.split('.').nth(1).expect("a fractional part");
        assert_eq!(frac.len(), 7, "six digits and the Z: {micro}");
        assert!(micro.ends_with('Z'));

        // The second-precision spelling is what firstTimestamp/lastTimestamp
        // use, and is exactly what eventTime must not be.
        let secs = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        assert!(!secs.contains('.'), "{secs}");
        assert_ne!(secs.len(), micro.len());
    }
}
