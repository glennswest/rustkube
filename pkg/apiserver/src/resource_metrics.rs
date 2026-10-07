//! `metrics.k8s.io/v1beta1` — the resource metrics API — served from each
//! node's cadvisor (#89; owner's decision: "fix the metrics for rustkube, we
//! already have cadvisor"). What upstream gets from metrics-server behind
//! aggregation, this apiserver answers itself: `kubectl top node|pod`, and
//! the CPU/memory the HPA scales on.
//!
//! - **Where.** Every node runs cadvisor (stormcos: port 9096). A request
//!   reads `/api/v1.3/subcontainers/` from the node's InternalIP — every
//!   cgroup with its labels and its recent samples, in one call — and keeps
//!   the answer 10 s per node (metrics-server's resolution is 15 s).
//! - **Node usage** is the root cgroup's: CPU as the rate between its last
//!   two samples, memory as the last working set.
//! - **Pod usage** is the sum of the pod's containers, a container found by
//!   upstream's runtime labels on its cgroup: `io.kubernetes.pod.namespace`,
//!   `io.kubernetes.pod.name`, `io.kubernetes.container.name` (the sandbox,
//!   `POD`, is left out). containerd and CRI-O containers carry them;
//!   stormpump's carry none until cadvisor attributes them (cadvisor#3), and
//!   until then a pod has no metrics — a 404, as metrics-server answers for a
//!   pod it has not measured — rather than a made-up number.

use crate::error::ApiError;
use crate::handlers::AppState;
use crate::storage::ResourceStorage;
use axum::extract::{Path, RawQuery, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long one node's answer is reused.
const CACHE: Duration = Duration::from_secs(10);
const LABEL_NAMESPACE: &str = "io.kubernetes.pod.namespace";
const LABEL_POD: &str = "io.kubernetes.pod.name";
const LABEL_CONTAINER: &str = "io.kubernetes.container.name";

/// CPU in nanocores and memory in bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    pub cpu_nanocores: u64,
    pub memory_bytes: u64,
}

impl Usage {
    fn json(&self) -> Value {
        json!({"cpu": format!("{}n", self.cpu_nanocores), "memory": format!("{}Ki", self.memory_bytes / 1024)})
    }
}

/// What one node's cadvisor said.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NodeSample {
    /// The newest sample's time (RFC 3339) and the seconds the CPU rate spans.
    pub timestamp: String,
    pub window_secs: f64,
    pub node: Option<Usage>,
    /// (namespace, pod) → containers by name.
    pub pods: HashMap<(String, String), BTreeMap<String, Usage>>,
}

/// One cgroup's usage from its last two samples: the CPU rate between them,
/// the last working set, the last sample's time and the span.
fn usage_of(info: &Value) -> Option<(Usage, chrono::DateTime<chrono::FixedOffset>, f64)> {
    let stats = info["stats"].as_array()?;
    let [.., a, b] = stats.as_slice() else { return None };
    let ta = chrono::DateTime::parse_from_rfc3339(a["timestamp"].as_str()?).ok()?;
    let tb = chrono::DateTime::parse_from_rfc3339(b["timestamp"].as_str()?).ok()?;
    let dt = (tb - ta).num_nanoseconds()? as f64 / 1e9;
    if dt <= 0.0 {
        return None;
    }
    let (ca, cb) = (a["cpu"]["usage"]["total"].as_u64()?, b["cpu"]["usage"]["total"].as_u64()?);
    let cpu = (cb.saturating_sub(ca) as f64 / dt).round() as u64;
    let memory = b["memory"]["working_set"].as_u64().unwrap_or(0);
    Some((Usage { cpu_nanocores: cpu, memory_bytes: memory }, tb, dt))
}

/// A node's usage and its pods' from cadvisor's `subcontainers` answer.
pub fn summarize(infos: &[Value]) -> NodeSample {
    let mut out = NodeSample::default();
    let mut newest: Option<chrono::DateTime<chrono::FixedOffset>> = None;
    for info in infos {
        let Some((usage, at, window)) = usage_of(info) else { continue };
        if info["name"] == "/" {
            out.node = Some(usage);
            out.window_secs = window;
            newest = Some(newest.map_or(at, |n| n.max(at)));
            continue;
        }
        let labels = &info["spec"]["labels"];
        let (Some(ns), Some(pod), Some(container)) = (
            labels[LABEL_NAMESPACE].as_str(),
            labels[LABEL_POD].as_str(),
            labels[LABEL_CONTAINER].as_str(),
        ) else {
            continue;
        };
        if container.is_empty() || container == "POD" {
            continue;
        }
        if out.window_secs == 0.0 {
            out.window_secs = window;
        }
        newest = Some(newest.map_or(at, |n| n.max(at)));
        out.pods.entry((ns.to_string(), pod.to_string())).or_default().insert(container.to_string(), usage);
    }
    out.timestamp = newest.map(|t| t.with_timezone(&chrono::Utc).format("%Y-%m-%dT%H:%M:%SZ").to_string()).unwrap_or_default();
    out
}

