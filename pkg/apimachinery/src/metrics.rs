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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Prometheus client_golang's `DefBuckets`, for every histogram upstream
/// gives no buckets of its own.
const DEFAULT_BUCKETS: &[f64] = &[0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0];
/// Upstream's buckets for `apiserver_request_duration_seconds` and
/// `etcd_request_duration_seconds`.
const REQUEST_BUCKETS: &[f64] = &[
    0.005, 0.025, 0.05, 0.1, 0.2, 0.4, 0.6, 0.8, 1.0, 1.25, 1.5, 2.0, 3.0, 4.0, 5.0, 6.0, 8.0, 10.0, 15.0,
    20.0, 30.0, 45.0, 60.0,
];

/// `count` buckets from `start`, each `factor` times the last (client_golang's
/// `ExponentialBuckets`).
fn exponential(start: f64, factor: f64, count: usize) -> Vec<f64> {
    (0..count).map(|i| start * factor.powi(i as i32)).collect()
}

/// The exporter with buckets on every histogram (#90). Without them
/// metrics-exporter-prometheus renders each histogram as a summary — no
/// `_bucket` series — and `histogram_quantile(…_bucket)`, which every upstream
/// dashboard and alert uses, returns nothing.
pub fn builder() -> metrics_exporter_prometheus::PrometheusBuilder {
    use metrics_exporter_prometheus::{Matcher, PrometheusBuilder};
    let full = |name: &str| Matcher::Full(name.to_string());
    PrometheusBuilder::new()
        .set_buckets(DEFAULT_BUCKETS)
        .and_then(|b| b.set_buckets_for_metric(full("apiserver_request_duration_seconds"), REQUEST_BUCKETS))
        .and_then(|b| b.set_buckets_for_metric(full("etcd_request_duration_seconds"), REQUEST_BUCKETS))
        .and_then(|b| {
            b.set_buckets_for_metric(full("scheduler_e2e_scheduling_duration_seconds"), &exponential(0.001, 2.0, 15))
        })
        .expect("bucket lists are non-empty")
}

/// `/metrics` as served: the `process_*` family refreshed, and
/// `process_cpu_seconds_total` typed a counter, as client_golang types it —
/// the exporter's counters are integers, so it is recorded as a gauge (#90).
pub fn render(handle: &metrics_exporter_prometheus::PrometheusHandle) -> String {
    refresh_process_metrics();
    handle
        .render()
        .replace("# TYPE process_cpu_seconds_total gauge", "# TYPE process_cpu_seconds_total counter")
}

