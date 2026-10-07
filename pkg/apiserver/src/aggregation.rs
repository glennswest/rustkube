//! API aggregation (#83): `apiregistration.k8s.io/v1` APIServices served
//! through this apiserver, as upstream's kube-aggregator does.
//!
//! - **The table.** Every replica follows the stored APIServices through the
//!   watch cache (a version check a second), so one registered anywhere is
//!   served everywhere within a second or so.
//! - **Proxying.** A request under `/apis/{group}/{version}` whose APIService
//!   names a `service` is proxied there once authentication and RBAC have
//!   passed — this apiserver authorizes it, as upstream's handler chain does,
//!   and the backend may authorize again. The backend is reached at the
//!   Service's ClusterIP, with TLS verified for `<name>.<namespace>.svc`
//!   against the APIService's `caBundle` (or not at all with
//!   `insecureSkipTLSVerify`). This apiserver presents
//!   `--proxy-client-cert-file` and says who asked in `X-Remote-User` /
//!   `X-Remote-Group` (the requestheader contract a backend learns from
//!   `kube-system/extension-apiserver-authentication`); the client's own
//!   `Authorization`, `Impersonate-*` and `X-Remote-*` headers are not
//!   forwarded. Responses stream, so a watch works. An APIService that
//!   is not `Available` answers 503. Built-in groups are never proxied.
//! - **Availability.** Each replica checks every APIService every few
//!   seconds and writes its `Available` condition when the answer changes:
//!   `Local` for one without a service, `ServiceNotFound`, or a discovery GET
//!   of `/apis/{group}/{version}` on the backend — `Passed` on a 2xx,
//!   `FailedDiscoveryCheck` otherwise.
//! - **Discovery.** Aggregated groups are in `/apis` and `/apis/{group}`;
//!   `/apis/{group}/{version}` is the backend's own answer, proxied.
//!
//! Not done: connection upgrades (exec-style) to a backend are refused with
//! 501, and aggregated discovery (#107) is not served.

use crate::auth::UserInfo;
use crate::error::ApiError;
use crate::storage::ResourceStorage;
use apimachinery::tls_reload::ReloadingKey;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

/// How often each replica checks the APIServices' backends.
const CHECK_INTERVAL: Duration = Duration::from_secs(5);
/// The largest request body passed to a backend.
const MAX_REQUEST_BODY: usize = 32 << 20;
/// How long a discovery check may take.
const CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// The Service an APIService proxies to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ServiceRef {
    pub namespace: String,
    pub name: String,
    pub port: u16,
}

/// One stored APIService, as the proxy needs it.
#[derive(Clone, Debug, PartialEq)]
pub struct ApiService {
    pub name: String,
    pub group: String,
    pub version: String,
    /// None: served locally (upstream's built-in and CRD APIServices).
    pub service: Option<ServiceRef>,
    pub ca_bundle: Option<Vec<u8>>,
    pub insecure: bool,
    pub group_priority: i64,
    pub version_priority: i64,
    /// Its `Available` condition is `True`.
    pub available: bool,
}

impl ApiService {
    pub fn parse(obj: &Value) -> Option<Self> {
        let spec = &obj["spec"];
        let group = spec["group"].as_str().filter(|g| !g.is_empty())?.to_string();
        let version = spec["version"].as_str().filter(|v| !v.is_empty())?.to_string();
        let service = spec["service"].as_object().map(|s| ServiceRef {
            namespace: s.get("namespace").and_then(Value::as_str).unwrap_or("default").to_string(),
            name: s.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
            port: s.get("port").and_then(Value::as_u64).and_then(|p| u16::try_from(p).ok()).unwrap_or(443),
        });
        let ca_bundle = spec["caBundle"].as_str().filter(|s| !s.is_empty()).and_then(|s| {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.decode(s).ok()
        });
        Some(Self {
            name: obj["metadata"]["name"].as_str().unwrap_or("").to_string(),
            group,
            version,
            service,
            ca_bundle,
            insecure: spec["insecureSkipTLSVerify"].as_bool().unwrap_or(false),
            group_priority: spec["groupPriorityMinimum"].as_i64().unwrap_or(0),
            version_priority: spec["versionPriority"].as_i64().unwrap_or(0),
            available: condition(obj, "Available").is_some_and(|c| c["status"] == "True"),
        })
    }
}