/// Where cadvisor is on each node, and the answers kept.
pub struct ResourceMetrics {
    client: reqwest::Client,
    scheme: String,
    port: u16,
    token_file: Option<PathBuf>,
    cache: Mutex<HashMap<String, (Instant, Arc<NodeSample>)>>,
}

impl ResourceMetrics {
    pub fn new(scheme: &str, port: u16, ca_file: Option<&std::path::Path>, token_file: Option<PathBuf>) -> anyhow::Result<Self> {
        let mut b = reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(5));
        if let Some(ca) = ca_file {
            for cert in reqwest::Certificate::from_pem_bundle(&std::fs::read(ca)?)? {
                b = b.add_root_certificate(cert);
            }
        }
        Ok(Self { client: b.build()?, scheme: scheme.into(), port, token_file, cache: Mutex::new(HashMap::new()) })
    }

    fn address(node: &Value) -> Option<String> {
        let addrs = node["status"]["addresses"].as_array()?;
        addrs
            .iter()
            .find(|a| a["type"] == "InternalIP")
            .or_else(|| addrs.iter().find(|a| a["type"] == "Hostname"))
            .and_then(|a| a["address"].as_str())
            .map(str::to_string)
    }

    /// This node's sample, from the cache or its cadvisor.
    pub async fn sample(&self, node: &Value) -> Result<Arc<NodeSample>, String> {
        let name = node["metadata"]["name"].as_str().unwrap_or("").to_string();
        if let Some((at, s)) = self.cache.lock().unwrap().get(&name) {
            if at.elapsed() < CACHE {
                return Ok(s.clone());
            }
        }
        let addr = Self::address(node).ok_or_else(|| format!("node {name} has no address"))?;
        let host = if addr.contains(':') { format!("[{addr}]") } else { addr };
        let mut req = self.client.get(format!("{}://{host}:{}/api/v1.3/subcontainers/", self.scheme, self.port));
        if let Some(t) = self.token_file.as_ref().and_then(|f| std::fs::read_to_string(f).ok()) {
            req = req.bearer_auth(t.trim());
        }
        let resp = req.send().await.map_err(|e| format!("cadvisor on {name}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("cadvisor on {name}: {}", resp.status()));
        }
        let infos: Vec<Value> = resp.json().await.map_err(|e| format!("cadvisor on {name}: {e}"))?;
        let sample = Arc::new(summarize(&infos));
        self.cache.lock().unwrap().insert(name, (Instant::now(), sample.clone()));
        Ok(sample)
    }
}

fn query_param(query: &Option<String>, key: &str) -> Option<String> {
    form_urlencoded::parse(query.as_deref().unwrap_or("").as_bytes())
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

fn matches_labels(obj: &Value, selector: &Option<String>) -> bool {
    let Some(sel) = selector.as_deref().filter(|s| !s.is_empty()) else { return true };
    let reqs = crate::selector::parse_label_selector(sel);
    let empty = serde_json::Map::new();
    crate::selector::matches_label_selector(obj["metadata"]["labels"].as_object().unwrap_or(&empty), &reqs)
}

async fn all(storage: &ResourceStorage, prefix: &str) -> Result<Vec<Value>, ApiError> {
    let mut out = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let (items, next, _) = storage.list(prefix, 500, token.as_deref()).await?;
        out.extend(items);
        match next {
            Some(t) => token = Some(t),
            None => return Ok(out),
        }
    }
}

fn node_metrics(node: &Value, s: &NodeSample) -> Option<Value> {
    let usage = s.node?;
    Some(json!({
        "metadata": {"name": node["metadata"]["name"], "labels": node["metadata"]["labels"],
                     "creationTimestamp": node["metadata"]["creationTimestamp"]},
        "timestamp": s.timestamp, "window": format!("{}s", s.window_secs.round() as u64),
        "usage": usage.json(),
    }))
}

fn pod_metrics(pod: &Value, s: &NodeSample) -> Option<Value> {
    let key = (pod["metadata"]["namespace"].as_str()?.to_string(), pod["metadata"]["name"].as_str()?.to_string());
    let containers = s.pods.get(&key)?;
    Some(json!({
        "metadata": {"name": key.1, "namespace": key.0, "labels": pod["metadata"]["labels"],
                     "creationTimestamp": pod["metadata"]["creationTimestamp"]},
        "timestamp": s.timestamp, "window": format!("{}s", s.window_secs.round() as u64),
        "containers": containers.iter().map(|(name, u)| json!({"name": name, "usage": u.json()})).collect::<Vec<_>>(),
    }))
}

