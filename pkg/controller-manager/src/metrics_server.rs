//! Metrics + health HTTP server for the controller manager.
//!
//! Upstream kube-controller-manager exposes `/metrics` and `/healthz` (on
//! :10257). We serve the Prometheus exposition format so ironprom can scrape it.

use axum::routing::get;
use axum::Router;
use metrics_exporter_prometheus::PrometheusHandle;

/// Install the Prometheus recorder and spawn a `/metrics` + `/healthz` server on
/// `port`. Call once at startup. Returns the handle (also captured by the route).
pub fn spawn(port: u16) -> Option<PrometheusHandle> {
    let handle = match metrics_exporter_prometheus::PrometheusBuilder::new().install_recorder() {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!("metrics recorder install failed: {e}");
            return None;
        }
    };
    metrics::gauge!("controller_manager_build_info", "version" => apimachinery::VERSION).set(1.0);

    let h = handle.clone();
    tokio::spawn(async move {
        let app = Router::new()
            .route("/healthz", get(|| async { "ok" }))
            .route(
                "/metrics",
                get(move || {
                    let h = h.clone();
                    async move { h.render() }
                }),
            );
        match tokio::net::TcpListener::bind(("0.0.0.0", port)).await {
            Ok(listener) => {
                tracing::info!("controller-manager metrics on :{port}/metrics");
                let _ = axum::serve(listener, app).await;
            }
            Err(e) => tracing::warn!("metrics server bind :{port} failed: {e}"),
        }
    });
    Some(handle)
}

/// Record whether this instance currently holds leadership (1) or not (0).
pub fn set_leader(is_leader: bool) {
    metrics::gauge!("controller_manager_leader").set(if is_leader { 1.0 } else { 0.0 });
}
