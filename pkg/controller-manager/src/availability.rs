//! When a pod counts as available (#379): upstream's `podutil.IsPodAvailable`.
//! A pod is available once its `Ready` condition has been True for the
//! owner's `spec.minReadySeconds`, counted from that condition's
//! `lastTransitionTime`; with `minReadySeconds` 0 it is available as soon as
//! it is Ready. A Ready pod with no `lastTransitionTime` is not available
//! while `minReadySeconds` is set, as upstream.
//!
//! A controller that finds a Ready pod not yet available asks to be run
//! again when the first such pod will be ([`requeue_until_available`]):
//! nothing else changes at that moment to wake it.

use chrono::{DateTime, Utc};
use serde_json::Value;
use std::time::Duration;

/// `spec.minReadySeconds` of a workload, 0 when unset or negative.
pub fn min_ready_seconds(obj: &Value) -> i64 {
    obj["spec"]["minReadySeconds"].as_i64().unwrap_or(0).max(0)
}

fn ready_condition(pod: &Value) -> Option<&Value> {
    pod["status"]["conditions"]
        .as_array()?
        .iter()
        .find(|c| c["type"] == "Ready")
}

/// `IsPodReady`: the Ready condition is True.
pub fn is_pod_ready(pod: &Value) -> bool {
    ready_condition(pod).is_some_and(|c| c["status"] == "True")
}

/// When a Ready pod becomes available, or `None` for a pod that is not
/// Ready, or Ready without a `lastTransitionTime` (and `min_ready` > 0).
fn available_at(pod: &Value, min_ready: i64) -> Option<DateTime<Utc>> {
    let c = ready_condition(pod).filter(|c| c["status"] == "True")?;
    let since = c["lastTransitionTime"]
        .as_str()
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&Utc));
    match since {
        Some(t) => Some(t + chrono::Duration::seconds(min_ready)),
        // Ready with no transition time: available at once only without a
        // minimum to count.
        None if min_ready == 0 => Some(DateTime::<Utc>::MIN_UTC),
        None => None,
    }
}

/// `IsPodAvailable(pod, minReadySeconds, now)`.
pub fn is_pod_available(pod: &Value, min_ready: i64, now: DateTime<Utc>) -> bool {
    if !is_pod_ready(pod) {
        return false;
    }
    if min_ready <= 0 {
        return true;
    }
    // Upstream: `lastTransitionTime + minReadySeconds` strictly before now.
    available_at(pod, min_ready).is_some_and(|at| at < now)
}

/// How long until the first of `pods` that is Ready but not yet available
/// becomes available; `None` when there is none.
pub fn next_available_in<'a>(
    pods: impl IntoIterator<Item = &'a Value>,
    min_ready: i64,
    now: DateTime<Utc>,
) -> Option<Duration> {
    if min_ready <= 0 {
        return None;
    }
    pods.into_iter()
        .filter(|p| is_pod_ready(p))
        .filter_map(|p| available_at(p, min_ready))
        .filter(|at| *at >= now)
        .min()
        // Just past the moment, since availability is strictly after it.
        .map(|at| (at - now).to_std().unwrap_or_default() + Duration::from_millis(100))
}

/// Run this reconcile again when the first Ready pod of `pods` becomes
/// available, so the counts and rollouts that wait on it move then.
pub fn requeue_until_available<'a>(pods: impl IntoIterator<Item = &'a Value>, min_ready: i64) {
    if let Some(d) = next_available_in(pods, min_ready, Utc::now()) {
        apimachinery::reactor::requeue_after(d);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pod(ready: &str, since: Option<&str>) -> Value {
        let mut c = json!({"type": "Ready", "status": ready});
        if let Some(t) = since {
            c["lastTransitionTime"] = json!(t);
        }
        json!({"status": {"conditions": [{"type": "PodScheduled", "status": "True"}, c]}})
    }

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn available_once_ready_for_min_ready_seconds() {
        let p = pod("True", Some("2026-11-25T10:00:00Z"));
        assert!(is_pod_available(&p, 0, at("2026-11-25T10:00:00Z")), "no minimum: Ready is available");
        assert!(!is_pod_available(&p, 10, at("2026-11-25T10:00:05Z")));
        assert!(!is_pod_available(&p, 10, at("2026-11-25T10:00:10Z")), "strictly after, as upstream");
        assert!(is_pod_available(&p, 10, at("2026-11-25T10:00:10.5Z")));
        assert!(!is_pod_available(&pod("False", Some("2026-11-25T09:00:00Z")), 0, at("2026-11-25T10:00:00Z")));
        assert!(!is_pod_available(&json!({}), 0, Utc::now()), "no conditions: not ready");
    }

    #[test]
    fn ready_without_a_transition_time() {
        let p = pod("True", None);
        assert!(is_pod_available(&p, 0, Utc::now()));
        assert!(!is_pod_available(&p, 5, Utc::now()), "nothing to count from, as upstream");
        assert_eq!(next_available_in([&p], 5, Utc::now()), None, "and nothing to wait for");
    }

    #[test]
    fn the_wait_is_for_the_first_pod_to_become_available() {
        let now = at("2026-11-25T10:00:05Z");
        let a = pod("True", Some("2026-11-25T10:00:00Z")); // available at :30
        let b = pod("True", Some("2026-11-25T10:00:03Z")); // at :33
        let done = pod("True", Some("2026-11-25T09:00:00Z"));
        let not_ready = pod("False", Some("2026-11-25T10:00:04Z"));
        assert_eq!(next_available_in([&b, &a, &done, &not_ready], 30, now), Some(Duration::from_millis(25_100)));
        assert_eq!(next_available_in([&done, &not_ready], 30, now), None);
        assert_eq!(next_available_in([&a], 0, now), None, "no minimum, no wait");
        assert_eq!(min_ready_seconds(&json!({"spec": {"minReadySeconds": 7}})), 7);
        assert_eq!(min_ready_seconds(&json!({"spec": {}})), 0);
    }
}