fn condition<'a>(obj: &'a Value, kind: &str) -> Option<&'a Value> {
    obj["status"]["conditions"].as_array()?.iter().find(|c| c["type"] == kind)
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct ClientKey {
    ca_bundle: Option<Vec<u8>>,
    insecure: bool,
    resolve: (String, SocketAddr),
}

/// The APIService table and the clients that reach the backends.
pub struct Aggregator {
    services: RwLock<Arc<Vec<ApiService>>>,
    builtin: HashSet<String>,
    clients: Mutex<HashMap<ClientKey, reqwest::Client>>,
    /// `--proxy-client-cert-file` / `--proxy-client-key-file`, followed.
    identity: Option<Arc<ReloadingKey>>,
}

impl Aggregator {
    pub fn new(identity: Option<Arc<ReloadingKey>>) -> Self {
        Self {
            services: RwLock::new(Arc::new(Vec::new())),
            builtin: crate::discovery::builtin_group_names(),
            clients: Mutex::new(HashMap::new()),
            identity,
        }
    }

    /// Replace the table with what is stored.
    pub fn set(&self, mut services: Vec<ApiService>) {
        services.sort_by(|a, b| a.name.cmp(&b.name));
        *self.services.write().unwrap() = Arc::new(services);
    }

    pub fn services(&self) -> Arc<Vec<ApiService>> {
        self.services.read().unwrap().clone()
    }

    /// The APIService that serves `path`: one with a service, for the group
    /// and version the path is under (`/apis/{group}/{version}` or below), in
    /// a group that is not built in.
    pub fn claims(&self, path: &str) -> Option<ApiService> {
        let rest = path.strip_prefix("/apis/")?;
        let mut parts = rest.splitn(3, '/');
        let group = parts.next().filter(|g| !g.is_empty())?;
        let version = parts.next().filter(|v| !v.is_empty())?;
        if self.builtin.contains(group) {
            return None;
        }
        self.services
            .read()
            .unwrap()
            .iter()
            .find(|s| s.service.is_some() && s.group == group && s.version == version)
            .cloned()
    }

    /// The aggregated groups as `/apis` lists them: each group's versions by
    /// `versionPriority`, highest first (the preferred one), groups by
    /// `groupPriorityMinimum`, highest first. Built-in groups are left out.
    pub fn groups(&self) -> Vec<Value> {
        let services = self.services();
        let mut by_group: Vec<(String, i64, Vec<&ApiService>)> = Vec::new();
        for s in services.iter().filter(|s| s.service.is_some() && !self.builtin.contains(&s.group)) {
            match by_group.iter_mut().find(|(g, _, _)| *g == s.group) {
                Some((_, p, v)) => {
                    *p = (*p).max(s.group_priority);
                    v.push(s);
                }
                None => by_group.push((s.group.clone(), s.group_priority, vec![s])),
            }
        }
        by_group.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        by_group
            .into_iter()
            .map(|(group, _, mut versions)| {
                versions.sort_by(|a, b| b.version_priority.cmp(&a.version_priority).then_with(|| a.version.cmp(&b.version)));
                let versions: Vec<Value> = versions
                    .iter()
                    .map(|s| json!({"groupVersion": format!("{group}/{}", s.version), "version": s.version}))
                    .collect();
                json!({"name": group, "versions": versions, "preferredVersion": versions[0]})
            })
            .collect()
    }