fn list(kind: &str, items: Vec<Value>) -> Response {
    let items: Vec<Value> = items
        .into_iter()
        .map(|mut i| {
            i["kind"] = json!(kind.trim_end_matches("List"));
            i["apiVersion"] = json!("metrics.k8s.io/v1beta1");
            i
        })
        .collect();
    Json(json!({"kind": kind, "apiVersion": "metrics.k8s.io/v1beta1", "metadata": {}, "items": items})).into_response()
}

fn one(kind: &str, mut item: Value) -> Response {
    item["kind"] = json!(kind);
    item["apiVersion"] = json!("metrics.k8s.io/v1beta1");
    Json(item).into_response()
}

/// Samples for every node in `names`, at once; a node whose cadvisor does
/// not answer is left out (and logged), as metrics-server leaves it out.
async fn samples(state: &AppState, nodes: &[Value]) -> HashMap<String, Arc<NodeSample>> {
    let futs = nodes.iter().map(|n| async move {
        let name = n["metadata"]["name"].as_str().unwrap_or("").to_string();
        (name, state.resource_metrics.sample(n).await)
    });
    futures::future::join_all(futs)
        .await
        .into_iter()
        .filter_map(|(name, r)| match r {
            Ok(s) => Some((name, s)),
            Err(e) => {
                tracing::debug!("metrics.k8s.io: {e}");
                None
            }
        })
        .collect()
}

/// GET /apis/metrics.k8s.io/v1beta1
pub async fn resources() -> impl IntoResponse {
    Json(json!({
        "kind": "APIResourceList", "apiVersion": "v1", "groupVersion": "metrics.k8s.io/v1beta1",
        "resources": [
            {"name": "nodes", "singularName": "", "namespaced": false, "kind": "NodeMetrics", "verbs": ["get", "list"]},
            {"name": "pods", "singularName": "", "namespaced": true, "kind": "PodMetrics", "verbs": ["get", "list"]},
        ]
    }))
}

/// GET /apis/metrics.k8s.io/v1beta1/nodes
pub async fn list_nodes(State(state): State<AppState>, RawQuery(q): RawQuery) -> Result<Response, ApiError> {
    let sel = query_param(&q, "labelSelector");
    let nodes: Vec<Value> = all(&state.storage, &ResourceStorage::cluster_prefix("nodes"))
        .await?
        .into_iter()
        .filter(|n| matches_labels(n, &sel))
        .collect();
    let s = samples(&state, &nodes).await;
    let items = nodes
        .iter()
        .filter_map(|n| node_metrics(n, s.get(n["metadata"]["name"].as_str()?)?))
        .collect();
    Ok(list("NodeMetricsList", items))
}

/// GET /apis/metrics.k8s.io/v1beta1/nodes/{name}
pub async fn get_node(State(state): State<AppState>, Path(name): Path<String>) -> Result<Response, ApiError> {
    let node = state.storage.get(&ResourceStorage::cluster_key("nodes", &name)).await?;
    let s = samples(&state, std::slice::from_ref(&node)).await;
    match s.get(&name).and_then(|s| node_metrics(&node, s)) {
        Some(m) => Ok(one("NodeMetrics", m)),
        None => Err(ApiError::not_found("nodemetrics.metrics.k8s.io", &name)),
    }
}

async fn pods_in(state: &AppState, namespace: Option<&str>, q: &Option<String>) -> Result<Vec<Value>, ApiError> {
    let prefix = match namespace {
        Some(ns) => ResourceStorage::namespace_prefix("pods", ns),
        None => ResourceStorage::all_namespaces_prefix("pods"),
    };
    let sel = query_param(q, "labelSelector");
    let pods: Vec<Value> = all(&state.storage, &prefix)
        .await?
        .into_iter()
        .filter(|p| matches_labels(p, &sel) && p["spec"]["nodeName"].as_str().is_some_and(|n| !n.is_empty()))
        .collect();
    let mut wanted: Vec<String> = pods.iter().filter_map(|p| p["spec"]["nodeName"].as_str().map(str::to_string)).collect();
    wanted.sort();
    wanted.dedup();
    let mut nodes = Vec::new();
    for n in wanted {
        if let Ok(node) = state.storage.get(&ResourceStorage::cluster_key("nodes", &n)).await {
            nodes.push(node);
        }
    }
    let s = samples(state, &nodes).await;
    Ok(pods
        .iter()
        .filter_map(|p| pod_metrics(p, s.get(p["spec"]["nodeName"].as_str()?)?))
        .collect())
}

