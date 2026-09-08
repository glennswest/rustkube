//! The controller manager's `/metrics` (upstream port 10257).
//!
//! The exporter itself is [`apimachinery::metrics`], shared with the scheduler
//! and the apiserver — this file is what is specific to the controller
//! manager, which is one gauge and the port number.

/// Install the recorder and serve `/metrics` + `/healthz` on `port`.
pub fn spawn(port: u16) -> Option<metrics_exporter_prometheus::PrometheusHandle> {
    let handle = apimachinery::metrics::install("kube-controller-manager")?;
    apimachinery::metrics::serve(port, handle.clone(), "controller-manager");
    Some(handle)
}

/// Record whether this instance currently holds leadership.
///
/// Under upstream's name (`leader_election_master_status{name}`), not the
/// `controller_manager_leader` this used to export, which nothing looks for.
/// It is the metric that shows two instances both believing they lead.
pub fn set_leader(is_leader: bool) {
    apimachinery::metrics::set_leader("kube-controller-manager", is_leader);
}

/// How long one controller's reconcile pass took, and whether it failed.
///
/// Deliberately **not** `workqueue_*`: these controllers are poll loops with
/// no queue, and exporting `workqueue_depth` as a constant zero would be a
/// number that reads as a fact. The shape differs from upstream, so the name
/// does too.
pub fn record_reconcile(controller: &'static str, seconds: f64, ok: bool) {
    metrics::histogram!("controller_reconcile_duration_seconds", "controller" => controller)
        .record(seconds);
    if !ok {
        metrics::counter!("controller_reconcile_errors_total", "controller" => controller)
            .increment(1);
    }
}