    /// Where a backend is, and a client that verifies it and presents the
    /// proxy identity.
    async fn endpoint(&self, storage: &ResourceStorage, api: &ApiService) -> Result<(String, reqwest::Client), Unreachable> {
        let s = api.service.as_ref().ok_or(Unreachable::Local)?;
        let svc = storage
            .get(&ResourceStorage::namespaced_key("services", &s.namespace, &s.name))
            .await
            .map_err(|e| match e.status {
                StatusCode::NOT_FOUND => Unreachable::NoService(format!("service/{} in \"{}\" is not present", s.name, s.namespace)),
                _ => Unreachable::Other(format!("service/{} in \"{}\": {}", s.name, s.namespace, e.message)),
            })?;
        let ip: IpAddr = svc["spec"]["clusterIP"]
            .as_str()
            .and_then(|ip| ip.parse().ok())
            .ok_or_else(|| Unreachable::Other(format!("service/{} in \"{}\" has no ClusterIP", s.name, s.namespace)))?;
        let host = format!("{}.{}.svc", s.name, s.namespace);
        let base = format!("https://{host}:{}", s.port);
        let key = ClientKey { ca_bundle: api.ca_bundle.clone(), insecure: api.insecure, resolve: (host, SocketAddr::new(ip, s.port)) };
        if let Some(c) = self.clients.lock().unwrap().get(&key) {
            return Ok((base, c.clone()));
        }
        // caBundle alone is trusted when given; none means the system roots.
        let tls = apimachinery::tls_reload::client_config_with(
            key.ca_bundle.as_deref(),
            key.ca_bundle.is_none(),
            key.insecure,
            self.identity.clone(),
        )
        .map_err(|e| Unreachable::Other(format!("caBundle: {e}")))?;
        let client = reqwest::Client::builder()
            .use_preconfigured_tls(tls)
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .resolve(&key.resolve.0, key.resolve.1)
            .build()
            .map_err(|e| Unreachable::Other(format!("client: {e}")))?;
        self.clients.lock().unwrap().insert(key, client.clone());
        Ok((base, client))
    }
}

enum Unreachable {
    Local,
    NoService(String),
    Other(String),
}

impl Unreachable {
    fn message(&self) -> &str {
        match self {
            Unreachable::Local => "served locally",
            Unreachable::NoService(m) | Unreachable::Other(m) => m,
        }
    }
}

/// Follow the stored APIServices into `aggregator`, as `follow_stored_crds`
/// follows CRDs: a version check of the watch cache a second.
pub async fn follow(storage: Arc<ResourceStorage>, aggregator: Arc<Aggregator>) {
    let prefix = ResourceStorage::cluster_prefix("apiservices");
    let cache = storage.watch_cache().clone();
    let mut seen = None;
    let mut complained = false;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tick.tick().await;
        let snap = match cache.version(&prefix).await {
            Ok(v) if Some(v) == seen => continue,
            Ok(_) => cache.snapshot(&prefix).await,
            Err(e) => Err(e),
        };
        match snap {
            Ok((version, items)) => {
                complained = false;
                let services = items
                    .iter()
                    .filter_map(|(_, b)| serde_json::from_slice::<Value>(b).ok())
                    .filter_map(|o| ApiService::parse(&o))
                    .collect();
                aggregator.set(services);
                seen = Some(version);
            }
            Err(e) if !complained => {
                tracing::warn!("aggregation: following APIServices: {e}; retrying");
                complained = true;
            }
            Err(_) => {}
        }
    }
}

/// Check every APIService's backend every few seconds and write its
/// `Available` condition when the answer changes.
pub async fn keep_available(storage: Arc<ResourceStorage>, aggregator: Arc<Aggregator>) {
    let mut tick = tokio::time::interval(CHECK_INTERVAL);
    loop {
        tick.tick().await;
        for api in aggregator.services().iter() {
            let (ok, reason, message) = check(&storage, &aggregator, api).await;
            write_available(&storage, &api.name, ok, reason, &message).await;
        }
    }
}