/// GET /apis/metrics.k8s.io/v1beta1/pods
pub async fn list_pods_all(State(state): State<AppState>, RawQuery(q): RawQuery) -> Result<Response, ApiError> {
    Ok(list("PodMetricsList", pods_in(&state, None, &q).await?))
}

/// GET /apis/metrics.k8s.io/v1beta1/namespaces/{ns}/pods
pub async fn list_pods(State(state): State<AppState>, Path(ns): Path<String>, RawQuery(q): RawQuery) -> Result<Response, ApiError> {
    Ok(list("PodMetricsList", pods_in(&state, Some(&ns), &q).await?))
}

/// GET /apis/metrics.k8s.io/v1beta1/namespaces/{ns}/pods/{name}
pub async fn get_pod(State(state): State<AppState>, Path((ns, name)): Path<(String, String)>) -> Result<Response, ApiError> {
    let pod = state.storage.get(&ResourceStorage::namespaced_key("pods", &ns, &name)).await?;
    let node = pod["spec"]["nodeName"].as_str().unwrap_or("").to_string();
    let found = match state.storage.get(&ResourceStorage::cluster_key("nodes", &node)).await {
        Ok(n) => samples(&state, std::slice::from_ref(&n)).await.get(&node).and_then(|s| pod_metrics(&pod, s)),
        Err(_) => None,
    };
    match found {
        Some(m) => Ok(one("PodMetrics", m)),
        None => Err(ApiError::not_found("podmetrics.metrics.k8s.io", &name)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cg(name: &str, labels: Value, cpu: (u64, u64), mem: u64) -> Value {
        json!({"name": name, "spec": {"labels": labels}, "stats": [
            {"timestamp": "2026-10-07T10:00:00Z", "cpu": {"usage": {"total": cpu.0}}, "memory": {"working_set": mem / 2}},
            {"timestamp": "2026-10-07T10:00:10Z", "cpu": {"usage": {"total": cpu.1}}, "memory": {"working_set": mem}},
        ]})
    }
    fn pod_labels(ns: &str, pod: &str, c: &str) -> Value {
        json!({LABEL_NAMESPACE: ns, LABEL_POD: pod, LABEL_CONTAINER: c})
    }

    #[test]
    fn a_node_and_its_pods_from_cadvisor() {
        let infos = vec![
            // The root: 2 cores busy for 10 s.
            cg("/", json!({}), (0, 20_000_000_000), 8 << 30),
            cg("/kubepods/a/c1", pod_labels("web", "a", "app"), (1_000_000_000, 6_000_000_000), 100 << 20),
            cg("/kubepods/a/c2", pod_labels("web", "a", "sidecar"), (0, 1_000_000_000), 10 << 20),
            cg("/kubepods/a/sandbox", pod_labels("web", "a", "POD"), (0, 1_000_000_000), 1 << 20),
            // A stormpump pod today: no runtime labels, not attributed.
            cg("/stormpump/w123-1", json!({}), (0, 9_000_000_000), 1 << 20),
        ];
        let s = summarize(&infos);
        assert_eq!(s.node, Some(Usage { cpu_nanocores: 2_000_000_000, memory_bytes: 8 << 30 }));
        assert_eq!(s.timestamp, "2026-10-07T10:00:10Z");
        assert_eq!(s.window_secs, 10.0);
        let a = &s.pods[&("web".to_string(), "a".to_string())];
        assert_eq!(a.len(), 2, "the sandbox is not a container");
        assert_eq!(a["app"], Usage { cpu_nanocores: 500_000_000, memory_bytes: 100 << 20 });
        assert_eq!(s.pods.len(), 1, "an unlabelled cgroup is nobody's");
        let m = pod_metrics(&json!({"metadata": {"name": "a", "namespace": "web"}}), &s).unwrap();
        assert_eq!(m["containers"][0]["usage"]["cpu"], "500000000n");
        assert_eq!(m["containers"][0]["usage"]["memory"], "102400Ki");
        assert_eq!(m["window"], "10s");
    }

    #[test]
    fn one_sample_is_no_rate() {
        let one = json!({"name": "/", "stats": [{"timestamp": "2026-10-07T10:00:00Z", "cpu": {"usage": {"total": 5}}, "memory": {"working_set": 1}}]});
        assert_eq!(summarize(&[one]).node, None);
    }

    #[test]
    fn a_node_is_reached_at_its_internal_ip() {
        let n = json!({"status": {"addresses": [{"type": "Hostname", "address": "n1"}, {"type": "InternalIP", "address": "10.0.0.5"}]}});
        assert_eq!(ResourceMetrics::address(&n).as_deref(), Some("10.0.0.5"));
        let h = json!({"status": {"addresses": [{"type": "Hostname", "address": "n1"}]}});
        assert_eq!(ResourceMetrics::address(&h).as_deref(), Some("n1"));
    }
}