/// Install the Prometheus recorder for this process.
///
/// Returns the handle used to render `/metrics`, or `None` if a recorder was
/// already installed (which is not an error worth failing a component over —
/// it means someone else is exporting).
pub fn install(component: &str) -> Option<metrics_exporter_prometheus::PrometheusHandle> {
    let handle = match builder().install_recorder() {
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

/// Paths a metrics listener serves to anyone: upstream's
/// `--authorization-always-allow-paths` default. A trailing `*` is a prefix.
pub const ALWAYS_ALLOW: &[&str] = &["/healthz", "/readyz", "/livez"];

/// How a component's own metrics listener is served (#90): TLS from a
/// stormcert pair, and everything but the always-allowed paths behind the
/// apiserver's TokenReview and SubjectAccessReview, as upstream's
/// kube-controller-manager and kube-scheduler delegate.
#[derive(Clone, Default)]
pub struct Serving {
    /// Certificate and key files; followed on disk. None: plain HTTP.
    pub tls: Option<(PathBuf, PathBuf)>,
    /// None: nothing is checked (tests and a component with no apiserver).
    pub auth: Option<Arc<DelegatedAuth>>,
    pub always_allow: Vec<String>,
}

fn allowed_path(patterns: &[String], path: &str) -> bool {
    patterns.iter().any(|p| match p.strip_suffix('*') {
        Some(prefix) => path.starts_with(prefix),
        None => p == path,
    })
}

/// What the apiserver said about a bearer token for a path.
#[derive(Clone, Debug, PartialEq)]
pub enum Decision {
    Allowed,
    Unauthenticated,
    Forbidden(String),
}

/// Authentication and authorization delegated to the apiserver: a
/// TokenReview for who the bearer is, a SubjectAccessReview for whether they
/// may `get` the path. Answers are kept 10 s, as upstream caches them.
pub struct DelegatedAuth {
    client: reqwest::Client,
    base: String,
    cache: Mutex<HashMap<(String, String), (Instant, Decision)>>,
}

const AUTH_CACHE_TTL: Duration = Duration::from_secs(10);

impl DelegatedAuth {
    /// `client` already carries this component's own credentials.
    pub fn new(client: reqwest::Client, base: &str) -> Arc<Self> {
        Arc::new(Self { client, base: base.trim_end_matches('/').to_string(), cache: Mutex::new(HashMap::new()) })
    }

    pub async fn check(&self, token: &str, path: &str) -> Result<Decision, String> {
        let key = (token.to_string(), path.to_string());
        if let Some((at, d)) = self.cache.lock().unwrap().get(&key) {
            if at.elapsed() < AUTH_CACHE_TTL {
                return Ok(d.clone());
            }
        }
        let decision = self.ask(token, path).await?;
        let mut cache = self.cache.lock().unwrap();
        if cache.len() > 1000 {
            cache.clear();
        }
        cache.insert(key, (Instant::now(), decision.clone()));
        Ok(decision)
    }

    async fn post(&self, path: &str, body: serde_json::Value) -> Result<serde_json::Value, String> {
        let resp = self.client.post(format!("{}{path}", self.base)).json(&body).send().await.map_err(|e| e.to_string())?;
        let status = resp.status();
        let out: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!("{path}: {status} {}", out["message"].as_str().unwrap_or("")));
        }
        Ok(out)
    }

    async fn ask(&self, token: &str, path: &str) -> Result<Decision, String> {
        let review = self
            .post("/apis/authentication.k8s.io/v1/tokenreviews", serde_json::json!({
                "apiVersion": "authentication.k8s.io/v1", "kind": "TokenReview", "spec": {"token": token}}))
            .await?;
        if review["status"]["authenticated"] != true {
            return Ok(Decision::Unauthenticated);
        }
        let user = &review["status"]["user"];
        let sar = self
            .post("/apis/authorization.k8s.io/v1/subjectaccessreviews", serde_json::json!({
                "apiVersion": "authorization.k8s.io/v1", "kind": "SubjectAccessReview",
                "spec": {"user": user["username"], "groups": user["groups"], "uid": user["uid"], "extra": user["extra"],
                         "nonResourceAttributes": {"path": path, "verb": "get"}}}))
            .await?;
        Ok(if sar["status"]["allowed"] == true {
            Decision::Allowed
        } else {
            Decision::Forbidden(user["username"].as_str().unwrap_or("").to_string())
        })
    }
}

async fn guard(
    axum::extract::State(serving): axum::extract::State<Arc<Serving>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    let path = req.uri().path().to_string();
    let Some(auth) = serving.auth.as_ref().filter(|_| !allowed_path(&serving.always_allow, &path)) else {
        return next.run(req).await;
    };
    let token = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.split_once(' ').filter(|(s, t)| s.eq_ignore_ascii_case("bearer") && !t.is_empty()).map(|(_, t)| t.to_string()));
    let Some(token) = token else {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    };
    match auth.check(&token, &path).await {
        Ok(Decision::Allowed) => next.run(req).await,
        Ok(Decision::Unauthenticated) => (StatusCode::UNAUTHORIZED, "Unauthorized").into_response(),
        Ok(Decision::Forbidden(user)) => {
            (StatusCode::FORBIDDEN, format!("forbidden: User \"{user}\" cannot get path \"{path}\"")).into_response()
        }
        Err(e) => {
            tracing::warn!("metrics: delegated authorization failed: {e}");
            (StatusCode::SERVICE_UNAVAILABLE, "authorization unavailable").into_response()
        }
    }
}

