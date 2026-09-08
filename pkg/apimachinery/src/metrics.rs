//! The metrics every component exposes, under the names upstream uses.
//!
//! The names are the point. A dashboard, a recording rule or an alert written
//! against upstream Kubernetes should work here unchanged; an equivalent
//! metric under a different name is worth much less than the same metric under
//! the same name, because the thing that consumes it was not written for us
//! (#51).
//!
//! It lives here because all three components need it and each had grown its
//! own copy: `scheduler::metrics_server` and `controller_manager::metrics_server`
//! were the same thirty lines with a different port, and the apiserver had a
//! third spelling inline. They had already drifted — the controller-manager
//! reported leadership as `controller_manager_leader`, which no upstream
//! dashboard looks for.
//!
//! ## `process_*`
//!
//! Upstream gets these free from the Prometheus Go client, so every Kubernetes
//! dashboard assumes them and nothing in Rust provides them. They are read from
//! `/proc/self` on each scrape, which is the right time: a value sampled on a
//! timer is stale by up to the timer, and this costs a few file reads only when
//! someone actually asks.
//!
//! On anything but Linux the collector is absent rather than wrong — a
//! workstation build compiles and serves the rest.

use std::time::Duration;

/// Install the Prometheus recorder for this process.
///
/// Returns the handle used to render `/metrics`, or `None` if a recorder was
/// already installed (which is not an error worth failing a component over —
/// it means someone else is exporting).
pub fn install(component: &str) -> Option<metrics_exporter_prometheus::PrometheusHandle> {
    let handle = match metrics_exporter_prometheus::PrometheusBuilder::new().install_recorder() {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!("metrics recorder install failed: {e}");
            return None;
        }
    };
    // Upstream's name for "what is running here", carried by every component.
    metrics::gauge!(
        "kubernetes_build_info",
        "gitVersion" => crate::VERSION,
        "component" => component.to_string(),
        "goVersion" => "rustc",
    )
    .set(1.0);
    Some(handle)
}

/// Serve `/metrics` and `/healthz` on `port`.
///
/// Upstream ports: apiserver 6443 (on the API listener), controller-manager
/// 10257, scheduler 10259. Failure to bind is a warning, not a fatal error: a
/// component that cannot export is still a component that should run.
pub fn serve(port: u16, handle: metrics_exporter_prometheus::PrometheusHandle, component: &str) {
    let component = component.to_string();
    tokio::spawn(async move {
        let app = axum::Router::new()
            .route("/healthz", axum::routing::get(|| async { "ok" }))
            .route(
                "/metrics",
                axum::routing::get(move || {
                    let h = handle.clone();
                    async move {
                        refresh_process_metrics();
                        h.render()
                    }
                }),
            );
        match tokio::net::TcpListener::bind(("0.0.0.0", port)).await {
            Ok(listener) => {
                tracing::info!("{component} metrics on :{port}/metrics");
                let _ = axum::serve(listener, app).await;
            }
            Err(e) => tracing::warn!("{component} metrics bind :{port} failed: {e}"),
        }
    });
}

/// Whether this instance holds the lease, under upstream's name.
///
/// `leader_election_master_status{name}` is the metric that answers "who is
/// actually leading" — and the one that shows two instances both believing
/// they are, which is otherwise invisible until they fight.
pub fn set_leader(name: &str, held: bool) {
    metrics::gauge!("leader_election_master_status", "name" => name.to_string())
        .set(if held { 1.0 } else { 0.0 });
}

