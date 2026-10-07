//! The scheduler's Events (#138): `Scheduled` when a Pod is bound and
//! `FailedScheduling` when no node fits, as upstream's default-scheduler
//! records them. `kubectl describe pod` reads them, and the conformance
//! suite's SchedulerPredicates specs wait for them.
//!
//! No aggregation is needed here: `FailedScheduling` is written only when
//! the Pod's `PodScheduled=False` message changes (the caller's check), and
//! `Scheduled` once per bind. The event controller's TTL removes them.

use serde_json::{json, Value};

/// The component every scheduler Event names, as upstream's.
pub const COMPONENT: &str = "default-scheduler";

/// The Event for `pod` (needs metadata.name/namespace/uid). `etype` is
/// `Normal` or `Warning`; `action` is upstream's (`Binding`, `Scheduling`).
pub fn event(pod: &Value, etype: &str, reason: &str, action: &str, message: &str) -> Value {
    let meta = &pod["metadata"];
    let namespace = meta["namespace"].as_str().unwrap_or("default");
    let name = meta["name"].as_str().unwrap_or("");
    let host = std::env::var("NODE_NAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| COMPONENT.to_string());
    let now = chrono::Utc::now();
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    json!({
        "apiVersion": "v1",
        "kind": "Event",
        "metadata": {"name": format!("{name}.{}", &suffix[..16]), "namespace": namespace},
        "involvedObject": {"apiVersion": "v1", "kind": "Pod", "namespace": namespace,
                           "name": name, "uid": meta["uid"]},
        "reason": reason,
        "message": message,
        "type": etype,
        "action": action,
        "count": 1,
        "firstTimestamp": now.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "lastTimestamp": now.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        // metav1.MicroTime: microseconds, or clients drop the whole list.
        "eventTime": now.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string(),
        "source": {"component": COMPONENT, "host": host},
        "reportingComponent": COMPONENT,
        "reportingInstance": host,
    })
}

/// `Successfully assigned ns/pod to node`, upstream's words.
pub fn scheduled(pod: &Value, node: &str) -> Value {
    let ns = pod["metadata"]["namespace"].as_str().unwrap_or("default");
    let name = pod["metadata"]["name"].as_str().unwrap_or("");
    event(pod, "Normal", "Scheduled", "Binding", &format!("Successfully assigned {ns}/{name} to {node}"))
}

/// `0/N nodes are available: …`, the same message as `PodScheduled=False`.
pub fn failed_scheduling(pod: &Value, why: &str) -> Value {
    event(pod, "Warning", "FailedScheduling", "Scheduling", why)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_shaped_as_upstreams() {
        let pod = json!({"metadata": {"name": "web", "namespace": "demo", "uid": "u1"}});
        let s = scheduled(&pod, "node-a");
        assert_eq!((s["type"].as_str(), s["reason"].as_str()), (Some("Normal"), Some("Scheduled")));
        assert_eq!(s["message"], "Successfully assigned demo/web to node-a");
        assert_eq!(s["involvedObject"], json!({"apiVersion": "v1", "kind": "Pod", "namespace": "demo", "name": "web", "uid": "u1"}));
        assert_eq!((s["source"]["component"].as_str(), s["reportingComponent"].as_str()),
                   (Some(COMPONENT), Some(COMPONENT)));
        assert!(s["metadata"]["name"].as_str().unwrap().starts_with("web."));
        assert_eq!(s["metadata"]["namespace"], "demo");
        // MicroTime has six fractional digits.
        let et = s["eventTime"].as_str().unwrap();
        assert_eq!(et.split('.').nth(1).map(|f| f.len()), Some(7), "{et}");
        let f = failed_scheduling(&pod, "0/2 nodes are available: 2 node(s) didn't match Pod's node affinity/selector.");
        assert_eq!((f["type"].as_str(), f["reason"].as_str(), f["action"].as_str()),
                   (Some("Warning"), Some("FailedScheduling"), Some("Scheduling")));
        assert!(f["message"].as_str().unwrap().starts_with("0/2 nodes are available"));
    }
}
