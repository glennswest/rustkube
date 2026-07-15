//! Metrics + health HTTP server for the scheduler (upstream :10259).
//! Serves Prometheus `/metrics` (scraped by ironprom) and `/healthz`.

use axum::routing::get;
use axum::Router;

/// Install the Prometheus recorder and spawn `/metrics` + `/healthz` on `port`.
pub fn spawn(port: u16) {
    let handle = match metrics_exporter_prometheus::PrometheusBuilder::new().install_recorder() {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!("metrics recorder install failed: {e}");
            return;
        }
    };
    metrics::gauge!("scheduler_build_info", "version" => apimachinery::VERSION).set(1.0);

    tokio::spawn(async move {
        let app = Router::new()
            .route("/healthz", get(|| async { "ok" }))
            .route(
                "/metrics",
                get(move || {
                    let h = handle.clone();
                    async move { h.render() }
                }),
            );
        match tokio::net::TcpListener::bind(("0.0.0.0", port)).await {
            Ok(listener) => {
                tracing::info!("scheduler metrics on :{port}/metrics");
                let _ = axum::serve(listener, app).await;
            }
            Err(e) => tracing::warn!("metrics server bind :{port} failed: {e}"),
        }
    });
}

/// Increment the count of scheduling attempts with the given result.
pub fn record_attempt(result: &'static str) {
    metrics::counter!("scheduler_schedule_attempts_total", "result" => result).increment(1);
}