async fn check(storage: &ResourceStorage, aggregator: &Aggregator, api: &ApiService) -> (bool, &'static str, String) {
    let (base, client) = match aggregator.endpoint(storage, api).await {
        Ok(e) => e,
        Err(Unreachable::Local) => return (true, "Local", "Local APIServices are always available".into()),
        Err(Unreachable::NoService(m)) => return (false, "ServiceNotFound", m),
        Err(Unreachable::Other(m)) => return (false, "FailedDiscoveryCheck", m),
    };
    let url = format!("{base}/apis/{}/{}", api.group, api.version);
    match client.get(&url).timeout(CHECK_TIMEOUT).send().await {
        Ok(r) if r.status().is_success() => (true, "Passed", "all checks passed".into()),
        Ok(r) => (false, "FailedDiscoveryCheck", format!("failing or missing response from {url}: bad status from {url}: {}", r.status().as_u16())),
        Err(e) => (false, "FailedDiscoveryCheck", format!("failing or missing response from {url}: {}", error_chain(&e))),
    }
}

fn error_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        out.push_str(": ");
        out.push_str(&s.to_string());
        source = s.source();
    }
    out
}

/// The `status` an APIService gets for a check's answer: its `Available`
/// condition set, its transition time kept unless the status flips; None
/// when nothing would change.
pub(crate) fn available_status(stored: &Value, ok: bool, reason: &str, message: &str, now: &str) -> Option<Value> {
    let status = if ok { "True" } else { "False" };
    let old = condition(stored, "Available");
    if old.is_some_and(|c| c["status"] == status && c["reason"] == reason && c["message"] == message) {
        return None;
    }
    let since = old
        .filter(|c| c["status"] == status)
        .and_then(|c| c["lastTransitionTime"].as_str())
        .unwrap_or(now);
    let mut conditions: Vec<Value> = stored["status"]["conditions"]
        .as_array()
        .map(|c| c.iter().filter(|c| c["type"] != "Available").cloned().collect())
        .unwrap_or_default();
    conditions.push(json!({"type": "Available", "status": status, "reason": reason, "message": message, "lastTransitionTime": since}));
    let mut out = stored["status"].as_object().cloned().map(Value::Object).unwrap_or_else(|| json!({}));
    out["conditions"] = Value::Array(conditions);
    Some(out)
}

async fn write_available(storage: &ResourceStorage, name: &str, ok: bool, reason: &str, message: &str) {
    let key = ResourceStorage::cluster_key("apiservices", name);
    let Ok(mut stored) = storage.get(&key).await else { return };
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let Some(status) = available_status(&stored, ok, reason, message, &now) else { return };
    let rev = stored["metadata"]["resourceVersion"].as_str().and_then(|r| r.parse().ok());
    stored["status"] = status;
    match storage.update(&key, stored, rev).await {
        Ok(_) => tracing::info!(apiservice = name, available = ok, reason, "aggregation: availability changed"),
        // Another replica wrote it first; the next check compares again.
        Err(e) if e.reason == "Conflict" => {}
        Err(e) => tracing::warn!(apiservice = name, "aggregation: status not written: {}", e.message),
    }
}

/// Headers never forwarded either way: hop-by-hop, and the request's own
/// credentials and identity claims.
fn forwarded(name: &HeaderName, request: bool) -> bool {
    let n = name.as_str();
    let hop = matches!(
        n,
        "connection" | "keep-alive" | "proxy-authenticate" | "proxy-authorization" | "te" | "trailer"
            | "transfer-encoding" | "upgrade" | "host" | "content-length"
    );
    let identity = request
        && (n == "authorization" || n.starts_with("impersonate-") || n.starts_with("x-remote-"));
    !hop && !identity
}

/// The request headers sent to a backend: the client's, less what
/// [`forwarded`] drops, plus who asked.
pub(crate) fn proxy_headers(incoming: &HeaderMap, user: Option<&UserInfo>) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (k, v) in incoming {
        if forwarded(k, true) {
            out.append(k.clone(), v.clone());
        }
    }
    if let Some(u) = user {
        if let Ok(v) = HeaderValue::from_str(&u.username) {
            out.insert("x-remote-user", v);
        }
        for g in &u.groups {
            if let Ok(v) = HeaderValue::from_str(g) {
                out.append("x-remote-group", v);
            }
        }
    }
    out
}