/// The router a component's metrics listener serves.
pub fn router(handle: metrics_exporter_prometheus::PrometheusHandle, serving: Arc<Serving>) -> axum::Router {
    let ok = || async { "ok" };
    axum::Router::new()
        .route("/healthz", axum::routing::get(ok))
        .route("/readyz", axum::routing::get(ok))
        .route("/livez", axum::routing::get(ok))
        .route(
            "/metrics",
            axum::routing::get(move || {
                let h = handle.clone();
                async move { render(&h) }
            }),
        )
        .layer(axum::middleware::from_fn_with_state(serving, guard))
}

/// Serve `/metrics`, `/healthz`, `/readyz` and `/livez` on `port`.
///
/// Upstream ports: apiserver 6443 (on the API listener), controller-manager
/// 10257, scheduler 10259. Failure to bind is a warning, not a fatal error: a
/// component that cannot export is still a component that should run. With
/// `serving.tls` the port speaks HTTPS only, from a certificate followed on
/// disk (#105's reload); the files are waited for if not there yet.
pub fn serve(port: u16, handle: metrics_exporter_prometheus::PrometheusHandle, component: &str, serving: Serving) {
    let component = component.to_string();
    tokio::spawn(async move {
        let tls = serving.tls.clone();
        if serving.auth.is_none() {
            tracing::warn!("{component} metrics: no delegated authorization; /metrics is open");
        }
        let app = router(handle, Arc::new(serving));
        let listener = match tokio::net::TcpListener::bind(("0.0.0.0", port)).await {
            Ok(l) => l,
            Err(e) => return tracing::warn!("{component} metrics bind :{port} failed: {e}"),
        };
        let Some((cert, key)) = tls else {
            tracing::warn!("{component} metrics on http://:{port} (no --tls-cert-file: plain HTTP)");
            let _ = axum::serve(listener, app).await;
            return;
        };
        let pair = loop {
            match (std::fs::read(&cert), std::fs::read(&key)) {
                (Ok(c), Ok(k)) => match crate::tls_reload::ReloadingKey::from_pem(&c, &k) {
                    Ok(pair) => break pair,
                    Err(e) => tracing::warn!("{component} metrics: serving certificate: {e}; retrying"),
                },
                _ => tracing::warn!("{component} metrics: waiting for {} and {}", cert.display(), key.display()),
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        };
        pair.watch("metrics serving certificate", cert, key, |_| {});
        let config = match rustls::ServerConfig::builder_with_provider(crate::tls_reload::provider())
            .with_safe_default_protocol_versions()
        {
            Ok(b) => {
                let mut c = b.with_no_client_auth().with_cert_resolver(pair);
                c.alpn_protocols = vec![b"http/1.1".to_vec()];
                Arc::new(c)
            }
            Err(e) => return tracing::error!("{component} metrics: TLS: {e}"),
        };
        tracing::info!("{component} metrics on https://:{port}");
        let acceptor = tokio_rustls::TlsAcceptor::from(config);
        loop {
            let Ok((stream, _)) = listener.accept().await else { continue };
            let (acceptor, app) = (acceptor.clone(), app.clone());
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(stream).await else { return };
                let svc = hyper_util::service::TowerToHyperService::new(app);
                let _ = hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                    .serve_connection(hyper_util::rt::TokioIo::new(tls), svc)
                    .await;
            });
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

/// Count a read the apiserver answered from its watch cache instead of the
/// datastore (#171), labelled like `etcd_request_duration_seconds`: beside
/// that histogram's count it says how much of the read load the cache took.
pub fn record_cache_read(operation: &'static str, resource: &str) {
    metrics::counter!(
        "apiserver_watch_cache_reads_total",
        "operation" => operation,
        "type" => resource.to_string(),
    )
    .increment(1);
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

    #[test]
    fn histograms_have_buckets_upstream_dashboards_can_use() {
        let recorder = builder().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            metrics::histogram!("apiserver_request_duration_seconds", "verb" => "GET").record(0.3);
            metrics::histogram!("scheduler_e2e_scheduling_duration_seconds").record(0.003);
            metrics::histogram!("controller_reconcile_duration_seconds").record(0.02);
        });
        let text = handle.render();
        assert!(!text.contains("quantile="), "no summaries:\n{text}");
        assert!(text.contains("apiserver_request_duration_seconds_bucket{verb=\"GET\",le=\"0.4\"} 1"), "{text}");
        assert!(text.contains("apiserver_request_duration_seconds_bucket{verb=\"GET\",le=\"0.2\"} 0"), "{text}");
        assert!(text.contains("scheduler_e2e_scheduling_duration_seconds_bucket{le=\"0.004\"} 1"), "{text}");
        assert!(text.contains("controller_reconcile_duration_seconds_bucket{le=\"0.025\"} 1"), "{text}");
    }

    #[test]
    fn always_allowed_paths_match_exactly_or_by_star_prefix() {
        let p: Vec<String> = ["/healthz", "/debug/*"].iter().map(|s| s.to_string()).collect();
        assert!(allowed_path(&p, "/healthz"));
        assert!(allowed_path(&p, "/debug/pprof"));
        assert!(!allowed_path(&p, "/metrics"));
        assert!(!allowed_path(&p, "/healthzx"));
    }

    #[tokio::test]
    async fn metrics_need_a_token_the_apiserver_accepts_and_health_does_not() {
        // A stand-in apiserver: token "good" is alice, allowed; "nosy" is bob,
        // not allowed; anything else unauthenticated.
        let api = axum::Router::new()
            .route("/apis/authentication.k8s.io/v1/tokenreviews", axum::routing::post(|axum::Json(b): axum::Json<serde_json::Value>| async move {
                let user = match b["spec"]["token"].as_str() { Some("good") => "alice", Some("nosy") => "bob", _ => "" };
                axum::Json(serde_json::json!({"status": {"authenticated": !user.is_empty(), "user": {"username": user, "groups": []}}}))
            }))
            .route("/apis/authorization.k8s.io/v1/subjectaccessreviews", axum::routing::post(|axum::Json(b): axum::Json<serde_json::Value>| async move {
                let ok = b["spec"]["user"] == "alice" && b["spec"]["nonResourceAttributes"]["path"] == "/metrics";
                axum::Json(serde_json::json!({"status": {"allowed": ok}}))
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, api).await.unwrap() });
        let auth = DelegatedAuth::new(reqwest::Client::new(), &format!("http://{addr}"));
        let serving = Arc::new(Serving { tls: None, auth: Some(auth), always_allow: ALWAYS_ALLOW.iter().map(|s| s.to_string()).collect() });
        let handle = builder().build_recorder().handle();
        let app = router(handle, serving);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let me = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::new();
        let get = |path: &str, token: Option<&str>| {
            let mut r = client.get(format!("http://{me}{path}"));
            if let Some(t) = token { r = r.bearer_auth(t); }
            r.send()
        };
        assert_eq!(get("/healthz", None).await.unwrap().status(), 200);
        assert_eq!(get("/livez", None).await.unwrap().status(), 200);
        assert_eq!(get("/metrics", None).await.unwrap().status(), 401);
        assert_eq!(get("/metrics", Some("forged")).await.unwrap().status(), 401);
        assert_eq!(get("/metrics", Some("nosy")).await.unwrap().status(), 403);
        assert_eq!(get("/metrics", Some("good")).await.unwrap().status(), 200);
    }

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