/// Refresh the `process_*` family from `/proc/self`.
#[cfg(target_os = "linux")]
pub fn refresh_process_metrics() {
    // USER_HZ. Configurable in principle, 100 on every Linux anyone runs; the
    // alternative is a libc dependency in a crate that has none.
    const TICKS_PER_SEC: f64 = 100.0;

    if let Ok(stat) = std::fs::read_to_string("/proc/self/stat") {
        // The comm field can contain spaces and parentheses, so fields are
        // counted from after the closing paren rather than by splitting the
        // whole line — the classic way to misparse /proc/self/stat.
        if let Some(rest) = stat.rsplit_once(") ").map(|(_, r)| r) {
            let f: Vec<&str> = rest.split_whitespace().collect();
            // After the comm and state fields, index 0 here is ppid (field 4).
            let get = |i: usize| f.get(i).and_then(|v| v.parse::<f64>().ok());
            if let (Some(utime), Some(stime)) = (get(11), get(12)) {
                metrics::gauge!("process_cpu_seconds_total")
                    .set((utime + stime) / TICKS_PER_SEC);
            }
            if let Some(starttime) = get(19) {
                if let Some(boot) = boot_time_secs() {
                    metrics::gauge!("process_start_time_seconds")
                        .set(boot + starttime / TICKS_PER_SEC);
                }
            }
        }
    }

    // VmRSS/VmSize are in kB and exact, which beats multiplying the `stat`
    // page counts by a page size this crate would have to guess (and would
    // guess wrong on a 64K-page arm64 kernel).
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if let Some(kb) = line.strip_prefix("VmRSS:") {
                if let Some(v) = kb.split_whitespace().next().and_then(|v| v.parse::<f64>().ok()) {
                    metrics::gauge!("process_resident_memory_bytes").set(v * 1024.0);
                }
            } else if let Some(kb) = line.strip_prefix("VmSize:") {
                if let Some(v) = kb.split_whitespace().next().and_then(|v| v.parse::<f64>().ok()) {
                    metrics::gauge!("process_virtual_memory_bytes").set(v * 1024.0);
                }
            }
        }
    }

    if let Ok(fds) = std::fs::read_dir("/proc/self/fd") {
        metrics::gauge!("process_open_fds").set(fds.count() as f64);
    }
    if let Ok(limits) = std::fs::read_to_string("/proc/self/limits") {
        for line in limits.lines() {
            if line.starts_with("Max open files") {
                if let Some(soft) = line.split_whitespace().nth(3) {
                    if let Ok(v) = soft.parse::<f64>() {
                        metrics::gauge!("process_max_fds").set(v);
                    }
                }
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub fn refresh_process_metrics() {
    // No /proc. Absent is the honest answer; a zero would be read as a fact.
}

#[cfg(target_os = "linux")]
fn boot_time_secs() -> Option<f64> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    stat.lines()
        .find_map(|l| l.strip_prefix("btime "))
        .and_then(|v| v.trim().parse::<f64>().ok())
}

/// Record how long a datastore round trip took, under upstream's name for it.
///
/// `etcd_request_duration_seconds{operation,type}` is what an alert on "the
/// store is slow" is written against, and the store being slow is the usual
/// reason an apiserver is slow.
pub fn record_store_request(operation: &'static str, resource: &str, seconds: f64) {
    metrics::histogram!(
        "etcd_request_duration_seconds",
        "operation" => operation,
        "type" => resource.to_string(),
    )
    .record(seconds);
}

/// A convenience for timing a store call.
pub struct StoreTimer {
    operation: &'static str,
    resource: String,
    started: std::time::Instant,
}

impl StoreTimer {
    pub fn new(operation: &'static str, resource: impl Into<String>) -> Self {
        Self {
            operation,
            resource: resource.into(),
            started: std::time::Instant::now(),
        }
    }
}

impl Drop for StoreTimer {
    fn drop(&mut self) {
        record_store_request(
            self.operation,
            &self.resource,
            self.started.elapsed().as_secs_f64(),
        );
    }
}

/// How long to keep serving after a shutdown signal — unused here, kept so the
/// module's callers have one place to look for exporter constants.
pub const SCRAPE_TIMEOUT_HINT: Duration = Duration::from_secs(10);

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn process_metrics_read_this_process() {
        // Not asserting values — asserting that parsing /proc/self does not
        // panic and finds the fields, which is the only way this breaks.
        refresh_process_metrics();
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        assert!(status.contains("VmRSS:"));
        let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
        let rest = stat.rsplit_once(") ").unwrap().1;
        let fields: Vec<&str> = rest.split_whitespace().collect();
        assert!(fields.len() > 19, "stat should have the fields we index");
        assert!(fields[11].parse::<u64>().is_ok(), "utime must parse");
    }

    #[test]
    fn a_command_name_with_spaces_does_not_shift_the_fields() {
        // /proc/self/stat's comm field is parenthesised and may contain spaces
        // and parens: `1234 (kube apiserver) S 1 ...`. Splitting the line on
        // whitespace shifts every field after it, which is the classic bug.
        let line = "1234 (kube (weird) name) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14";
        let rest = line.rsplit_once(") ").unwrap().1;
        let fields: Vec<&str> = rest.split_whitespace().collect();
        assert_eq!(fields[0], "S", "state comes first after the comm");
    }
}