/// The proxy, as a middleware inside authentication and RBAC: a request an
/// APIService claims goes to its backend, anything else on to the routes.
pub async fn proxy(State(state): State<crate::handlers::AppState>, req: Request, next: Next) -> Response {
    let Some(api) = state.aggregator.claims(req.uri().path()) else {
        return next.run(req).await;
    };
    if !api.available {
        return ApiError::unavailable("service unavailable").into_response();
    }
    if req.headers().contains_key(header::UPGRADE) {
        return ApiError {
            status: StatusCode::NOT_IMPLEMENTED,
            reason: "NotImplemented".into(),
            message: "connection upgrades to an aggregated API are not supported (rustkube#83)".into(),
            continue_token: None,
        }
        .into_response();
    }
    let (base, client) = match state.aggregator.endpoint(&state.storage, &api).await {
        Ok(e) => e,
        Err(e) => return ApiError::unavailable(&format!("error trying to reach service: {}", e.message())).into_response(),
    };
    let (parts, body) = req.into_parts();
    // Request bodies are objects, read whole; responses stream (a watch).
    let body = match axum::body::to_bytes(body, MAX_REQUEST_BODY).await {
        Ok(b) => b,
        Err(e) => return ApiError::bad_request(&format!("reading the request body: {e}")).into_response(),
    };
    let path = parts.uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let headers = proxy_headers(&parts.headers, parts.extensions.get::<UserInfo>());
    let upstream = client
        .request(parts.method.clone(), format!("{base}{path}"))
        .headers(headers)
        .body(body)
        .send()
        .await;
    let resp = match upstream {
        Ok(r) => r,
        Err(e) => {
            return ApiError::unavailable(&format!("error trying to reach service: {}", error_chain(&e))).into_response()
        }
    };
    let mut out = Response::builder().status(resp.status().as_u16());
    for (k, v) in resp.headers() {
        if forwarded(k, false) {
            out = out.header(k, v);
        }
    }
    out.body(Body::from_stream(resp.bytes_stream()))
        .unwrap_or_else(|e| ApiError::internal(&e.to_string()).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api(group: &str, version: &str, service: bool, gp: i64, vp: i64) -> Value {
        let mut spec = json!({"group": group, "version": version, "groupPriorityMinimum": gp, "versionPriority": vp,
                              "caBundle": "dGVzdA==", "insecureSkipTLSVerify": false});
        if service {
            spec["service"] = json!({"namespace": "kube-system", "name": "metrics-server", "port": 8443});
        }
        json!({"metadata": {"name": format!("{version}.{group}")}, "spec": spec,
               "status": {"conditions": [{"type": "Available", "status": "True"}]}})
    }

    #[test]
    fn an_apiservice_parses_as_upstream_spells_it() {
        let a = ApiService::parse(&api("metrics.k8s.io", "v1beta1", true, 100, 100)).unwrap();
        assert_eq!(a.service, Some(ServiceRef { namespace: "kube-system".into(), name: "metrics-server".into(), port: 8443 }));
        assert_eq!(a.ca_bundle.as_deref(), Some(&b"test"[..]));
        assert!(a.available);
        let mut no_status = api("x.io", "v1", true, 1, 1);
        no_status["status"] = Value::Null;
        assert!(!ApiService::parse(&no_status).unwrap().available, "no condition yet: not available");
        assert!(ApiService::parse(&json!({"spec": {"version": "v1"}})).is_none());
    }

    #[test]
    fn only_a_served_version_of_a_non_builtin_group_is_claimed() {
        let agg = Aggregator::new(None);
        agg.set(vec![
            ApiService::parse(&api("metrics.k8s.io", "v1beta1", true, 100, 100)).unwrap(),
            ApiService::parse(&api("local.example.com", "v1", false, 100, 100)).unwrap(),
            ApiService::parse(&api("apps", "v1", true, 100, 100)).unwrap(),
        ]);
        assert!(agg.claims("/apis/metrics.k8s.io/v1beta1").is_some());
        assert!(agg.claims("/apis/metrics.k8s.io/v1beta1/namespaces/a/pods").is_some());
        assert!(agg.claims("/apis/metrics.k8s.io").is_none(), "the group document is local");
        assert!(agg.claims("/apis/metrics.k8s.io/v1").is_none());
        assert!(agg.claims("/apis/local.example.com/v1/things").is_none(), "no service: local");
        assert!(agg.claims("/apis/apps/v1/deployments").is_none(), "built-in groups are never proxied");
        assert!(agg.claims("/api/v1/pods").is_none());
    }

    #[test]
    fn groups_list_by_priority_with_the_preferred_version_first() {
        let agg = Aggregator::new(None);
        agg.set(vec![
            ApiService::parse(&api("b.example.com", "v1beta1", true, 100, 9)).unwrap(),
            ApiService::parse(&api("b.example.com", "v1", true, 100, 15)).unwrap(),
            ApiService::parse(&api("a.example.com", "v1", true, 2000, 1)).unwrap(),
            ApiService::parse(&api("apps", "v9", true, 9999, 1)).unwrap(),
        ]);
        let g = agg.groups();
        assert_eq!(g.len(), 2);
        assert_eq!(g[0]["name"], "a.example.com");
        assert_eq!(g[1]["preferredVersion"]["version"], "v1");
        assert_eq!(g[1]["versions"][1]["groupVersion"], "b.example.com/v1beta1");
    }

    #[test]
    fn availability_is_written_only_when_it_changes() {
        let stored = json!({"status": {"conditions": [{"type": "Available", "status": "False",
            "reason": "FailedDiscoveryCheck", "message": "m", "lastTransitionTime": "t0"}]}});
        assert!(available_status(&stored, false, "FailedDiscoveryCheck", "m", "t1").is_none());
        let s = available_status(&stored, false, "ServiceNotFound", "n", "t1").unwrap();
        assert_eq!(s["conditions"][0]["lastTransitionTime"], "t0", "same status: transition time kept");
        let s = available_status(&stored, true, "Passed", "all checks passed", "t2").unwrap();
        assert_eq!((s["conditions"][0]["status"].as_str(), s["conditions"][0]["lastTransitionTime"].as_str()),
                   (Some("True"), Some("t2")));
        let s = available_status(&json!({}), true, "Local", "l", "t3").unwrap();
        assert_eq!(s["conditions"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn the_backend_hears_who_asked_and_never_the_clients_credentials() {
        let mut h = HeaderMap::new();
        h.insert("authorization", HeaderValue::from_static("Bearer secret"));
        h.insert("impersonate-user", HeaderValue::from_static("admin"));
        h.insert("x-remote-user", HeaderValue::from_static("forged"));
        h.insert("x-remote-group", HeaderValue::from_static("system:masters"));
        h.insert("accept", HeaderValue::from_static("application/json"));
        h.insert("connection", HeaderValue::from_static("keep-alive"));
        let user = UserInfo { username: "alice".into(), groups: vec!["dev".into(), "system:authenticated".into()] };
        let out = proxy_headers(&h, Some(&user));
        assert!(out.get("authorization").is_none() && out.get("impersonate-user").is_none() && out.get("connection").is_none());
        assert_eq!(out.get("x-remote-user").unwrap(), "alice");
        let groups: Vec<_> = out.get_all("x-remote-group").iter().map(|v| v.to_str().unwrap()).collect();
        assert_eq!(groups, ["dev", "system:authenticated"], "the forged group is gone");
        assert_eq!(out.get("accept").unwrap(), "application/json");
    }
}
