//! The scheduler's `/metrics` (upstream port 10259).
//!
//! The exporter itself is [`apimachinery::metrics`], shared with the
//! controller manager and the apiserver; what is here is the scheduler's own
//! metrics, under the names upstream gives them.

/// Install the recorder and serve `/metrics` + `/healthz` on `port`.
pub fn spawn(port: u16) {
    if let Some(handle) = apimachinery::metrics::install("kube-scheduler") {
        apimachinery::metrics::serve(port, handle, "scheduler");
    }
}

/// Increment the count of scheduling attempts with the given result
/// (`scheduled`, `unschedulable`, `error` — upstream's values).
pub fn record_attempt(result: &'static str) {
    metrics::counter!("scheduler_schedule_attempts_total", "result" => result).increment(1);
}

/// How long a pod waited from being seen to being bound.
pub fn record_e2e_latency(seconds: f64, result: &'static str) {
    metrics::histogram!("scheduler_e2e_scheduling_duration_seconds", "result" => result)
        .record(seconds);
}

/// How many pods are waiting to be scheduled.
///
/// Upstream splits this by queue (`active`, `backoff`, `unschedulable`); this
/// scheduler has one queue — the pods it found unscheduled this pass — so it
/// reports `active` and nothing else rather than inventing empty queues.
pub fn set_pending_pods(count: usize) {
    metrics::gauge!("scheduler_pending_pods", "queue" => "active").set(count as f64);
}

/// Whether this instance holds the scheduler lease.
pub fn set_leader(is_leader: bool) {
    apimachinery::metrics::set_leader("kube-scheduler", is_leader);
}
